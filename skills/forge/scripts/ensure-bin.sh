#!/usr/bin/env bash
# Resolve the scrutiny binary. stdout = absolute path only. Progress on stderr.
# Works from repo root scripts/ or skills/*/scripts/ (walks up for Cargo.toml).
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

# Prefer already-built / cached binaries
if is_executable "${CACHED_BIN}"; then
  emit "${CACHED_BIN}"
fi
if is_executable "${CACHED_BIN_EXE}"; then
  emit "${CACHED_BIN_EXE}"
fi
if is_executable "${TARGET_BIN}"; then
  emit "${TARGET_BIN}"
fi
if is_executable "${TARGET_BIN_EXE}"; then
  emit "${TARGET_BIN_EXE}"
fi

detect_triple() {
  local os arch
  os="$(uname -s | tr '[:upper:]' '[:lower:]')"
  arch="$(uname -m)"
  case "${os}" in
    darwin)
      case "${arch}" in
        arm64|aarch64) echo "aarch64-apple-darwin" ;;
        x86_64)
          # No release asset (Intel runner retired). Cargo fallback builds host.
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
      # Windows Git Bash sometimes reports MINGW64_NT-...
      if [[ "${OS:-}" == "Windows_NT" ]]; then
        echo "x86_64-pc-windows-msvc"
      else
        die "unsupported OS: ${os}"
      fi
      ;;
  esac
}

read_version() {
  if [[ -n "${SCRUTINY_VERSION:-}" ]]; then
    printf '%s\n' "${SCRUTINY_VERSION#v}"
    return
  fi
  local cargo_toml="${SKILL_ROOT}/Cargo.toml"
  if [[ ! -f "${cargo_toml}" ]]; then
    # Installed skill without sources — use latest release tag via env or fixed fallback
    printf '%s\n' "${SCRUTINY_VERSION_FALLBACK:-0.1.1}"
    return
  fi
  local ver
  ver="$(
    awk '
      /^\[workspace\.package\]/ { in_ws=1; next }
      /^\[/ { in_ws=0 }
      in_ws && /^version[[:space:]]*=/ {
        gsub(/"/, "", $3);
        print $3;
        exit
      }
    ' "${cargo_toml}"
  )"
  if [[ -z "${ver}" ]]; then
    ver="$(
      awk '
        /^\[package\]/ { in_pkg=1; next }
        /^\[/ { in_pkg=0 }
        in_pkg && /^version[[:space:]]*=/ {
          gsub(/"/, "", $3);
          print $3;
          exit
        }
      ' "${cargo_toml}"
    )"
  fi
  [[ -n "${ver}" ]] || die "could not read version from Cargo.toml"
  printf '%s\n' "${ver}"
}

github_repo() {
  if [[ -n "${SCRUTINY_GITHUB_REPO:-}" ]]; then
    printf '%s\n' "${SCRUTINY_GITHUB_REPO}"
    return
  fi
  # Try git remote if this install is still a git checkout
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

try_download() {
  local repo version triple asset url dest tmp
  repo="$(github_repo)"
  version="$(read_version)"
  triple="$(detect_triple)"

  # Intel macOS: no GitHub Release asset (runner retired). Skip to cargo.
  if [[ "${triple}" == "x86_64-apple-darwin" ]]; then
    log "scrutiny ensure-bin: no release binary for Intel Mac; falling back to cargo"
    return 1
  fi

  if [[ "${triple}" == *windows* ]]; then
    asset="scrutiny-${triple}.exe"
    dest="${CACHED_BIN_EXE}"
  else
    asset="scrutiny-${triple}"
    dest="${CACHED_BIN}"
  fi

  url="https://github.com/${repo}/releases/download/v${version}/${asset}"
  mkdir -p "${BIN_DIR}"
  tmp="${dest}.tmp"

  log "scrutiny ensure-bin: trying ${url}"

  if command -v curl >/dev/null 2>&1; then
    if ! curl -fsSL --connect-timeout 10 --max-time 120 -o "${tmp}" "${url}"; then
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

  # Reject tiny/HTML error bodies
  local size
  size="$(wc -c < "${tmp}" | tr -d ' ')"
  if [[ "${size}" -lt 1000 ]]; then
    log "scrutiny ensure-bin: download too small (${size} bytes); treating as miss"
    rm -f "${tmp}"
    return 1
  fi

  mv "${tmp}" "${dest}"
  chmod +x "${dest}" 2>/dev/null || true
  log "scrutiny ensure-bin: installed ${dest}"
  emit "${dest}"
}

try_cargo_build() {
  command -v cargo >/dev/null 2>&1 || return 1
  [[ -f "${SKILL_ROOT}/Cargo.toml" ]] || return 1
  log "scrutiny ensure-bin: building with cargo (release)"
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

# Download first (works without Rust), then cargo fallback
if try_download; then
  :
fi

if try_cargo_build; then
  :
fi

die "no binary found.
Install a Rust toolchain (https://rustup.rs) so cargo can build, or publish/download a release for this platform.
Repo: $(github_repo)  Version: $(read_version)  Triple: $(detect_triple)
Override with SCRUTINY_GITHUB_REPO / SCRUTINY_VERSION if needed."
