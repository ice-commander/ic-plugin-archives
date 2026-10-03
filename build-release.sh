#!/bin/bash
# Builds this plugin's release into distr/: the bare library of each system under its own name, and md5sums.txt.
# macOS is built on this Mac; Windows and Linux in the application's Docker builders.
# A plugin that ships other people's libraries keeps them in 3rdparty/out/<platform>/<folder>/, made by hand with
# 3rdparty/build.sh; they go into distr/<folder>/ as they are — the layout they are installed in, beside the plugin.
set -e
cd "$(dirname "$0")"

NAME=$(node -p "require('./package.json').name")
VERSION=$(node -p "require('./package.json').version")
PLATFORMS=$(node -p "(require('./package.json').platforms || ['macos', 'windows', 'linux']).join(' ')")
# "crate" in package.json: build only that crate, where a whole-workspace build turns on features the plugin must not ship.
CRATE=$(node -p "require('./package.json').crate || ''")
CARGO_BUILD="cargo build --release${CRATE:+ -p $CRATE}"
DISTR="$PWD/distr"

node builder/gen-version.js
sh build.sh
sh test.sh

rm -rf "$DISTR"
mkdir -p "$DISTR"

md5_of() {
    if command -v md5sum >/dev/null; then md5sum "$1" | awk '{print $1}'; else md5 -q "$1"; fi
}

# The app finds an enabled plugin by its file name, so the library is published under the name it is built with.
take() {
    local label="$1" ext="$2" from="$3"
    shopt -s nullglob
    local libraries=("$from"/*."$ext")
    shopt -u nullglob
    if [ "${#libraries[@]}" -ne 1 ]; then
        echo "expected one .$ext library for $label in $from, found ${#libraries[@]}" >&2
        exit 1
    fi
    cp "${libraries[0]}" "$DISTR/"
    echo "$(md5_of "${libraries[0]}") [$label] $(basename "${libraries[0]}")" >> "$DISTR/md5sums.txt"
    echo "  distr/$(basename "${libraries[0]}")"
}

# What 3rdparty/build.sh left in 3rdparty/out/<platform>/, under the same paths; md5sums.txt names them with the folder.
# Never rebuilt here: the same files keep the same md5, and the updater does not fetch them again.
carry() {
    local platform="$1" label="$2"
    [ -f 3rdparty/build.sh ] || return 0
    if [ ! -d "3rdparty/out/$platform" ]; then
        echo "no 3rdparty/out/$platform: run bash 3rdparty/build.sh $platform first" >&2
        exit 1
    fi
    local path
    while IFS= read -r path; do
        case "$path" in
            */*) ;;
            *)
                echo "3rdparty/out/$platform/$path is not in a folder: the host would try to load it as a plugin" >&2
                exit 1
                ;;
        esac
        mkdir -p "$DISTR/$(dirname "$path")"
        cp "3rdparty/out/$platform/$path" "$DISTR/$path"
        echo "$(md5_of "$DISTR/$path") [$label] $path" >> "$DISTR/md5sums.txt"
        echo "  distr/$path"
    done < <(cd "3rdparty/out/$platform" && find . -type f ! -name '.*' | sed 's#^\./##' | sort)
}

# Linux is built on the oldest glibc of the supported distributions, so one .so loads on all of them.
in_docker() {
    docker run --rm -v "$PWD:/home/builder/w" -w /home/builder/w --user builder \
        --entrypoint bash "$1" -lc "export CARGO_HOME=/home/builder/w/bin/cargo-home; $2"
}

for platform in $PLATFORMS; do
    echo "=== $platform ==="
    case "$platform" in
        macos)
            if [ "$(uname -s)" != Darwin ]; then
                echo "the macOS build needs a Mac" >&2
                exit 1
            fi
            take "MACOS-$(uname -m | tr a-z A-Z)" dylib bin
            carry macos "MACOS-$(uname -m | tr a-z A-Z)"
            ;;
        windows)
            rm -f bin/target-win/x86_64-pc-windows-gnu/release/*.dll
            in_docker msys-builder "CARGO_TARGET_DIR=bin/target-win $CARGO_BUILD --target x86_64-pc-windows-gnu"
            take WINDOWS-X86_64 dll bin/target-win/x86_64-pc-windows-gnu/release
            carry windows WINDOWS-X86_64
            ;;
        linux)
            rm -f bin/target-linux/release/*.so
            in_docker fedora41-builder "CARGO_TARGET_DIR=bin/target-linux $CARGO_BUILD"
            take LINUX-X86_64 so bin/target-linux/release
            carry linux LINUX-X86_64
            ;;
        *)
            echo "unknown platform '$platform' in package.json" >&2
            exit 1
            ;;
    esac
done

# A release on GitHub is flat: every file is uploaded under its own name, whatever folder it goes to.
clashing=$(awk '{print $3}' "$DISTR/md5sums.txt" | sed 's#.*/##' | sort | uniq -d)
if [ -n "$clashing" ]; then
    echo "two files of this release share a name, and GitHub keeps one: $clashing" >&2
    exit 1
fi

dirty=""
[ -z "$(git status --porcelain)" ] || dirty=" dirty"
echo "$(git rev-parse HEAD)$dirty" > "$DISTR/.built-from"
echo "$NAME $VERSION built into distr/ from $(cat "$DISTR/.built-from")"
