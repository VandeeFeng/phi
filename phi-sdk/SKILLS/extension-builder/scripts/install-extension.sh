#!/usr/bin/env sh
set -eu

candidate=${1:-}
scope=${2:-global}
if [ -z "$candidate" ]; then
  echo "usage: $0 /tmp/<extension>.lua [global|project]" >&2
  exit 2
fi

case "$scope" in
  global) target_dir=${PHI_CONFIG_DIR:-"$HOME/.phi"}/extensions ;;
  project) target_dir="$PWD/.phi/extensions" ;;
  *) echo "scope must be global or project" >&2; exit 2 ;;
esac

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
lua "$script_dir/test-extension.lua" "$candidate"

name=$(basename -- "$candidate")
target="$target_dir/$name"
if [ -e "$target" ]; then
  echo "refusing to overwrite existing extension: $target" >&2
  exit 1
fi

mkdir -p "$target_dir"
mv -- "$candidate" "$target"
printf 'installed %s\nRun /reload in Phi.\n' "$target"
