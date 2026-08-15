#!/bin/sh
set -eu

repository="${CODEX_WAKE_REPOSITORY:-RainyPixel/codex-wake}"
version="${CODEX_WAKE_VERSION:-latest}"
bin_dir="${CODEX_WAKE_INSTALL_DIR:-$HOME/.local/bin}"
scope=""

usage() {
    cat <<'EOF'
Install the latest codex-wake release.

Usage: install.sh [--scope project|user] [--version vX.Y.Z] [--bin-dir PATH]

Environment:
  CODEX_WAKE_INSTALL_DIR  Destination directory (default: ~/.local/bin)
  CODEX_WAKE_VERSION      Release tag or latest
  CODEX_WAKE_REPOSITORY   GitHub owner/repository override
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --scope)
            [ "$#" -ge 2 ] || { echo "install.sh: --scope requires a value" >&2; exit 2; }
            scope="$2"
            shift 2
            ;;
        --version)
            [ "$#" -ge 2 ] || { echo "install.sh: --version requires a value" >&2; exit 2; }
            version="$2"
            shift 2
            ;;
        --bin-dir)
            [ "$#" -ge 2 ] || { echo "install.sh: --bin-dir requires a value" >&2; exit 2; }
            bin_dir="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "install.sh: unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

case "$scope" in
    ""|project|user|global) ;;
    *) echo "install.sh: --scope must be project or user" >&2; exit 2 ;;
esac

case "$version" in
    latest) release_path="latest/download" ;;
    *)
        echo "$version" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$' || {
            echo "install.sh: --version must be latest or a vX.Y.Z tag" >&2
            exit 2
        }
        release_path="download/$version"
        ;;
esac

case "$(uname -s)" in
    Linux) os="linux" ;;
    Darwin) os="macos" ;;
    *) echo "install.sh: unsupported operating system: $(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
    x86_64|amd64) arch="x86_64" ;;
    arm64|aarch64) arch="aarch64" ;;
    *) echo "install.sh: unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

case "$os/$arch" in
    linux/x86_64) target="x86_64-unknown-linux-musl" ;;
    linux/aarch64) target="aarch64-unknown-linux-musl" ;;
    macos/x86_64) target="x86_64-apple-darwin" ;;
    macos/aarch64) target="aarch64-apple-darwin" ;;
esac

command -v curl >/dev/null 2>&1 || { echo "install.sh: curl is required" >&2; exit 1; }
command -v tar >/dev/null 2>&1 || { echo "install.sh: tar is required" >&2; exit 1; }

asset="codex-wake-$target.tar.gz"
base_url="https://github.com/$repository/releases/$release_path"
temporary="$(mktemp -d "${TMPDIR:-/tmp}/codex-wake-install.XXXXXX")"
pending=""
cleanup() {
    [ -z "$pending" ] || rm -f "$pending"
    rm -rf "$temporary"
}
trap cleanup EXIT HUP INT TERM

curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location \
    --output "$temporary/$asset" "$base_url/$asset"
curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location \
    --output "$temporary/SHA256SUMS" "$base_url/SHA256SUMS"

expected="$(awk -v asset="$asset" '
    {
        name = $2
        sub(/^\*/, "", name)
        if (name == asset) print $1
    }
' "$temporary/SHA256SUMS")"
[ "${#expected}" -eq 64 ] || {
    echo "install.sh: SHA256SUMS has no valid entry for $asset" >&2
    exit 1
}

if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$temporary/$asset" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$temporary/$asset" | awk '{print $1}')"
else
    echo "install.sh: sha256sum or shasum is required" >&2
    exit 1
fi
[ "$actual" = "$expected" ] || {
    echo "install.sh: SHA-256 mismatch for $asset" >&2
    exit 1
}

tar -tzf "$temporary/$asset" >"$temporary/archive.list"
[ "$(grep -c '^codex-wake$' "$temporary/archive.list")" -eq 1 ] || {
    echo "install.sh: release archive has an unexpected layout" >&2
    exit 1
}
while IFS= read -r entry; do
    case "/$entry/" in
        *"/../"*|"//"*) echo "install.sh: release archive contains an unsafe path" >&2; exit 1 ;;
    esac
done <"$temporary/archive.list"
tar -xzf "$temporary/$asset" -C "$temporary" codex-wake

mkdir -p "$bin_dir"
destination="$bin_dir/codex-wake"
pending="$bin_dir/.codex-wake.install.$$"
cp "$temporary/codex-wake" "$pending"
chmod 0755 "$pending"
mv "$pending" "$destination"
pending=""

echo "installed $($destination --version) to $destination"

path_ready=false
case ":$PATH:" in
    *":$bin_dir:"*) path_ready=true ;;
esac

if [ "$scope" = "project" ] && [ "$path_ready" = false ]; then
    echo "install.sh: installed the binary, but project scope requires $bin_dir on PATH" >&2
    echo "add it to PATH, then run: codex-wake setup --scope project" >&2
    exit 1
fi

if [ -n "$scope" ]; then
    "$destination" setup --scope "$scope"
else
    echo "next: codex-wake setup --scope project|user"
fi

if [ "$path_ready" = false ]; then
    echo "warning: add $bin_dir to PATH" >&2
fi
