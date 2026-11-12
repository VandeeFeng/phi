#!/usr/bin/env bash
set -euo pipefail

usage() {
    echo "Usage: $0 [--output FILE | --install]"
    echo "Build twice from pinned sources and compare both builds."
    echo "Defaults to a new file under /tmp; --install replaces the vendored WASM."
}

mode=export
output=''
case "${1:-}" in
    '') if [[ $# -ne 0 ]]; then usage >&2; exit 2; fi ;;
    --help|-h) usage; exit 0 ;;
    --install)
        if [[ $# -ne 1 ]]; then usage >&2; exit 2; fi
        mode=install
        ;;
    --output)
        if [[ $# -ne 2 || -z "$2" ]]; then usage >&2; exit 2; fi
        output="$2"
        ;;
    *) usage >&2; exit 2 ;;
esac
if [[ -n "$output" && ( -e "$output" || -L "$output" ) ]]; then
    echo "Output already exists: $output" >&2
    exit 2
fi

readonly source_commit=5a7a0eeda87c99542f8cf3095b6d61ecfa755977
readonly quickjs_commit=6d46d07d04041b40f4f49eaa7fdebe44c314c699
readonly source_sha=cb3a2e58fd7187104b9294962e205c28433ecb7653d75771a35ef98dfdfb8484
readonly quickjs_sha=a62cf1ff7d6d2f82b90a2d247a57e9eb56b81c03feb1f372a53923426e358cb0
readonly sdk_version=32.0
readonly binaryen_version=130

case "$(uname -s)-$(uname -m)" in
    Linux-x86_64)
        sdk_platform=x86_64-linux
        binaryen_platform=x86_64-linux
        sdk_sha=55fc523ebfbc98f69d1034fcfcb83d1ff5610cd9ab7eceef6cd097a30ba4ef93
        binaryen_sha=0a18362361ad05465118cd8eeb72edaeec89de6894bc283576ef4e07aa3babcc
        ;;
    Linux-aarch64|Linux-arm64)
        sdk_platform=arm64-linux
        binaryen_platform=aarch64-linux
        sdk_sha=b2070865e6cb0c1e97a38e6ac8d9c37a9dfcd0752764ebabc6bacd3e60cedb96
        binaryen_sha=e6ae6e09ac40f4e14bc5be6f687c58e2995c84170013975fa641809dd3b480a0
        ;;
    *) echo 'Supported hosts: Linux x86_64 and ARM64.' >&2; exit 2 ;;
esac

for tool in curl tar sha256sum make cmp mktemp cp dd env mkdir rm dirname; do
    command -v "$tool" >/dev/null || { echo "Missing tool: $tool" >&2; exit 2; }
done
readonly script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly work_dir="$(mktemp -d /tmp/phi-quickjs-build.XXXXXXXX)"
trap 'rm -rf -- "$work_dir"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

fetch() {
    local url="$1" checksum="$2" archive="$3"
    curl --fail --location --silent --show-error --retry 3 \
        --proto '=https' --proto-redir '=https' \
        -H 'Accept: text/markdown, text/html' "$url" -o "$archive"
    if ! printf '%s  %s\n' "$checksum" "$archive" | sha256sum --check --status; then
        echo "SHA-256 mismatch: $url" >&2
        return 1
    fi
}

extract() {
    mkdir -p -- "$2"
    tar -xzf "$1" -C "$2" --strip-components=1 --no-same-owner
}

fetch "https://codeload.github.com/vercel-labs/quickjs-wasi/tar.gz/$source_commit" \
    "$source_sha" "$work_dir/source.tar.gz"
fetch "https://codeload.github.com/quickjs-ng/quickjs/tar.gz/$quickjs_commit" \
    "$quickjs_sha" "$work_dir/quickjs.tar.gz"
fetch "https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-32/wasi-sdk-$sdk_version-$sdk_platform.tar.gz" \
    "$sdk_sha" "$work_dir/sdk.tar.gz"
fetch "https://github.com/WebAssembly/binaryen/releases/download/version_$binaryen_version/binaryen-version_$binaryen_version-$binaryen_platform.tar.gz" \
    "$binaryen_sha" "$work_dir/binaryen.tar.gz"
extract "$work_dir/sdk.tar.gz" "$work_dir/sdk"
extract "$work_dir/binaryen.tar.gz" "$work_dir/binaryen"

for build in first second; do
    source_dir="$work_dir/$build"
    extract "$work_dir/source.tar.gz" "$source_dir"
    extract "$work_dir/quickjs.tar.gz" "$source_dir/quickjs-ng"
    # Do not inherit compiler flags, make overrides or optional optimizer discovery.
    env -i PATH=/usr/bin:/bin HOME="$work_dir" LC_ALL=C TZ=UTC SOURCE_DATE_EPOCH=0 \
        make -C "$source_dir" check-wasi-sdk quickjs.wasm \
        "WASI_SDK=$work_dir/sdk" "WASM_OPT=$work_dir/binaryen/bin/wasm-opt" OPT=-Oz
done

if ! cmp -s "$work_dir/first/quickjs.wasm" "$work_dir/second/quickjs.wasm"; then
    echo 'Independent builds differ; refusing to export a non-reproducible artifact.' >&2
    exit 1
fi
if [[ "$mode" == install ]]; then
    output="$script_dir/vendor/quickjs-wasi/quickjs.wasm"
    cp -- "$work_dir/first/quickjs.wasm" "$output"
elif [[ -z "$output" ]]; then
    output="$(mktemp /tmp/phi-quickjs.XXXXXXXX.wasm)"
    cp -- "$work_dir/first/quickjs.wasm" "$output"
else
    # Noclobber also protects against an output created while the build was running.
    (set -o noclobber; dd if="$work_dir/first/quickjs.wasm" status=none > "$output")
fi
printf 'Reproducible artifact: %s\n' "$output"
sha256sum -- "$output"
