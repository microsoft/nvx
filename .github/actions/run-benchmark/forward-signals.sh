# shellcheck shell=bash
# Sourced by the Linux steps of run-benchmark, which run the test harness
# among other commands (see doc/ci.md#cancellation).
#
# When a job is cancelled, the runner sends SIGINT, and then SIGTERM, to the
# step's shell alone, and Bash runs a trap only once its foreground command
# ends. So sourcing this file traps both signals: one that arrives while
# run_harness runs a command reaches that command as SIGTERM, which nvx.py
# handles as SIGINT, so that it stops its OpenVMM processes and removes its
# scratch directories, and ends the step once the command exits; one that
# arrives at any other time ends the step at once.

harness_pid=
starting=
deferred=
cancelled=

on_cancellation() {
    cancelled=$1
    if [[ -n "${starting}" ]]; then
        # The command may have started, but run_harness doesn't know its PID
        # yet, so it passes the signal on once it does.
        deferred=1
    elif [[ -n "${harness_pid}" ]]; then
        # The command may have exited already, so that the signal reaches nothing.
        kill -TERM "${harness_pid}" 2>/dev/null || true
    else
        exit "$1"
    fi
}

trap 'on_cancellation 130' INT
trap 'on_cancellation 143' TERM

# Runs a command in the background, where Bash would ignore SIGINT unless the
# command restored it, and returns its exit status once it exits. A
# cancellation reaches the command as SIGTERM, which it never ignores, not
# even before it has restored SIGINT, as it has not when a cancellation that
# arrived as it started reaches it.
run_harness() {
    starting=1
    (
        trap - INT
        exec "$@"
    ) &
    harness_pid=$!
    starting=
    if [[ -n "${deferred}" ]]; then
        deferred=
        kill -TERM "${harness_pid}" 2>/dev/null || true
    fi
    local status=0
    wait "${harness_pid}" || status=$?
    # A trapped signal ends the wait before the command exits.
    while kill -0 "${harness_pid}" 2>/dev/null; do
        status=0
        wait "${harness_pid}" || status=$?
    done
    harness_pid=
    if [[ -n "${cancelled}" ]]; then
        exit "${cancelled}"
    fi
    return "${status}"
}
