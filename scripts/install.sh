#!/usr/bin/env bash
set -euo pipefail

readonly GITHUB_REPO="dobesv/luchta"
readonly GITHUB_API_LATEST="https://api.github.com/repos/${GITHUB_REPO}/releases/latest"
readonly DEFAULT_INSTALL_DIR="${HOME}/.luchta/bin"

usage() {
    cat <<'EOF'
Install luchta and bundled workers from GitHub releases.

Usage:
  install.sh [--version X.Y.Z] [--dir PATH]
  install.sh --help

Options:
  --version X.Y.Z   Install specific version. Overrides latest lookup.
  --dir PATH        Install into PATH. Default: $HOME/.luchta/bin
  --help            Show this help.

Environment:
  LUCHTA_VERSION      Version override.
  LUCHTA_INSTALL_DIR  Install dir override.
  GITHUB_TOKEN        Optional GitHub token for API requests.

Notes:
  - Installs all bundled binaries into one dedicated bin dir.
  - Does not modify shell rc files automatically.
  - Windows users should use scripts/install.ps1.
EOF
}

err() {
    printf 'Error: %s\n' "$*" >&2
    exit 1
}

warn() {
    printf 'Warning: %s\n' "$*" >&2
}

have_cmd() {
    command -v "$1" >/dev/null 2>&1
}

cleanup() {
    if [ -n "${TMP_DIR:-}" ] && [ -d "$TMP_DIR" ]; then
        rm -rf "$TMP_DIR"
    fi
}

make_tmpdir() {
    if have_cmd mktemp; then
        mktemp -d 2>/dev/null && return 0
        mktemp -d -t luchta-install 2>/dev/null && return 0
    fi
    err "mktemp is required"
}

parse_args() {
    VERSION="${LUCHTA_VERSION:-}"
    INSTALL_DIR="${LUCHTA_INSTALL_DIR:-$DEFAULT_INSTALL_DIR}"

    while [ "$#" -gt 0 ]; do
        case "$1" in
            --version)
                [ "$#" -ge 2 ] || err "--version requires a value"
                VERSION="$2"
                shift 2
                ;;
            --dir)
                [ "$#" -ge 2 ] || err "--dir requires a value"
                INSTALL_DIR="$2"
                shift 2
                ;;
            --help|-h)
                usage
                exit 0
                ;;
            *)
                err "unknown argument: $1"
                ;;
        esac
    done
}

api_headers() {
    if [ -n "${GITHUB_TOKEN:-}" ]; then
        printf 'Authorization: Bearer %s\n' "$GITHUB_TOKEN"
    fi
}

fetch_url() {
    url="$1"
    out="$2"

    if have_cmd curl; then
        if [ -n "${GITHUB_TOKEN:-}" ]; then
            curl -fsSL -H "Authorization: Bearer ${GITHUB_TOKEN}" "$url" -o "$out"
        else
            curl -fsSL "$url" -o "$out"
        fi
        return 0
    fi

    if have_cmd wget; then
        if [ -n "${GITHUB_TOKEN:-}" ]; then
            wget --header="Authorization: Bearer ${GITHUB_TOKEN}" -qO "$out" "$url"
        else
            wget -qO "$out" "$url"
        fi
        return 0
    fi

    err "curl or wget is required"
}

fetch_text() {
    url="$1"

    if have_cmd curl; then
        if [ -n "${GITHUB_TOKEN:-}" ]; then
            curl -fsSL -H "Authorization: Bearer ${GITHUB_TOKEN}" "$url"
        else
            curl -fsSL "$url"
        fi
        return 0
    fi

    if have_cmd wget; then
        if [ -n "${GITHUB_TOKEN:-}" ]; then
            wget --header="Authorization: Bearer ${GITHUB_TOKEN}" -qO- "$url"
        else
            wget -qO- "$url"
        fi
        return 0
    fi

    err "curl or wget is required"
}

extract_tag_name() {
    body="$1"
    printf '%s' "$body" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1
}

normalize_version() {
    version_input="$1"
    case "$version_input" in
        v*) printf '%s\n' "${version_input#v}" ;;
        *) printf '%s\n' "$version_input" ;;
    esac
}

resolve_version() {
    if [ -n "$VERSION" ]; then
        RESOLVED_VERSION="$(normalize_version "$VERSION")"
        return 0
    fi

    release_json="$(fetch_text "$GITHUB_API_LATEST")" || err "failed to query GitHub latest release API"
    tag_name="$(extract_tag_name "$release_json")"
    [ -n "$tag_name" ] || err "could not parse tag_name from GitHub API response"

    case "$tag_name" in
        luchta/v*) RESOLVED_VERSION="$(normalize_version "${tag_name#luchta/v}")" ;;
        *) err "unexpected latest release tag format: $tag_name" ;;
    esac
}

normalize_os() {
    os_name="$1"
    case "$os_name" in
        Linux) printf 'linux\n' ;;
        Darwin) printf 'darwin\n' ;;
        MINGW*|MSYS*|CYGWIN*|Windows_NT) printf 'windows\n' ;;
        *) err "unsupported operating system: $os_name" ;;
    esac
}

normalize_arch() {
    arch_name="$1"
    case "$arch_name" in
        x86_64|amd64) printf 'x86_64\n' ;;
        aarch64|arm64) printf 'aarch64\n' ;;
        i386|i486|i586|i686) printf 'i686\n' ;;
        *) err "unsupported architecture: $arch_name" ;;
    esac
}

triple_for() {
    os="$1"
    arch="$2"

    case "$os:$arch" in
        linux:x86_64) printf 'x86_64-unknown-linux-musl\n' ;;
        linux:aarch64) printf 'aarch64-unknown-linux-musl\n' ;;
        darwin:x86_64) printf 'x86_64-apple-darwin\n' ;;
        darwin:aarch64) printf 'aarch64-apple-darwin\n' ;;
        windows:*) err "Windows detected. Use scripts/install.ps1 instead." ;;
        linux:i686) err "32-bit Linux is not supported. No i686 Linux release is published." ;;
        *) err "unsupported platform combination: $os/$arch" ;;
    esac
}

resolve_triple() {
    os="$(normalize_os "$(uname -s)")"
    arch="$(normalize_arch "$(uname -m)")"
    TARGET_TRIPLE="$(triple_for "$os" "$arch")"
}

archive_url_for() {
    version="$1"
    triple="$2"
    printf 'https://github.com/%s/releases/download/luchta/v%s/luchta-v%s-%s.tar.gz\n' "$GITHUB_REPO" "$version" "$version" "$triple"
}

ensure_extract_tools() {
    have_cmd tar || err "tar is required"
}

extract_archive() {
    archive_path="$1"
    install_dir="$2"

    mkdir -p "$install_dir"
    tar -xzf "$archive_path" -C "$install_dir"
}

collect_installed_binaries() {
    install_dir="$1"
    INSTALLED_BINARIES="$(find "" -maxdepth 1 -type f -name 'luchta*' -exec basename {} \; | LC_ALL=C sort)"
}

ensure_core_binary_present() {
    install_dir="$1"
    [ -f "$install_dir/luchta" ] || err "archive extraction incomplete: core binary 'luchta' is missing"
}

chmod_binaries() {
    install_dir="$1"
    while IFS= read -r binary; do
        [ -n "$binary" ] || continue
        chmod +x "$install_dir/$binary"
    done <<EOF
$INSTALLED_BINARIES
EOF
}

path_contains_dir() {
    case ":${PATH:-}:" in
        *":$1:"*) return 0 ;;
        *) return 1 ;;
    esac
}

print_success() {
    install_dir="$1"
    version="$2"
    triple="$3"

    printf 'Installed luchta %s for %s into %s\n' "$version" "$triple" "$install_dir"
    printf 'Installed binaries:\n'
    while IFS= read -r binary; do
        [ -n "$binary" ] || continue
        printf '  - %s\n' "$binary"
    done <<EOF
$INSTALLED_BINARIES
EOF

    if path_contains_dir "$install_dir"; then
        printf 'PATH already contains %s\n' "$install_dir"
    else
        printf 'Add this to your shell rc to use luchta and bundled workers automatically:\n'
        printf '  export PATH="%s:$''PATH"\n' "$install_dir"
    fi

    printf 'luchta discovers bundled workers via PATH. Keeping all binaries in %s lets tsc/oxc/etc. tasks resolve automatically.\n' "$install_dir"
}

main() {
    parse_args "$@"
    ensure_extract_tools
    TMP_DIR="$(make_tmpdir)"
    trap cleanup EXIT INT TERM HUP

    resolve_version
    resolve_triple

    ARCHIVE_URL="$(archive_url_for "$RESOLVED_VERSION" "$TARGET_TRIPLE")"
    ARCHIVE_PATH="$TMP_DIR/luchta.tar.gz"

    printf 'Downloading %s\n' "$ARCHIVE_URL"
    fetch_url "$ARCHIVE_URL" "$ARCHIVE_PATH" || err "failed to download release archive"
    [ -s "$ARCHIVE_PATH" ] || err "downloaded archive is empty"

    extract_archive "$ARCHIVE_PATH" "$INSTALL_DIR"
    collect_installed_binaries "$INSTALL_DIR"
    ensure_core_binary_present "$INSTALL_DIR"
    chmod_binaries "$INSTALL_DIR"
    print_success "$INSTALL_DIR" "$RESOLVED_VERSION" "$TARGET_TRIPLE"
}

main "$@"
