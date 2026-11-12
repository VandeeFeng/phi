#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf '%s\n' \
         'usage: ./test.sh [full|web|runtime|sdk|repeat|check|focus CRATE FILTER]' \
         '  full: Web tests/checks, workspace tests, then cargo check (default)' \
         '  web: Web tests, type checking and lint/format checks' \
         '  check: Rust and Web static checks' \
         '  runtime: application runtime regressions' \
         '  sdk: SDK harness regressions (Manager and Instance)' \
         '  repeat: repeat runtime and SDK harness suites (TEST_REPEAT_ROUNDS=5)' \
         '  focus: run a named test filter in an existing workspace crate' \
         'defaults: 2 build jobs, 4 test threads, no debug info, 6G memory cap' \
         'override with CARGO_BUILD_JOBS, RUST_TEST_THREADS and TEST_MEMORY_MAX' \
         'compile/test jobs require memory isolation; separate Cargo suites do not overlap' \
         'logs remain in /tmp; isolated test Session/config/temp directories are deleted'
}

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
cd "$root"
command -v setsid >/dev/null || { printf 'setsid is required\n' >&2; exit 1; }
mode=${1:-full}
case "$mode" in
  full|web|runtime|sdk|repeat|check) [[ $# -le 1 ]] || { usage; exit 2; } ;;
  focus) [[ $# -eq 3 ]] || { usage; exit 2; }
         case "$2" in phi-sdk|phi-interactive|phi-tui|phi-codemode|phi-remote) ;; *) usage; exit 2 ;; esac ;;
  -h|--help) usage; exit 0 ;;
  *) usage; exit 2 ;;
esac
rounds=${TEST_REPEAT_ROUNDS:-5}
[[ "$rounds" =~ ^[1-9][0-9]*$ ]] || { printf 'invalid TEST_REPEAT_ROUNDS\n' >&2; exit 2; }
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}
export RUST_TEST_THREADS=${RUST_TEST_THREADS:-4}
export CARGO_PROFILE_DEV_DEBUG=${CARGO_PROFILE_DEV_DEBUG:-0}
export CARGO_PROFILE_TEST_DEBUG=${CARGO_PROFILE_TEST_DEBUG:-0}
memory_max=${TEST_MEMORY_MAX:-6G}
command -v systemd-run >/dev/null && systemctl --user is-active --quiet default.target || {
    printf 'a user systemd manager is required for memory-isolated validation\n' >&2
    exit 1
  }
logs=$(mktemp -d /tmp/phi-validation-XXXXXX)
pids=()
names=()
session_directories=()

cleanup() {
  local pid directory
  for pid in "${pids[@]}"; do
    kill -- "-$pid" 2>/dev/null || true
  done
  for pid in "${pids[@]}"; do
    wait "$pid" 2>/dev/null || true
  done
  for directory in "${session_directories[@]}"; do
    rm -rf -- "$directory"
  done
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

start_job() {
  local name=$1 directory
  shift
  directory=$(mktemp -d /tmp/pht-XXXXXX)
  session_directories+=("$directory")
  mkdir -p "$directory/sessions" "$directory/config" "$directory/tmp"
  setsid --wait systemd-run --user --scope --quiet --collect \
         --property="MemoryMax=$memory_max" --property=MemorySwapMax=0 \
         env PHI_SESSION_DIR="$directory/sessions" \
         PHI_CONFIG_DIR="$directory/config" TMPDIR="$directory/tmp" \
         "$@" >"$logs/$name.log" 2>&1 &
  pids+=("$!")
  names+=("$name")
}

wait_jobs() {
  local index failed=0
  for index in "${!pids[@]}"; do
    if wait "${pids[$index]}"; then
      printf 'PASS %s\n' "${names[$index]}"
      grep -E 'test result:|Finished' "$logs/${names[$index]}.log" || true
    else
      printf 'FAIL %s (%s)\n' "${names[$index]}" "$logs/${names[$index]}.log" >&2
      tail -n 40 "$logs/${names[$index]}.log" >&2
      failed=1
    fi
    rm -rf -- "${session_directories[$index]}"
  done
  pids=()
  names=()
  session_directories=()
  return "$failed"
}

start_static_checks() {
  start_job format cargo fmt --all -- --check
  start_job whitespace bash -c 'git diff --check && git diff --cached --check'
}

start_web_checks() {
  start_job web-test pnpm --dir phi-remote/plugins/web test
  start_job web-check pnpm --dir phi-remote/plugins/web check
}

start_checks() {
  start_job check cargo check --workspace
  start_job web-check pnpm --dir phi-remote/plugins/web check
  start_static_checks
}

printf 'logs: %s\nbuild jobs: %s; test threads: %s; memory cap: %s\n' \
       "$logs" "$CARGO_BUILD_JOBS" "$RUST_TEST_THREADS" "$memory_max"
case "$mode" in
  full)
    start_web_checks
    start_static_checks
    wait_jobs
    start_job workspace cargo test --workspace
    wait_jobs
    start_job check cargo check --workspace
    ;;
  web) start_web_checks ;;
  runtime) start_job runtime cargo test -p phi-interactive runtime:: ;;
  sdk) start_job sdk-harness cargo test -p phi-sdk harness:: ;;
  focus) start_job focused cargo test -p "$2" "$3" ;;
  check) start_checks ;;
  repeat)
          for ((round = 1; round <= rounds; round++)); do
            start_job "runtime-$round" cargo test -p phi-interactive runtime::
            wait_jobs
            start_job "sdk-harness-$round" cargo test -p phi-sdk harness::
            wait_jobs
          done
          ;;
  esac
  wait_jobs
