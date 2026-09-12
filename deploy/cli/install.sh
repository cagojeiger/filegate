#!/bin/sh
set -eu

fail() { printf 'gscli installer: %s\n' "$*" >&2; exit 1; }
version=
bin_dir=${HOME:?HOME is required}/.local/bin
while [ "$#" -gt 0 ]; do
    case "$1" in
        --version|--bin-dir)
            [ "$#" -ge 2 ] || fail "$1 requires a value"
            case "$1" in
                --version) version=$2 ;;
                --bin-dir) bin_dir=$2 ;;
            esac
            shift 2 ;;
        --help|-h)
            printf '%s\n' 'Usage: sh install.sh --version X.Y.Z [--bin-dir PATH]'
            exit 0 ;;
        *) fail "unknown argument: $1" ;;
    esac
done
case "$version" in ''|*[!0-9.]*) fail 'specify --version X.Y.Z' ;; esac
printf '%s\n' "$version" | LC_ALL=C grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' || fail 'specify --version X.Y.Z'
case "$bin_dir" in /*) ;; *) fail '--bin-dir must be an absolute path' ;; esac
case "$(uname -s)/$(uname -m)" in
    Linux/x86_64) target=x86_64-unknown-linux-gnu ;;
    Linux/aarch64|Linux/arm64) target=aarch64-unknown-linux-gnu ;;
    Darwin/x86_64) target=x86_64-apple-darwin ;;
    Darwin/arm64|Darwin/aarch64) target=aarch64-apple-darwin ;;
    *) fail 'supported platforms: Linux/macOS on x86_64 or arm64' ;;
esac
command -v curl >/dev/null 2>&1 || fail 'curl is required'
if command -v sha256sum >/dev/null 2>&1; then
    checksum() { sha256sum "$1"; }
elif command -v shasum >/dev/null 2>&1; then
    checksum() { shasum -a 256 "$1"; }
else
    fail 'sha256sum or shasum is required'
fi
mkdir -p "$bin_dir"
destination=$bin_dir/gscli
[ ! -L "$destination" ] || fail 'destination is a symlink'
[ ! -e "$destination" ] || [ -f "$destination" ] || fail 'destination is not a regular file'
tmp=$(mktemp -d "$bin_dir/.gscli-install.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' HUP INT TERM
asset=gscli-$target
base=https://github.com/cagojeiger/filegate/releases/download/v$version
download() {
    curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' \
        --connect-timeout 15 --max-time 300 "$base/$1" --output "$tmp/$1"
}
download "$asset"
download "$asset.sha256"
expected=$(cat "$tmp/$asset.sha256")
printf '%s\n' "$expected" | LC_ALL=C grep -Eq '^[0-9a-f]{64}$' || fail 'invalid checksum file'
actual=$(checksum "$tmp/$asset")
actual=${actual%% *}
[ "$actual" = "$expected" ] || fail 'checksum mismatch; existing installation preserved'
chmod 755 "$tmp/$asset"
[ "$("$tmp/$asset" --version)" = "gscli $version" ] || fail 'binary version mismatch; existing installation preserved'
"$tmp/$asset" __install --bin-dir "$bin_dir" >/dev/null
printf 'Installed gscli %s at %s\n' "$version" "$destination"
