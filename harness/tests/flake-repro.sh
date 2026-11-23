#!/usr/bin/env bash
# Run from the repository root on the host.
set -eu

mkdir -p "${CARGO_TARGET_DIR:-target}"
logs=$(mktemp -d "${CARGO_TARGET_DIR:-target}/process-flake.XXXXXX")
burners=()
cleanup() {
    if (("${#burners[@]}")); then
        kill "${burners[@]}" 2>/dev/null || true
        wait "${burners[@]}" 2>/dev/null || true
    fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

for ((i = 0; i < 6; i++)); do
    yes >/dev/null &
    burners+=("$!")
done

failures=0
for ((run = 1; run <= 20; run++)); do
    log="$logs/run-$run.log"
    if cargo test -p cs-harness --lib -- --nocapture >"$log" 2>&1; then
        rm "$log"
        printf 'Run %d/20 passed\n' "$run"
    else
        failures=$((failures + 1))
        printf 'Run %d/20 failed. Log saved to %s\n' "$run" "$log"
    fi
done
printf '%d of 20 runs failed. Logs directory %s\n' "$failures" "$logs"
((failures == 0))
