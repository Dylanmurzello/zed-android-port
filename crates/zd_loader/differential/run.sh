#!/bin/sh
# The differential suite: golden.sh under a real Linux (an Ubuntu 26.04
# arm64 container, matching the rootfs) and under the engine on the
# connected tablet, outputs diffed. A CI oracle, never shipped. Known
# divergence classes are listed in the plan's chunk 3 findings.
#
#   ANDROID_SERIAL=<tablet>   which device, when more than one is attached
#   ZD_LOADER=<device path>   a loader other than the installed one
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT:-/tmp/zd-differential}
mkdir -p "$OUT"
echo "== oracle: ubuntu:26.04 (docker)"
docker run --rm -v "$HERE/golden.sh:/golden.sh:ro" ubuntu:26.04 bash /golden.sh > "$OUT/oracle.txt" 2>&1 || true
echo "== engine: com.zdroid on $(adb get-serialno)"
adb push "$HERE/golden.sh" /data/local/tmp/zd-golden.sh >/dev/null
adb push "$HERE/engine-runner.sh" /data/local/tmp/zd-golden-runner.sh >/dev/null
adb shell "ZD_LOADER=${ZD_LOADER:-} run-as com.zdroid sh /data/local/tmp/zd-golden-runner.sh" > "$OUT/engine.txt" 2>&1 || true
adb shell rm -f /data/local/tmp/zd-golden.sh /data/local/tmp/zd-golden-runner.sh
if diff -u "$OUT/oracle.txt" "$OUT/engine.txt" > "$OUT/diff.txt"; then
    echo "no differences ($(wc -l < "$OUT/oracle.txt" | tr -d ' ') lines)"
else
    cat "$OUT/diff.txt"
    echo "differences: $(grep -c '^[-+][^-+]' "$OUT/diff.txt") lines"
fi
