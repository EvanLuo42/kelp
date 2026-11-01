#!/bin/sh
# Installs Kelp from a GitHub Release (Linux x86_64).
#
#   curl -fsSL https://raw.githubusercontent.com/EvanLuo42/kelp/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/EvanLuo42/kelp/main/install.sh | sh -s -- --components cli
#
# Run with --help for the options. Every option has an environment variable
# for use without arguments.

set -eu

REPO="EvanLuo42/kelp"

usage() {
    cat <<EOF
Installs Kelp from a GitHub Release.

Usage: install.sh [OPTIONS]

Options:
  -c, --components <list>  What to install: all, cli (kelp and kelpd), server
                           (kelp-server) or "cli,server"; asked when omitted
                           in a terminal, otherwise all
                           [env: KELP_COMPONENTS]
  -d, --dir <path>         Installation directory; asked when omitted in a
                           terminal, otherwise ~/.local/bin
                           [env: KELP_INSTALL_DIR]
  -v, --version <tag>      Release to install, such as v0.1.1 [default: latest]
                           [env: KELP_VERSION]
      --no-modify-path     Do not add the directory to PATH in shell profiles
                           [env: KELP_NO_MODIFY_PATH=1]
  -y, --yes                Do not ask questions; use defaults for the rest
                           [env: KELP_YES=1]
      --target <triple>    Override the detected platform
                           [env: KELP_TARGET]
  -h, --help               Prints this help
EOF
}

say() { printf '%s\n' "$*"; }
err() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

version="${KELP_VERSION:-latest}"
dir="${KELP_INSTALL_DIR:-}"
components="${KELP_COMPONENTS:-}"
target="${KELP_TARGET:-}"
modify_path=1
[ -n "${KELP_NO_MODIFY_PATH:-}" ] && modify_path=0
yes=0
[ -n "${KELP_YES:-}" ] && yes=1

while [ $# -gt 0 ]; do
    case "$1" in
    -c | --components | -d | --dir | -v | --version | --target)
        [ $# -ge 2 ] || err "$1 needs a value"
        value=$2
        flag=$1
        shift 2
        ;;
    --components=* | --dir=* | --version=* | --target=*)
        value=${1#*=}
        flag=${1%%=*}
        shift
        ;;
    --no-modify-path)
        modify_path=0
        shift
        continue
        ;;
    -y | --yes)
        yes=1
        shift
        continue
        ;;
    -h | --help)
        usage
        exit 0
        ;;
    *) err "unknown option $1 (see --help)" ;;
    esac
    case "$flag" in
    -c | --components) components=$value ;;
    -d | --dir) dir=$value ;;
    -v | --version) version=$value ;;
    --target) target=$value ;;
    esac
done

# Reads an answer from the terminal, even when the script itself comes from a
# pipe. Prints nothing when there is no terminal.
ask() {
    if [ "$yes" = 1 ] || ! (: </dev/tty) 2>/dev/null; then
        return 0
    fi
    printf '%s' "$1" >/dev/tty
    IFS= read -r answer </dev/tty || answer=""
    printf '%s' "$answer"
}

# --- platform

if [ -z "$target" ]; then
    os=$(uname -s)
    arch=$(uname -m)
    case "$os/$arch" in
    Linux/x86_64 | Linux/amd64) target="x86_64-unknown-linux-gnu" ;;
    *) err "no prebuilt binaries for $os/$arch; use 'cargo install kelp-server kelp-cli' instead" ;;
    esac
fi

# --- choices

if [ -z "$components" ]; then
    answer=$(ask "Install what?
  1) all    - kelp, kelpd and kelp-server
  2) cli    - kelp and kelpd
  3) server - kelp-server
Choice [1]: ")
    case "${answer:-1}" in
    1 | all) components=all ;;
    2 | cli) components=cli ;;
    3 | server) components=server ;;
    *) err "unknown choice '$answer'" ;;
    esac
fi

bins=""
for part in $(printf '%s' "$components" | tr ',' ' '); do
    case "$part" in
    all) bins="kelp kelpd kelp-server" ;;
    cli) bins="$bins kelp kelpd" ;;
    server) bins="$bins kelp-server" ;;
    *) err "unknown component '$part' (use all, cli or server)" ;;
    esac
done

if [ -z "$dir" ]; then
    default_dir="$HOME/.local/bin"
    answer=$(ask "Install into [$default_dir]: ")
    dir=${answer:-$default_dir}
fi
case "$dir" in
"~") dir=$HOME ;;
"~/"*) dir="$HOME/${dir#"~/"}" ;;
esac

# --- download

if command -v curl >/dev/null 2>&1; then
    download() { curl -fsSL -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
    download() { wget -qO "$2" "$1"; }
else
    err "curl or wget is required"
fi

if [ -n "${KELP_BASE_URL:-}" ]; then
    base=$KELP_BASE_URL
elif [ "$version" = latest ]; then
    base="https://github.com/$REPO/releases/latest/download"
else
    case "$version" in
    v*) ;;
    *) version="v$version" ;;
    esac
    base="https://github.com/$REPO/releases/download/$version"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

archive="kelp-$target.tar.gz"
say "Downloading $archive ($version)"
download "$base/$archive" "$tmp/$archive" ||
    err "could not download $base/$archive (is there a release for $target?)"

if download "$base/SHA256SUMS" "$tmp/SHA256SUMS" 2>/dev/null; then
    expected=$(awk -v f="$archive" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")
    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$tmp/$archive" | awk '{ print $1 }')
    elif command -v shasum >/dev/null 2>&1; then
        actual=$(shasum -a 256 "$tmp/$archive" | awk '{ print $1 }')
    else
        actual=""
        say "warning: no sha256sum or shasum; skipping checksum verification"
    fi
    if [ -n "$actual" ]; then
        [ -n "$expected" ] || err "SHA256SUMS has no entry for $archive"
        [ "$expected" = "$actual" ] || err "checksum mismatch for $archive"
    fi
else
    say "warning: no SHA256SUMS in this release; skipping checksum verification"
fi

tar -xzf "$tmp/$archive" -C "$tmp"
src="$tmp/kelp-$target"

# --- install

mkdir -p "$dir" || err "cannot create $dir; choose another directory with --dir"
[ -w "$dir" ] || err "$dir is not writable; choose another directory with --dir"
for bin in $bins; do
    [ -f "$src/$bin" ] || err "$bin is not in the archive"
    # Copy beside the target and rename, so a running binary can be replaced.
    cp "$src/$bin" "$dir/.$bin.new"
    chmod 755 "$dir/.$bin.new"
    mv -f "$dir/.$bin.new" "$dir/$bin"
    say "Installed $dir/$bin"
done

# --- PATH

case ":$PATH:" in
*":$dir:"*) on_path=1 ;;
*) on_path=0 ;;
esac

if [ "$on_path" = 0 ]; then
    shown=$dir
    case "$dir" in
    "$HOME"/*) shown="\$HOME/${dir#"$HOME"/}" ;;
    esac
    line="export PATH=\"$shown:\$PATH\""

    if [ "$modify_path" = 1 ]; then
        add_line() {
            [ -f "$1" ] && grep -qxF "$line" "$1" && return 0
            printf '\n# Added by the Kelp installer\n%s\n' "$line" >>"$1"
            say "Added $dir to PATH in $1"
        }
        shell_name=$(basename "${SHELL:-sh}")
        add_line "$HOME/.profile"
        case "$shell_name" in
        bash) add_line "$HOME/.bashrc" ;;
        zsh) add_line "${ZDOTDIR:-$HOME}/.zshenv" ;;
        fish)
            mkdir -p "$HOME/.config/fish/conf.d"
            fish_file="$HOME/.config/fish/conf.d/kelp.fish"
            if ! [ -f "$fish_file" ]; then
                printf 'fish_add_path "%s"\n' "$dir" >"$fish_file"
                say "Added $dir to PATH in $fish_file"
            fi
            ;;
        esac
        say "Open a new terminal, or run '$line', to use Kelp."
    else
        say "$dir is not in PATH; add it with: $line"
    fi
fi

for bin in $bins; do
    "$dir/$bin" --version 2>/dev/null || true
done
