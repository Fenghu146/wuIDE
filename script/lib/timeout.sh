#!/usr/bin/env bash

# Bounded execution for the steps that can stall with no output at all. Sourced
# by the bundlers and by `generate-licenses`, not run on its own.

# Run "$@" in its own process group and kill the group once it has outlived the
# limit, so the command and whatever it started die together and the caller's
# `set -e` fails on the non-zero `wait`. `timeout(1)` is not on every runner
# (macOS ships none), so this stays in bash. `set -m` gives the background job a
# process group of its own.
run_with_timeout() {
    local limit="$1"
    local message="$2"
    shift 2
    local pid watchdog
    local rc=0
    set -m
    "$@" &
    pid=$!
    set +m
    (
        local waited=0
        while [ "$waited" -lt "$limit" ] && kill -0 "$pid" 2>/dev/null; do
            sleep 5
            waited=$((waited + 5))
        done
        kill -0 "$pid" 2>/dev/null || exit 0
        printf '%s\n' "$message" >&2
        kill -TERM -"$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null || true
        sleep 5
        kill -KILL -"$pid" 2>/dev/null || kill -KILL "$pid" 2>/dev/null || true
    ) &
    watchdog=$!
    wait "$pid" || rc=$?
    kill "$watchdog" 2>/dev/null || true
    wait "$watchdog" 2>/dev/null || true
    return "$rc"
}

# Download the lockfile's crates and git sources as a step of its own, before
# anything needs them. `cargo about` fetches them itself through `cargo metadata`
# with its output captured, and that fetch is where 1.4.7's bundle_mac (aarch64)
# printed nothing for 4h55m: done here, it shows progress, and when it does stall
# the job says so by name instead of the next step hanging silently.
prefetch_crates() {
    local limit="${CARGO_FETCH_TIMEOUT_SECONDS:-600}"
    run_with_timeout "$limit" "cargo fetch --locked timed out after ${limit}s" cargo fetch --locked
}
