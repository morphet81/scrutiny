//! `scrutiny ready` — resolve open review threads, mark the PR ready,
//! nominate reviewers from config, and move the Jira ticket to In Review.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::config::{ensure_config, find_shipped_default, load_config_from};
use crate::forge::fetch::{
    extract_jira_key_from_text, jira_key_from_branch, jira_key_from_url_or_raw,
};
use crate::forge::jira_ops::jira_transition;
use crate::forge::tools::require_acli;
use crate::gh::{
    ensure_gh, gh_graphql, gh_json, gh_output_retry, repo_name_with_owner, split_repo,
};
use crate::paths::{artifact_path, prepare_artifacts, write_json_pretty};

const THREAD_IDS_QUERY: &str = r#"
query($owner: String!, $name: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 50, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes { id isResolved }
      }
    }
  }
}
"#;

const RESOLVE_THREAD: &str = r#"
mutation($id: ID!) {
  resolveReviewThread(input: {threadId: $id}) {
    thread { isResolved }
  }
}
"#;

const ORG_EMAILS_QUERY: &str = r#"
query($org: String!, $cursor: String) {
  organization(login: $org) {
    membersWithRole(first: 100, after: $cursor) {
      pageInfo { hasNextPage endCursor }
      nodes {
        login
        organizationVerifiedDomainEmails(login: $org)
      }
    }
  }
}
"#;

const SEARCH_USERS: &str = r#"
query($q: String!) {
  search(query: $q, type: USER, first: 5) {
    nodes {
      ... on User { login }
    }
  }
}
"#;

#[derive(Debug, Clone)]
pub struct ReadyCmdInput {
    pub cwd: PathBuf,
    /// PR URL or number. None → current branch PR.
    pub pr: Option<String>,
    /// Jira key or URL. None → branch, then PR title, then PR body.
    pub ticket: Option<String>,
}

#[derive(Debug, Serialize)]
struct ReadySummary {
    pr_number: u64,
    pr_url: String,
    resolved_threads: u32,
    reviewers: Vec<String>,
    ticket: Option<String>,
    jira_status: Option<String>,
}

struct PrSnap {
    number: u64,
    url: String,
    title: String,
    body: String,
    author: Option<String>,
}

pub fn run_ready(input: ReadyCmdInput) -> Result<PathBuf> {
    ensure_gh()?;
    let cwd = &input.cwd;
    let shipped = find_shipped_default(cwd);
    let cfg_path = ensure_config(&shipped)?;
    let cfg = load_config_from(&cfg_path, Some(cwd))?;

    let pr = view_pr(cwd, input.pr.as_deref())?;
    prepare_artifacts(cwd, Some(&pr.number.to_string()), &[])?;

    let ticket = ticket_key(cwd, input.ticket.as_deref(), &pr)?;
    let status = transition_status(&cfg.ready.in_review_status);
    if status.is_some() && ticket.is_some() {
        require_acli()?;
    }

    let reviewers = nominate_logins(
        cwd,
        &cfg.ready.reviewers,
        cfg.ready.reviewer_count,
        pr.author.as_deref(),
    )?;

    let resolved = resolve_open_threads(cwd, pr.number)?;
    eprintln!("scrutiny ready: resolved {resolved} review thread(s)");

    mark_pr_ready(cwd, pr.number)?;
    eprintln!("scrutiny ready: PR #{} ready for review", pr.number);

    if !reviewers.is_empty() {
        let num = pr.number.to_string();
        let joined = reviewers.join(",");
        gh_ok(
            cwd,
            &["pr", "edit", &num, "--add-reviewer", &joined],
            "request reviewers",
        )?;
        eprintln!(
            "scrutiny ready: requested review from {}",
            reviewers.join(", ")
        );
    }

    let applied_status = match (status, ticket.as_deref()) {
        (Some(status), Some(key)) => {
            jira_transition(cwd, key, status)?;
            eprintln!("scrutiny ready: {key} → {status}");
            Some(status.to_string())
        }
        (Some(_), None) => {
            eprintln!("scrutiny ready: no Jira key on branch or PR — skip transition");
            None
        }
        (None, _) => {
            eprintln!("scrutiny ready: in_review_status empty — skip transition");
            None
        }
    };

    let summary = ReadySummary {
        pr_number: pr.number,
        pr_url: pr.url,
        resolved_threads: resolved,
        reviewers,
        ticket,
        jira_status: applied_status,
    };
    let out = artifact_path("ready");
    write_json_pretty(&out, &summary)?;
    Ok(out)
}

fn nominate_logins(
    cwd: &Path,
    pool: &[String],
    count: u32,
    author: Option<&str>,
) -> Result<Vec<String>> {
    if !request_enabled(pool, count) {
        if pool.iter().all(|r| r.trim().is_empty()) {
            eprintln!("scrutiny ready: no reviewers in config — skip request");
        } else {
            eprintln!("scrutiny ready: reviewer_count is 0 — skip request");
        }
        return Ok(Vec::new());
    }

    let picked = nominate_reviewers(pool, count);
    let needs_lookup = picked
        .iter()
        .any(|p| p.contains('@') && login_from_noreply(p).is_none());
    let index = if needs_lookup {
        let repo = repo_name_with_owner(cwd)?;
        let (owner, _) = split_repo(&repo)?;
        org_email_index(cwd, &owner)
    } else {
        HashMap::new()
    };

    let mut logins = Vec::new();
    for email in &picked {
        logins.push(resolve_reviewer_login(email, &index, |e| {
            search_users_by_email(cwd, e)
        })?);
    }
    let mut seen = BTreeSet::new();
    logins.retain(|login| seen.insert(login.to_ascii_lowercase()));
    let requested = drop_author(&logins, author);
    if requested.is_empty() {
        eprintln!("scrutiny ready: nominated reviewer is the PR author — skip request");
    } else if requested.len() < logins.len() {
        eprintln!("scrutiny ready: skip PR author");
    }
    Ok(requested)
}

/// Non-empty pool and a positive count.
fn request_enabled(pool: &[String], count: u32) -> bool {
    count > 0 && pool.iter().any(|r| !r.trim().is_empty())
}

/// Trimmed status, or `None` when the config leaves it blank.
fn transition_status(status: &str) -> Option<&str> {
    let status = status.trim();
    if status.is_empty() {
        None
    } else {
        Some(status)
    }
}

/// Pick `count` entries from `pool` without replacement.
/// Duplicates (case-insensitive, surrounding space ignored) collapse first.
/// When `count` is at least the unique pool size, every unique entry is returned.
pub fn nominate_reviewers(pool: &[String], count: u32) -> Vec<String> {
    nominate_reviewers_with(pool, count, random_below)
}

fn nominate_reviewers_with(
    pool: &[String],
    count: u32,
    mut rng: impl FnMut(usize) -> usize,
) -> Vec<String> {
    let mut unique = Vec::new();
    let mut seen = BTreeSet::new();
    for raw in pool {
        let email = raw.trim();
        if email.is_empty() {
            continue;
        }
        if seen.insert(email.to_ascii_lowercase()) {
            unique.push(email.to_string());
        }
    }
    if unique.is_empty() || count == 0 {
        return Vec::new();
    }
    let n = (count as usize).min(unique.len());
    for i in 0..n {
        let span = unique.len() - i;
        let j = i + rng(span);
        unique.swap(i, j);
    }
    unique.truncate(n);
    unique
}

fn random_below(n: usize) -> usize {
    if n <= 1 {
        return 0;
    }
    let mut buf = [0u8; 8];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
        .is_ok()
    {
        return (u64::from_ne_bytes(buf) as usize) % n;
    }
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_usize(n);
    (hasher.finish() as usize) % n
}

fn drop_author(logins: &[String], author: Option<&str>) -> Vec<String> {
    match author.map(str::trim).filter(|s| !s.is_empty()) {
        Some(author) => logins
            .iter()
            .filter(|login| !login.eq_ignore_ascii_case(author))
            .cloned()
            .collect(),
        None => logins.to_vec(),
    }
}

/// Map one config entry to a GitHub login.
/// A value with no `@` is already a login. `*@users.noreply.github.com` yields
/// the login embedded in the local part. Other emails use the org verified-domain
/// index, then GitHub user search.
fn resolve_reviewer_login(
    raw: &str,
    org_index: &HashMap<String, String>,
    mut search: impl FnMut(&str) -> Result<Vec<String>>,
) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("empty reviewer");
    }
    if !raw.contains('@') {
        return Ok(raw.to_string());
    }
    if let Some(login) = login_from_noreply(raw) {
        return Ok(login);
    }
    let key = raw.to_ascii_lowercase();
    if let Some(login) = org_index.get(&key) {
        return Ok(login.clone());
    }
    let hits = search(&key)?;
    match hits.len() {
        1 => Ok(hits.into_iter().next().expect("one hit")),
        0 => bail!(
            "could not map {raw} to a GitHub login. Put a login in [ready].reviewers, or an email on the repo org's verified domain"
        ),
        _ => bail!(
            "ambiguous GitHub user for {raw}: {}",
            hits.join(", ")
        ),
    }
}

/// `login@users.noreply.github.com` or `12345+login@users.noreply.github.com`.
fn login_from_noreply(email: &str) -> Option<String> {
    let (local, host) = email.rsplit_once('@')?;
    if !host.eq_ignore_ascii_case("users.noreply.github.com") {
        return None;
    }
    let login = local
        .rsplit_once('+')
        .map(|(_, rest)| rest)
        .unwrap_or(local);
    let login = login.trim();
    if login.is_empty() || !login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    Some(login.to_string())
}

fn org_email_index(cwd: &Path, owner: &str) -> HashMap<String, String> {
    let mut index = HashMap::new();
    let mut cursor: Option<String> = None;
    for _ in 0..40 {
        let vars = json!({ "org": owner, "cursor": cursor });
        let data = match gh_graphql(cwd, ORG_EMAILS_QUERY, &vars) {
            Ok(data) => data,
            Err(_) => break,
        };
        match data.get("organization") {
            None | Some(Value::Null) => break,
            Some(_) => {}
        }
        let next = match absorb_org_members(&mut index, &data) {
            Ok(next) => next,
            Err(_) => break,
        };
        match next {
            Some(next) if cursor.as_deref() != Some(next.as_str()) => cursor = Some(next),
            _ => break,
        }
    }
    index
}

fn absorb_org_members(index: &mut HashMap<String, String>, data: &Value) -> Result<Option<String>> {
    let conn = data
        .pointer("/organization/membersWithRole")
        .context("membersWithRole missing")?;
    let nodes = conn
        .get("nodes")
        .and_then(|n| n.as_array())
        .context("membersWithRole nodes missing")?;
    for node in nodes {
        let Some(login) = node.get("login").and_then(|v| v.as_str()) else {
            continue;
        };
        if login.is_empty() {
            continue;
        }
        let Some(emails) = node
            .get("organizationVerifiedDomainEmails")
            .and_then(|v| v.as_array())
        else {
            continue;
        };
        for email in emails {
            let Some(email) = email.as_str() else {
                continue;
            };
            let email = email.trim().to_ascii_lowercase();
            if email.is_empty() {
                continue;
            }
            index.entry(email).or_insert_with(|| login.to_string());
        }
    }
    let has_next = conn
        .pointer("/pageInfo/hasNextPage")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !has_next {
        return Ok(None);
    }
    Ok(conn
        .pointer("/pageInfo/endCursor")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string))
}

fn search_users_by_email(cwd: &Path, email: &str) -> Result<Vec<String>> {
    let data = gh_graphql(
        cwd,
        SEARCH_USERS,
        &json!({ "q": format!("{email} in:email") }),
    )?;
    let nodes = data
        .pointer("/search/nodes")
        .and_then(|n| n.as_array())
        .cloned()
        .unwrap_or_default();
    let mut logins = Vec::new();
    for node in nodes {
        if let Some(login) = node.get("login").and_then(|v| v.as_str()) {
            if !login.is_empty()
                && !logins
                    .iter()
                    .any(|have: &String| have.eq_ignore_ascii_case(login))
            {
                logins.push(login.to_string());
            }
        }
    }
    Ok(logins)
}

fn ticket_key(cwd: &Path, explicit: Option<&str>, pr: &PrSnap) -> Result<Option<String>> {
    if let Some(raw) = explicit {
        return Ok(Some(jira_key_from_url_or_raw(raw)?));
    }
    if let Some(key) = jira_key_from_branch(cwd)? {
        return Ok(Some(key));
    }
    if let Some(key) = extract_jira_key_from_text(&pr.title) {
        return Ok(Some(key));
    }
    Ok(extract_jira_key_from_text(&pr.body))
}

fn view_pr(cwd: &Path, pr: Option<&str>) -> Result<PrSnap> {
    let mut args = vec!["pr".to_string(), "view".to_string()];
    if let Some(pr) = pr {
        args.push(pr.to_string());
    }
    args.push("--json".to_string());
    args.push("number,url,title,body,author".to_string());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let v = gh_json(cwd, &refs)?;
    let number = v
        .get("number")
        .and_then(|n| n.as_u64())
        .context("pr number missing")?;
    Ok(PrSnap {
        number,
        url: v
            .get("url")
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .to_string(),
        title: v
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string(),
        body: v
            .get("body")
            .and_then(|b| b.as_str())
            .unwrap_or("")
            .to_string(),
        author: v
            .pointer("/author/login")
            .and_then(|s| s.as_str())
            .map(str::to_string),
    })
}

fn resolve_open_threads(cwd: &Path, pr_number: u64) -> Result<u32> {
    let ids = unresolved_thread_ids(cwd, pr_number)?;
    for id in &ids {
        let data = gh_graphql(cwd, RESOLVE_THREAD, &json!({ "id": id }))?;
        let resolved = data
            .pointer("/resolveReviewThread/thread/isResolved")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !resolved {
            bail!("resolveReviewThread did not resolve {id}");
        }
    }
    Ok(ids.len() as u32)
}

fn unresolved_thread_ids(cwd: &Path, pr_number: u64) -> Result<Vec<String>> {
    let repo = repo_name_with_owner(cwd)?;
    let (owner, name) = split_repo(&repo)?;
    let mut ids = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let vars = json!({
            "owner": owner,
            "name": name,
            "number": pr_number as i64,
            "cursor": cursor,
        });
        let data = gh_graphql(cwd, THREAD_IDS_QUERY, &vars)?;
        let threads = data
            .pointer("/repository/pullRequest/reviewThreads")
            .cloned()
            .unwrap_or(Value::Null);
        let nodes = threads
            .get("nodes")
            .and_then(|n| n.as_array())
            .cloned()
            .unwrap_or_default();
        for node in nodes {
            if node.get("isResolved").and_then(|v| v.as_bool()) == Some(true) {
                continue;
            }
            if let Some(id) = node.get("id").and_then(|v| v.as_str()) {
                if !id.is_empty() {
                    ids.push(id.to_string());
                }
            }
        }
        let has_next = threads
            .pointer("/pageInfo/hasNextPage")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !has_next {
            break;
        }
        let next = threads
            .pointer("/pageInfo/endCursor")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        if next.is_none() || next == cursor {
            break;
        }
        cursor = next;
    }
    Ok(ids)
}

fn mark_pr_ready(cwd: &Path, pr_number: u64) -> Result<()> {
    let num = pr_number.to_string();
    let output = gh_output_retry(cwd, &["pr", "ready", &num], None)?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stderr} {stdout}").to_ascii_lowercase();
    if combined.contains("already") && combined.contains("ready") {
        return Ok(());
    }
    bail!("gh pr ready failed: {} {}", stderr.trim(), stdout.trim());
}

fn gh_ok(cwd: &Path, args: &[&str], what: &str) -> Result<()> {
    let output = gh_output_retry(cwd, args, None)?;
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "{what} failed: {} {}",
        String::from_utf8_lossy(&output.stderr).trim(),
        String::from_utf8_lossy(&output.stdout).trim()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn nominate_zero_or_empty_returns_nothing() {
        assert!(nominate_reviewers(&["a@ex.com".into()], 0).is_empty());
        assert!(nominate_reviewers(&[], 2).is_empty());
        assert!(nominate_reviewers(&["  ".into()], 1).is_empty());
        assert!(!request_enabled(&[], 1));
        assert!(!request_enabled(&["a@ex.com".into()], 0));
        assert!(request_enabled(&["a@ex.com".into()], 1));
    }

    #[test]
    fn nominate_collapses_dupes_and_takes_all_when_count_covers_pool() {
        let got = nominate_reviewers_with(
            &[" A@Ex.com ".into(), "a@ex.com".into(), "b@ex.com".into()],
            5,
            |_| 0,
        );
        assert_eq!(got, vec!["A@Ex.com", "b@ex.com"]);
    }

    #[test]
    fn nominate_rng_zero_keeps_prefix() {
        let got = nominate_reviewers_with(
            &["a@ex.com".into(), "b@ex.com".into(), "c@ex.com".into()],
            2,
            |_| 0,
        );
        assert_eq!(got, vec!["a@ex.com", "b@ex.com"]);
    }

    #[test]
    fn nominate_rng_end_swaps_from_the_back() {
        let got = nominate_reviewers_with(
            &["a@ex.com".into(), "b@ex.com".into(), "c@ex.com".into()],
            2,
            |span| span - 1,
        );
        assert_eq!(got, vec!["c@ex.com", "a@ex.com"]);
    }

    #[test]
    fn drop_author_is_case_insensitive() {
        let logins = vec!["Alice".into(), "bob".into()];
        assert_eq!(drop_author(&logins, Some("alice")), vec!["bob"]);
        assert_eq!(drop_author(&logins, None), logins);
        assert!(drop_author(&logins, Some("  ")).len() == 2);
    }

    #[test]
    fn blank_status_skips_transition() {
        assert_eq!(transition_status("  In Review  "), Some("In Review"));
        assert_eq!(transition_status("  "), None);
    }

    #[test]
    fn login_resolution_prefers_bare_noreply_and_org_index() {
        let mut index = HashMap::new();
        index.insert("ada@ex.com".into(), "ada".into());
        let search = |_email: &str| -> Result<Vec<String>> { Ok(vec!["searched".into()]) };

        assert_eq!(
            resolve_reviewer_login("octocat", &index, search).unwrap(),
            "octocat"
        );
        assert_eq!(
            resolve_reviewer_login("ada@ex.com", &index, search).unwrap(),
            "ada"
        );
        assert_eq!(
            resolve_reviewer_login("123+linus@users.noreply.github.com", &index, search).unwrap(),
            "linus"
        );
        assert_eq!(
            resolve_reviewer_login("missing@ex.com", &HashMap::new(), search).unwrap(),
            "searched"
        );
        let none = |_email: &str| -> Result<Vec<String>> { Ok(vec![]) };
        assert!(resolve_reviewer_login("missing@ex.com", &HashMap::new(), none).is_err());
        let many = |_email: &str| -> Result<Vec<String>> { Ok(vec!["a".into(), "b".into()]) };
        assert!(resolve_reviewer_login("missing@ex.com", &HashMap::new(), many).is_err());
    }

    #[test]
    fn absorb_org_members_indexes_emails_and_cursor() {
        let data = json!({
            "organization": {
                "membersWithRole": {
                    "pageInfo": { "hasNextPage": true, "endCursor": "CUR" },
                    "nodes": [
                        {
                            "login": "ada",
                            "organizationVerifiedDomainEmails": ["Ada@Ex.com", ""]
                        },
                        { "login": null },
                        {
                            "login": "bea",
                            "organizationVerifiedDomainEmails": ["ada@ex.com", "bea@ex.com"]
                        }
                    ]
                }
            }
        });
        let mut index = HashMap::new();
        let next = absorb_org_members(&mut index, &data).unwrap();
        assert_eq!(next.as_deref(), Some("CUR"));
        assert_eq!(index.get("ada@ex.com").map(String::as_str), Some("ada"));
        assert_eq!(index.get("bea@ex.com").map(String::as_str), Some("bea"));
    }
}
