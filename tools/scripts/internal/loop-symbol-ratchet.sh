#!/usr/bin/env bash
# The code half of the dispatch-loop layout ratchet (proposal
# `i55-L7-proposal-a-layout-ratchet-for-the-dispatch-loop-and-its-hot-structs`,
# interpreter round i2 wave 56, lane L7; the data half is the crate test
# `vm/src/runtime/layout_ratchet_tests.rs`).
#
# Three times a wave made `--nojit` loop and door rows 15-25% slower with no
# source change to the loop: fat LTO's inliner flipped an `#[inline]`-hinted
# callee in or out of `execute_frame_from_index` when unrelated code grew,
# which changes the loop's size, its stack frame (spill slots) and register
# pressure. This prints, for each binary, the size of the loop's symbol (and
# of the out-of-line arms named below) and the immediate of the first
# `sub $N,%rsp` in its prologue -- the stack frame -- so a bench summary can
# carry the two numbers next to the previous wave's and a regression arrives
# with its first suspect named.
#
# Usage (Linux, binutils `nm` / `objdump`, a fat-LTO `cratonvm` WITH symbols):
#   tools/scripts/internal/loop-symbol-ratchet.sh BASE_BINARY [WAVE_BINARY]
# With two binaries it also prints the deltas and exits 1 when a symbol's
# size moved by more than LOOP_RATCHET_SIZE_PCT percent (default 3), its
# stack frame changed, or its copy count changed. LOOP_RATCHET_SYMBOLS
# overrides the symbol list (space-separated last path components of the
# demangled names).
#
# Reading it: a size or frame change on a wave that did not touch the loop is
# the inliner. The lane that grew a callee pins it (`#[inline(never)]` /
# `#[inline(always)]`, as waves 47-51 did for `op_checkcast`,
# `cast_site_hit_admits` and `site_stats::bump`).

set -u

SYMBOLS=${LOOP_RATCHET_SYMBOLS:-"execute_frame_from_index op_checkcast op_instanceof execute_instruction"}
SIZE_PCT=${LOOP_RATCHET_SIZE_PCT:-3}

if [ $# -lt 1 ] || [ $# -gt 2 ]; then
    echo "usage: $0 BASE_BINARY [WAVE_BINARY]" >&2
    exit 2
fi
for tool in nm objdump awk; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "$0: needs $tool" >&2
        exit 2
    fi
done

# One line per symbol: "<sym> <size> <frame> <copies>". `size` is the
# largest copy's (an LTO build can keep more than one), `frame` the first
# `sub $N,%rsp` immediate in that copy's first 512 bytes (0 if none), and
# `copies` how many text definitions matched. Plain POSIX awk (mawk), no
# gawk extensions; hex is converted by the shell.
measure() {
    local bin=$1 sym a s addr best copies end imm frame defs
    defs=$(nm -S --defined-only --demangle "$bin" 2>/dev/null)
    for sym in $SYMBOLS; do
        best=0
        addr=""
        copies=0
        while read -r a s; do
            [ -n "$a" ] || continue
            copies=$((copies + 1))
            if [ $((16#$s)) -gt "$best" ]; then
                best=$((16#$s))
                addr=$a
            fi
        done < <(printf '%s\n' "$defs" | awk -v want="$sym" '
            NF >= 4 && $3 ~ /^[tT]$/ {
                name = $0
                sub(/^[^ ]+ +[^ ]+ +[^ ]+ +/, "", name)
                sub(/\.llvm\.[0-9]+$/, "", name)
                sub(/::h[0-9a-f]+$/, "", name)
                n = split(name, parts, "::")
                if (parts[n] == want) print $1, $2
            }')
        if [ "$copies" -eq 0 ]; then
            echo "$sym 0 0 0"
            continue
        fi
        end=$(printf '0x%x' $((16#$addr + 512)))
        imm=$(objdump -d --no-show-raw-insn --start-address="0x$addr" \
                --stop-address="$end" "$bin" 2>/dev/null \
            | awk 'match($0, /sub +\$0x[0-9a-f]+,%rsp/) {
                    imm = substr($0, RSTART, RLENGTH)
                    sub(/^sub +\$0x/, "", imm)
                    sub(/,%rsp$/, "", imm)
                    print imm
                    exit
                }')
        frame=0
        if [ -n "$imm" ]; then
            frame=$((16#$imm))
        fi
        echo "$sym $best $frame $copies"
    done
}

print_table() {
    printf '%-28s %10s %8s %6s   %s\n' symbol size frame copies "$2"
    printf '%s\n' "$1" | while read -r sym size frame copies; do
        printf '%-28s %10d %8d %6d\n' "$sym" "$size" "$frame" "$copies"
    done
}

BASE=$(measure "$1")
print_table "$BASE" "$1"
[ $# -eq 2 ] || exit 0

WAVE=$(measure "$2")
echo
print_table "$WAVE" "$2"
echo

status=0
while read -r sym bsize bframe bcopies && read -r _ wsize wframe wcopies <&3; do
    note=""
    if [ "$bsize" -gt 0 ]; then
        permille=$(( (wsize - bsize) * 1000 / bsize ))
        if [ "${permille#-}" -gt $((SIZE_PCT * 10)) ]; then
            note="$note size $(awk -v p="$permille" 'BEGIN { printf "%+.1f%%", p / 10 }')"
        fi
    elif [ "$wsize" -gt 0 ]; then
        note="$note appeared"
    fi
    if [ "$bframe" -ne "$wframe" ]; then
        note="$note frame $bframe->$wframe"
    fi
    if [ "$bcopies" -ne "$wcopies" ]; then
        note="$note copies $bcopies->$wcopies"
    fi
    if [ -n "$note" ]; then
        echo "MOVED $sym:$note"
        status=1
    else
        echo "same  $sym (size $bsize->$wsize, frame $bframe)"
    fi
done <<<"$BASE" 3<<<"$WAVE"
exit $status
