#!/bin/bash
# Checks that the chronicle-ui test watchdog still ends a hung test run.
#
# It runs each opt-in self-test, which hangs on purpose, with a 5 s watchdog.
# The watchdog reads two variables, and this script sets both:
#   CHRONICLE_TEST_WATCHDOG_SECONDS   seconds before the run is ended; 0 turns
#                                     the watchdog off; unset means 120
#   CHRONICLE_TEST_WATCHDOG_SELFTEST  1 enables the self-tests
# CI's main swift test step must set neither.
#
# Extra arguments go to swift, for example --only-use-versions-from-resolved-file.
#
# The build gets 600 s. On a cold checkout, run swift build --build-tests in
# chronicle-ui first, or rerun after a "FAIL build: still running" line.
set -euo pipefail
cd "$(dirname "$0")/../chronicle-ui"

watchdog_seconds=5
deadline=60
build_deadline=600
swift_args=("$@")
logs=$(mktemp -d)
job=
helpers=()
saw_testing_helper=0
failed=0

record_children() {
  [ -n "$job" ] || return 0
  local child
  for child in $(pgrep -P "$job" || true); do
    helpers+=("$child")
    case "$(ps -o comm= -p "$child" 2>/dev/null)" in
      *swiftpm-testing-helper) saw_testing_helper=1 ;;
    esac
  done
}

show_log() {
  sed 's/^/    /' "$1"
}

signal_run() {
  if [ -n "$job" ]; then kill -s "$1" -- "-$job" 2>/dev/null || true; fi
  local helper
  for helper in ${helpers[@]+"${helpers[@]}"}; do
    kill -s "$1" -- "-$helper" 2>/dev/null || true
  done
}

run_alive() {
  if [ -n "$job" ] && kill -0 "$job" 2>/dev/null; then return 0; fi
  local helper
  for helper in ${helpers[@]+"${helpers[@]}"}; do
    if kill -0 "$helper" 2>/dev/null; then return 0; fi
  done
  return 1
}

# TERM, up to 3 s of grace, then KILL: cleanup has a deadline too.
kill_run() {
  record_children
  signal_run TERM
  local waited=0
  while run_alive && (( waited < 3 )); do sleep 1; waited=$((waited + 1)); done
  signal_run KILL
}
trap kill_run EXIT
trap 'kill_run; exit 130' INT
trap 'kill_run; exit 143' TERM

# The build waits on the .build lock that any other swift test in this
# checkout holds, so it gets a deadline too.
build_log="$logs/build.log"
echo "Building tests (up to ${build_deadline} s)..."
set -m
swift build --build-tests ${swift_args[@]+"${swift_args[@]}"} </dev/null >"$build_log" 2>&1 &
job=$!
set +m
start=$SECONDS
while kill -0 "$job" 2>/dev/null; do
  if (( SECONDS - start >= build_deadline )); then
    echo "FAIL build: still running after ${build_deadline} s"
    kill_run
    job=
    helpers=()
    show_log "$build_log"
    exit 1
  fi
  sleep 1
done
build_status=0
wait "$job" || build_status=$?
job=
if (( build_status != 0 )); then
  echo "FAIL build: exit $build_status"
  show_log "$build_log"
  exit "$build_status"
fi

check() {
  local name=$1 log="$logs/$1.log" start status=0 ok=1 helper
  helpers=()
  saw_testing_helper=0
  set -m
  CHRONICLE_TEST_WATCHDOG_SELFTEST=1 CHRONICLE_TEST_WATCHDOG_SECONDS=$watchdog_seconds \
    swift test --skip-build --filter "$name" ${swift_args[@]+"${swift_args[@]}"} </dev/null >"$log" 2>&1 &
  job=$!
  set +m
  start=$SECONDS
  while kill -0 "$job" 2>/dev/null; do
    record_children
    if (( SECONDS - start >= deadline )); then
      echo "FAIL $name: still running after ${deadline} s"
      kill_run
      wait "$job" 2>/dev/null || true
      job=
      for helper in ${helpers[@]+"${helpers[@]}"}; do
        if kill -0 "$helper" 2>/dev/null; then echo "  helper $helper survived KILL"; fi
      done
      helpers=()
      show_log "$log"
      failed=1
      return
    fi
    sleep 1
  done
  wait "$job" || status=$?
  job=

  if (( status == 0 )); then echo "  exit status was 0"; ok=0; fi
  if ! grep -qF "CHRONICLE TEST WATCHDOG: N is $watchdog_seconds s" "$log"; then echo "  no override line"; ok=0; fi
  local fired
  fired=$(sed -n '/^CHRONICLE TEST WATCHDOG FIRED:/,$p' "$log")
  if [[ "$fired" != *"  $name() ("* ]]; then
    echo "  no FIRED line naming $name"; ok=0
  fi
  if [ "$name" = watchdogFiresWhileTheCooperativePoolIsBlocked ]; then
    local entered cpus
    entered=$(grep -c "^CHRONICLE TEST WATCHDOG SELFTEST: blocker [0-9]* entered" "$log" || true)
    cpus=$(sysctl -n hw.activecpu)
    if (( entered < cpus )); then echo "  only $entered of $cpus pool threads blocked"; ok=0; fi
    if (( entered >= cpus + 2 )); then echo "  all $entered blockers ran, so the pool was never full"; ok=0; fi
  fi
  if (( ! saw_testing_helper )); then echo "  no swiftpm-testing-helper seen"; ok=0; fi
  for helper in ${helpers[@]+"${helpers[@]}"}; do
    if kill -0 "$helper" 2>/dev/null; then
      echo "  helper $helper outlived the run"; ok=0
      kill -s KILL -- "-$helper" 2>/dev/null || true
    fi
  done
  helpers=()

  if (( ok )); then
    echo "PASS $name ($(( SECONDS - start )) s)"
  else
    echo "FAIL $name; log:"
    show_log "$log"
    failed=1
  fi
}

check watchdogFiresOnAParkedContinuation
check watchdogFiresWhileTheCooperativePoolIsBlocked
if (( ! failed )); then rm -rf "$logs"; fi
exit "$failed"
