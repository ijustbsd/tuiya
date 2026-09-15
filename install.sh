#!/bin/sh
set -eu

die() {
    printf 'Error: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<'EOF'
Usage: install.sh [--version VERSION] [--bin-dir DIRECTORY]

Install tuiya from GitHub releases (Linux x86_64/aarch64 or macOS).

  --version VERSION    Release to install, e.g. 0.1.0 or v0.1.0 (default: latest)
  --bin-dir DIRECTORY  Destination directory (default: ~/.local/bin)
  -h, --help           Show this help
EOF
}

main() {
    version=latest
    bin_dir=${HOME:?HOME must be set}/.local/bin
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --version|--bin-dir)
                [ "$#" -ge 2 ] && [ -n "$2" ] || die "$1 requires a value"
                case "$1" in
                    --version) version=$2 ;;
                    --bin-dir) bin_dir=$2 ;;
                esac
                shift 2
                ;;
            -h|--help) usage; return ;;
            *) die "Unknown argument: $1 (see --help)" ;;
        esac
    done

    for cmd in curl tar uname mktemp mkdir install mv rm; do
        command -v "$cmd" >/dev/null 2>&1 || die "Required command not found: $cmd"
    done

    platform=$(uname -s)
    arch=$(uname -m)
    case "$platform/$arch" in
        Linux/x86_64|Linux/amd64) target=x86_64-unknown-linux-gnu ;;
        Linux/aarch64|Linux/arm64) target=aarch64-unknown-linux-gnu ;;
        Darwin/x86_64|Darwin/arm64|Darwin/aarch64) target=universal-apple-darwin ;;
        *) die "Unsupported platform: $platform/$arch" ;;
    esac

    releases=https://github.com/ijustbsd/tuiya/releases
    if [ "$version" = latest ]; then
        printf 'Looking up the latest tuiya release...\n'
        release_url=$(curl --fail --silent --show-error --location \
            --proto '=https' --proto-redir '=https' \
            --output /dev/null --write-out '%{url_effective}' "$releases/latest") \
            || die "Could not find the latest release. Check your connection and GitHub releases."
        case "$release_url" in
            "$releases/tag/v"*) version=${release_url##*/} ;;
            *) die "No published release found at $releases" ;;
        esac
    fi
    version=${version#v}
    case "$version" in
        ''|*[!0-9A-Za-z.+-]*) die "Invalid version: $version" ;;
    esac
    case "$version" in
        [0-9]*) ;;
        *) die "Version must start with a number: $version" ;;
    esac

    # Make paths absolute so directory names cannot become command options.
    case "$bin_dir" in
        /*) ;;
        *) bin_dir=$PWD/$bin_dir ;;
    esac
    tmp_dir=$(mktemp -d)
    staged_binary=
    trap 'rm -rf "$tmp_dir"; if [ -n "$staged_binary" ]; then rm -f "$staged_binary"; fi' EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM
    trap 'exit 129' HUP

    archive=tuiya-$version-$target.tar.gz
    printf 'Downloading tuiya %s for %s...\n' "$version" "$target"
    curl --fail --silent --show-error --location \
        --proto '=https' --proto-redir '=https' \
        --output "$tmp_dir/release.tar.gz" "$releases/download/v$version/$archive" \
        || die "Download failed. Check that v$version includes $archive at $releases."
    tar -xzf "$tmp_dir/release.tar.gz" -C "$tmp_dir" tuiya \
        || die "Could not unpack the release archive"
    [ -f "$tmp_dir/tuiya" ] && [ ! -L "$tmp_dir/tuiya" ] \
        || die "The release archive does not contain a regular tuiya binary"

    mkdir -p "$bin_dir" || die "Cannot create $bin_dir; use --bin-dir to choose a writable directory"
    [ ! -d "$bin_dir/tuiya" ] || die "$bin_dir/tuiya is a directory"
    # Stage on the destination filesystem before replacing an existing install.
    staged_binary=$(mktemp "$bin_dir/.tuiya.XXXXXX") \
        || die "Cannot write to $bin_dir; use --bin-dir to choose a writable directory"
    install -m 755 "$tmp_dir/tuiya" "$staged_binary"
    mv -f "$staged_binary" "$bin_dir/tuiya"
    staged_binary=

    printf 'Installed tuiya %s to %s/tuiya\n' "$version" "$bin_dir"
    case ":${PATH:-}:" in
        *:"$bin_dir":*) printf 'Run: tuiya\n' ;;
        *) printf 'Add %s to your PATH, or run: "%s/tuiya"\n' "$bin_dir" "$bin_dir" ;;
    esac
}

# Keep piped execution from starting until the complete script has arrived.
main "$@"
