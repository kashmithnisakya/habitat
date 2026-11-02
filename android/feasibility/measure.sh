#!/usr/bin/env bash
# Measures startup, memory and CPU on the connected device, then prints a results table.
# See README.md for the method.
#
#   measure.sh <results.csv> [repeats]    Run the measurements and write them to a new file.
#   measure.sh --summary <results.csv>    Print the table for an existing file.
set -euo pipefail

APP="io.github.kashmithnisakya.habitat.feasibility"
# Set MODES to measure only some of them. Keep each CompileAgain mode after its CompileFirst mode.
MODES="${MODES:-CraneliftOnDevice CraneliftPrecompiled PulleyOnDevice PulleyPrecompiled CraneliftCompileFirst CraneliftCompileAgain PulleyCompileFirst PulleyCompileAgain}"
GUESTS="Hello Large"
HEADER="mode,guest,file_kb,app_ms,cold_us,warm_us,base_kb,peak_kb,cpu_elapsed_us,cpu_time_us,wait_elapsed_us,wait_time_us,denials"
TIMEOUT_SECONDS=300

HERE="$(cd "$(dirname "$0")" && pwd)"
SDK="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
ADB="$SDK/platform-tools/adb"

# Prints a Markdown table with the median of each measurement, and the range of the cold start.
summarize() {
    grep -v -E '^(#|mode,)' "$1" | awk -F, '
        function median(key, column,    i, j, value, count, sorted) {
            count = runs[key]
            for (i = 1; i <= count; i++) {
                value = data[key, column, i]
                for (j = i - 1; j >= 1 && sorted[j] > value; j--) sorted[j + 1] = sorted[j]
                sorted[j + 1] = value
            }
            low = sorted[1]
            high = sorted[count]
            return count % 2 ? sorted[(count + 1) / 2] : (sorted[count / 2] + sorted[count / 2 + 1]) / 2
        }
        {
            key = $1 " | " $2
            if (!(key in runs)) order[++keys] = key
            n = ++runs[key]
            data[key, "app", n] = $4
            data[key, "cold", n] = $5 / 1000
            data[key, "warm", n] = $6
            data[key, "base", n] = $7 / 1024
            data[key, "added", n] = ($8 - $7) / 1024
            data[key, "cpu", n] = $10 / 1000
            data[key, "elapsed", n] = $9 / 1000
            data[key, "wait", n] = $12 / 1000
            file[key] = $3
            denials += $13
        }
        END {
            print "| Mode | Guest | File (KB) | Runs | App start (ms) | Agent cold start, median (range) (ms) | Warm start (us) | Memory before agent (MB) | Memory added (MB) | CPU workload, CPU time (ms) | CPU workload, elapsed (ms) | CPU time while waiting (ms) |"
            print "|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|"
            for (k = 1; k <= keys; k++) {
                key = order[k]
                cold = median(key, "cold")
                coldLow = low
                coldHigh = high
                printf "| %s | %d | %d | %.0f | %.1f (%.1f-%.1f) | %.0f | %.0f | %.1f | %.0f | %.0f | %.1f |\n", key, file[key], runs[key], median(key, "app"), cold, coldLow, coldHigh, median(key, "warm"), median(key, "base"), median(key, "added"), median(key, "cpu"), median(key, "elapsed"), median(key, "wait")
            }
            printf "\nSELinux denials across all runs: %d\n", denials
        }'
}

if [ "${1:-}" = "--summary" ]; then
    summarize "$2"
    exit 0
fi

OUTPUT="${1:?Usage: measure.sh <results.csv> [repeats]}"
REPEATS="${2:-15}"
if [ -e "$OUTPUT" ]; then
    echo "$OUTPUT already exists." >&2
    exit 1
fi

# Starts one measurement in a new process and prints its row without the MEASURE prefix.
run_once() {
    local mode="$1" guest="$2" line="" waited=0 denials
    "$ADB" logcat -c
    "$ADB" shell am start -S -W -n "$APP/.MainActivity" --es mode "$mode" --es guest "$guest" >/dev/null
    until line="$("$ADB" logcat -d -v raw -s HabitatFeasibility:I | grep -E '^(MEASURE|FAIL)' | head -1)" && [ -n "$line" ]; do
        waited=$((waited + 1))
        if [ "$waited" -gt "$TIMEOUT_SECONDS" ]; then
            echo "Timed out: $mode $guest" >&2
            return 1
        fi
        sleep 1
    done
    # Other apps get denials too. Lines that don't name an app are counted, to be safe.
    denials="$("$ADB" logcat -d | awk -v app="app=$APP" '/avc: *denied/ && /untrusted_app/ && (!/ app=/ || index($0, app)) { count++ } END { print count + 0 }')"
    "$ADB" shell am force-stop "$APP"
    case "$line" in
        MEASURE,*) echo "${line#MEASURE,},$denials" ;;
        *) echo "$line" >&2; return 1 ;;
    esac
}

# Incremental installs stream the APK on demand, which would distort startup times.
"$ADB" install -r --no-incremental "$HERE/build/habitat-feasibility.apk"

{
    echo "# model: $("$ADB" shell getprop ro.product.model)"
    echo "# android: $("$ADB" shell getprop ro.build.version.release), API $("$ADB" shell getprop ro.build.version.sdk)"
    echo "# emulator: $("$ADB" shell getprop ro.kernel.qemu)"
    echo "# cpu: $("$ADB" shell getprop ro.product.cpu.abi), $("$ADB" shell nproc) cores"
    echo "# memory: $("$ADB" shell grep MemTotal /proc/meminfo)"
    echo "# page size: $("$ADB" shell getconf PAGE_SIZE)"
    echo "# selinux: $("$ADB" shell getenforce)"
    echo "# repeats: $REPEATS"
    echo "$HEADER"
} > "$OUTPUT"

# The first launch extracts the guest files, which would distort its memory figure.
echo "==> Extracting guest files"
run_once PulleyPrecompiled Hello >/dev/null

# Each repeat runs every combination, so slow drift affects them all equally.
for repeat in $(seq 1 "$REPEATS"); do
    echo "==> Repeat $repeat of $REPEATS"
    for guest in $GUESTS; do
        for mode in $MODES; do
            run_once "$mode" "$guest" >> "$OUTPUT"
        done
    done
done

summarize "$OUTPUT"
