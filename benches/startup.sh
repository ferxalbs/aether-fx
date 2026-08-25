#!/bin/sh
set -eu

# Linux process-cold startup probe: every sample execs a fresh process. Kernel
# page cache is intentionally left alone, matching normal CLI invocation.
binary=${1:-target/release/aether}
samples=${AETHER_STARTUP_SAMPLES:-30}

if [ ! -x "$binary" ]; then
    echo "missing executable: $binary" >&2
    exit 1
fi
if ! /usr/bin/time -f '%e' -o /dev/null true 2>/dev/null; then
    echo "GNU /usr/bin/time is required" >&2
    exit 1
fi

results=$(mktemp)
trap 'rm -f "$results"' EXIT HUP INT TERM

measure() {
    name=$1
    shift
    : >"$results"
    count=0
    while [ "$count" -lt "$samples" ]; do
        /usr/bin/time -f '%e' -o "$results" -a "$@" >/dev/null 2>/dev/null
        count=$((count + 1))
    done
    sort -n "$results" | awk -v name="$name" -v samples="$samples" '
        { values[NR] = $1; sum += $1 }
        END {
            middle = int((samples + 1) / 2)
            if (samples % 2 == 0) {
                median = (values[middle] + values[middle + 1]) / 2
            } else {
                median = values[middle]
            }
            printf "%s samples=%d min=%.3fs median=%.3fs mean=%.3fs max=%.3fs\n", \
                name, samples, values[1], median, sum / samples, values[samples]
        }
    '
}

bytes=$(wc -c <"$binary" | tr -d ' ')
printf 'binary=%s bytes=%s\n' "$binary" "$bytes"
measure version "$binary" --version
measure help "$binary" --help
measure minimal-agent sh -c 'exec "$1" </dev/null' sh "$binary"
