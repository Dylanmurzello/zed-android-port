#!/bin/sh
# Checks the loader ELF has the shape the rootfs engine depends on. The
# seccomp filter allows syscalls only from a fixed address window, so a
# PIE, a dynamic interpreter, or a different image base would make every
# guest process die at its first path syscall.
#
# usage: verify.sh <ndk-dir> <loader-binary>
set -eu

ndk="$1"
bin="$2"
image_base=0x5400000000
window_end=0x5400400000

readelf=""
for candidate in "$ndk"/toolchains/llvm/prebuilt/*/bin/llvm-readelf; do
    if [ -x "$candidate" ]; then
        readelf="$candidate"
        break
    fi
done
if [ -z "$readelf" ]; then
    echo "verify-zd-loader: llvm-readelf not found under $ndk" >&2
    exit 1
fi

header="$("$readelf" -h "$bin")"
segments="$("$readelf" -l -W "$bin")"

fail() {
    echo "verify-zd-loader: $1" >&2
    echo "$header" >&2
    echo "$segments" >&2
    exit 1
}

echo "$header" | grep -q 'Class: *ELF64' || fail "not ELF64"
echo "$header" | grep -q 'Type: *EXEC' || fail "not ET_EXEC (expected a static non-PIE image)"
echo "$header" | grep -q 'Machine: *AArch64' || fail "not AArch64"
if echo "$segments" | grep -q '^ *INTERP'; then fail "has a PT_INTERP segment"; fi
if echo "$segments" | grep -q '^ *DYNAMIC'; then fail "has a PT_DYNAMIC segment"; fi

first_load="$(echo "$segments" | awk '$1 == "LOAD" { print $3; exit }')"
[ "$first_load" = "0x0000005400000000" ] || fail "first LOAD at $first_load, expected 0x0000005400000000"

if echo "$segments" | awk '$1 == "LOAD" && $NF != "0x4000" { bad = 1 } END { exit bad }'; then :; else
    fail "a LOAD segment is not 16 KiB aligned"
fi

if echo "$segments" | awk '$1 == "GNU_STACK" { found = 1; if ($(NF - 1) != "RW") bad = 1 } END { exit (found && !bad) ? 0 : 1 }'; then :; else
    fail "GNU_STACK missing or executable"
fi

entry="$(echo "$header" | awk '/Entry point address/ { print $NF }')"
entry_dec=$(printf '%d' "$entry")
if [ "$entry_dec" -lt "$(printf '%d' $image_base)" ] || [ "$entry_dec" -ge "$(printf '%d' $window_end)" ]; then
    fail "entry point $entry is outside the loader window"
fi

max_end=0
for pair in $(echo "$segments" | awk '$1 == "LOAD" { print $3 "," $6 }'); do
    vaddr=$(printf '%d' "${pair%,*}")
    memsz=$(printf '%d' "${pair#*,}")
    end=$((vaddr + memsz))
    if [ "$end" -gt "$max_end" ]; then max_end=$end; fi
done
[ "$max_end" -le "$(printf '%d' $window_end)" ] || fail "image extends past the loader window"

size=$(wc -c < "$bin")
[ "$size" -lt 1048576 ] || fail "binary is $size bytes, expected under 1 MiB"

echo "verify-zd-loader: ok ($size bytes, entry $entry)"
