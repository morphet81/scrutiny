#!/usr/bin/env bash
# Resolve the scrutiny binary. stdout = absolute path only. Progress on stderr.
# Works from repo root scripts/ or skills/*/scripts/ (walks up for Cargo.toml).
# Default: download GitHub Release *latest* asset. Pin only via SCRUTINY_VERSION.
# Cached bin is reused only when .scrutiny-version matches desired tag.
set -euo pipefail

log() { printf '%s\n' "$*" >&2; }
die() { log "scrutiny ensure-bin: $*"; exit 1; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# Prefer git/cargo checkout root when scripts live under skills/<name>/scripts/
REPO_ROOT=""
cur="${INSTALL_ROOT}"
for _ in 1 2 3 4 5 6 7 8; do
  if [[ -f "${cur}/Cargo.toml" ]]; then
    REPO_ROOT="${cur}"
    break
  fi
  parent="$(cd "${cur}/.." && pwd)"
  [[ "${parent}" == "${cur}" ]] && break
  cur="${parent}"
done

SKILL_ROOT="${REPO_ROOT:-$INSTALL_ROOT}"

BIN_DIR="${SKILL_ROOT}/bin"
# When skill is installed without Cargo.toml, cache under the skill install dir
if [[ -z "${REPO_ROOT}" ]]; then
  BIN_DIR="${INSTALL_ROOT}/bin"
fi

TARGET_BIN="${SKILL_ROOT}/target/release/scrutiny"
TARGET_BIN_EXE="${SKILL_ROOT}/target/release/scrutiny.exe"
CACHED_BIN="${BIN_DIR}/scrutiny"
CACHED_BIN_EXE="${BIN_DIR}/scrutiny.exe"
VERSION_STAMP="${BIN_DIR}/.scrutiny-version"

is_executable() {
  [[ -f "$1" && -x "$1" ]]
}

emit() {
  # Absolute path only on stdout
  local p
  p="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
  printf '%s\n' "$p"
  exit 0
}

detect_triple() {
  local os arch
  os="$(uname -s | tr '[:upper:]' '[:lower:]')"
  arch="$(uname -m)"
  case "${os}" in
    darwin)
      case "${arch}" in
        arm64|aarch64) echo "aarch64-apple-darwin" ;;
        x86_64)
          echo "x86_64-apple-darwin"
          ;;
        *) die "unsupported macOS arch: ${arch}" ;;
      esac
      ;;
    linux)
      case "${arch}" in
        x86_64|amd64) echo "x86_64-unknown-linux-gnu" ;;
        aarch64|arm64) echo "aarch64-unknown-linux-gnu" ;;
        *) die "unsupported Linux arch: ${arch}" ;;
      esac
      ;;
    mingw*|msys*|cygwin*)
      echo "x86_64-pc-windows-msvc"
      ;;
    *)
      if [[ "${OS:-}" == "Windows_NT" ]]; then
        echo "x86_64-pc-windows-msvc"
      else
        die "unsupported OS: ${os}"
      fi
      ;;
  esac
}

github_repo() {
  if [[ -n "${SCRUTINY_GITHUB_REPO:-}" ]]; then
    printf '%s\n' "${SCRUTINY_GITHUB_REPO}"
    return
  fi
  if command -v git >/dev/null 2>&1 && [[ -d "${SKILL_ROOT}/.git" ]]; then
    local url
    url="$(git -C "${SKILL_ROOT}" remote get-url origin 2>/dev/null || true)"
    if [[ "${url}" =~ github\.com[:/]([^/]+)/([^/.]+)(\.git)?$ ]]; then
      printf '%s/%s\n' "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}"
      return
    fi
  fi
  printf '%s\n' "morphet81/scrutiny"
}

# Resolve desired release version (no leading v).
desired_version() {
  if [[ -n "${SCRUTINY_VERSION:-}" ]]; then
    printf '%s\n' "${SCRUTINY_VERSION#v}"
    return
  fi
  local repo tag
  repo="$(github_repo)"
  tag="$(fetch_latest_tag "${repo}" || true)"
  if [[ -n "${tag}" ]]; then
    printf '%s\n' "${tag#v}"
    return
  fi
  die "could not resolve latest release tag for ${repo} (set SCRUTINY_VERSION or check network)"
}

fetch_latest_tag() {
  local repo="$1" api body
  api="https://api.github.com/repos/${repo}/releases/latest"
  if command -v curl >/dev/null 2>&1; then
    body="$(curl -fsSL --connect-timeout 10 --max-time 30 "${api}" 2>/dev/null || true)"
  elif command -v wget >/dev/null 2>&1; then
    body="$(wget -q -O - "${api}" 2>/dev/null || true)"
  else
    return 1
  fi
  [[ -n "${body}" ]] || return 1
  # Prefer python/jq; fallback to sed
  if command -v jq >/dev/null 2>&1; then
    printf '%s\n' "$(printf '%s' "${body}" | jq -r '.tag_name // empty')"
    return 0
  fi
  printf '%s' "${body}" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1
}

cached_version() {
  if [[ -f "${VERSION_STAMP}" ]]; then
    tr -d '[:space:]' < "${VERSION_STAMP}"
  fi
}

write_version_stamp() {
  local ver="$1"
  mkdir -p "${BIN_DIR}"
  printf '%s\n' "${ver}" > "${VERSION_STAMP}"
}

# Download URL for a concrete version.
release_asset_url() {
  local repo="$1" ver="$2" asset="$3"
  printf '%s\n' "https://github.com/${repo}/releases/download/v${ver#v}/${asset}"
}

try_emit_matching_cache() {
  local want="$1"
  local have
  have="$(cached_version || true)"
  if [[ "${have}" == "${want}" ]]; then
    if is_executable "${CACHED_BIN}"; then
      log "scrutiny ensure-bin: cache hit v${want} → ${CACHED_BIN}"
      emit "${CACHED_BIN}"
    fi
    if is_executable "${CACHED_BIN_EXE}"; then
      log "scrutiny ensure-bin: cache hit v${want} → ${CACHED_BIN_EXE}"
      emit "${CACHED_BIN_EXE}"
    fi
  elif [[ -n "${have}" ]]; then
    log "scrutiny ensure-bin: cache is v${have}, want v${want} — refreshing"
  fi
}

try_download() {
  local repo ver triple asset url dest tmp
  repo="$(github_repo)"
  ver="$(desired_version)"
  triple="$(detect_triple)"

  if [[ "${triple}" == "x86_64-apple-darwin" ]]; then
    log "scrutiny ensure-bin: no release binary for Intel Mac; falling back to cargo"
    return 1
  fi

  try_emit_matching_cache "${ver}"

  if [[ "${triple}" == *windows* ]]; then
    asset="scrutiny-${triple}.exe"
    dest="${CACHED_BIN_EXE}"
  else
    asset="scrutiny-${triple}"
    dest="${CACHED_BIN}"
  fi

  url="$(release_asset_url "${repo}" "${ver}" "${asset}")"
  mkdir -p "${BIN_DIR}"
  tmp="${dest}.tmp"

  log "scrutiny ensure-bin: downloading v${ver} ← ${url}"

  if command -v curl >/dev/null 2>&1; then
    if ! curl -fsSL --connect-timeout 10 --max-time 120 -L -o "${tmp}" "${url}"; then
      rm -f "${tmp}"
      return 1
    fi
  elif command -v wget >/dev/null 2>&1; then
    if ! wget -q -O "${tmp}" "${url}"; then
      rm -f "${tmp}"
      return 1
    fi
  else
    log "scrutiny ensure-bin: neither curl nor wget available"
    return 1
  fi

  local size
  size="$(wc -c < "${tmp}" | tr -d ' ')"
  if [[ "${size}" -lt 1000 ]]; then
    log "scrutiny ensure-bin: download too small (${size} bytes); treating as miss"
    rm -f "${tmp}"
    return 1
  fi

  mv "${tmp}" "${dest}"
  chmod +x "${dest}" 2>/dev/null || true
  write_version_stamp "${ver}"
  log "scrutiny ensure-bin: installed v${ver} → ${dest}"
  emit "${dest}"
}

try_cargo_build() {
  command -v cargo >/dev/null 2>&1 || return 1
  [[ -f "${SKILL_ROOT}/Cargo.toml" ]] || return 1
  log "scrutiny ensure-bin: building with cargo (release) from ${SKILL_ROOT}"
  (
    cd "${SKILL_ROOT}"
    cargo build --release --manifest-path "${SKILL_ROOT}/Cargo.toml"
  )
  if is_executable "${TARGET_BIN}"; then
    emit "${TARGET_BIN}"
  fi
  if is_executable "${TARGET_BIN_EXE}"; then
    emit "${TARGET_BIN_EXE}"
  fi
  return 1
}

# Local cargo checkout: use target/release only when SCRUTINY_USE_LOCAL=1
# or when download fails. Prefer release download so installed+dev stay current.
if [[ "${SCRUTINY_USE_LOCAL:-}" == "1" ]]; then
  if is_executable "${TARGET_BIN}"; then
    log "scrutiny ensure-bin: SCRUTINY_USE_LOCAL=1 → ${TARGET_BIN}"
    emit "${TARGET_BIN}"
  fi
  if is_executable "${TARGET_BIN_EXE}"; then
    log "scrutiny ensure-bin: SCRUTINY_USE_LOCAL=1 → ${TARGET_BIN_EXE}"
    emit "${TARGET_BIN_EXE}"
  fi
fi

if try_download; then
  :
fi

if try_cargo_build; then
  :
fi

die "no binary found.
Install a Rust toolchain (https://rustup.rs) so cargo can build, or publish/download a release for this platform.
Repo: $(github_repo)  Want: v$(desired_version 2>/dev/null || echo '?')  Triple: $(detect_triple)
Default fetches GitHub Release latest (cache keyed by .scrutiny-version).
Pin with SCRUTINY_VERSION. Force local build with SCRUTINY_USE_LOCAL=1.
Override repo with SCRUTINY_GITHUB_REPO."
