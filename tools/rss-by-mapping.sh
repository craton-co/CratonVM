#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
#
# tools/rss-by-mapping.sh -- resident set of a process BY OWNER, from
# /proc/<pid>/smaps (Linux only). zr z7/rs (2026-10-13), for
# docs/known-issues/footprint/boot-rss-by-owner-20261013.md.
#
# Runs a command (or inspects a running pid / a saved smaps file), takes one
# snapshot of /proc/<pid>/smaps, and prints the resident set summed per mapping
# class: the binary's text / rodata / relocated data, shared libraries, the
# JDK runtime image (lib/modules), CDS archives, jars, the Java heap, the GC's
# bitmaps, the JDK's jmods, anonymous executable memory (JIT code), thread
# stacks (an anonymous rw mapping right above a <= 1 MiB PROT_NONE guard), brk, and the
# remaining anonymous memory (the allocator: class metadata, interned tables,
# JIT compiler working memory ...).
#
# The snapshot is taken
#   * when a line matching the marker (-m, default `zr7rs-hold ready`, printed by
#     tools/probes/Zr7RsBoot.java) appears on the command's stdout or stderr --
#     a stable point, the probe then sleeps; or
#   * otherwise at the highest VmRSS seen while polling every -i seconds
#     (a short-lived command's peak can fall between two polls: compare the
#     printed vmhwm_kb, which is the kernel's own peak and what
#     `/usr/bin/time %M` reports, with the snapshot's vmrss_kb).
#
# CratonVM ZGC: with CRATONVM_GC_STATS=1 the VM prints
#   [GC] zgc-footprint-spans: arena=0xLO-0xHI start_bits=.. mark_bits=.. old_bits=.. arena_no_thp=N at=init
# on stderr at heap construction. The script then counts the resident pages of
# each range from /proc/<pid>/pagemap at the snapshot (exact, even where the
# kernel merged the range's mapping with an allocator neighbour) and splits the
# anonymous total into gc:java-heap, gc:bitmaps and the rest. Without the line
# (HotSpot, or no CRATONVM_GC_STATS) the heap is found by name (HotSpot ZGC's
# `memfd:java_heap`) or not at all.
#
# Usage:
#   tools/rss-by-mapping.sh [options] -- CMD [ARGS...]
#   tools/rss-by-mapping.sh [options] -p PID
#   tools/rss-by-mapping.sh [options] -s SMAPS_FILE      (no pagemap counts)
# Options:
#   -m REGEX  snapshot marker (default 'zr7rs-hold ready'; '' = peak only)
#   -i SECS   poll interval (default 0.05)
#   -o DIR    keep the raw files (smaps, status, stdout, stderr) in DIR
#   -g FILE   read the spans line from FILE instead of the command's stderr
#   -e PATH   the executable to call "exe" (default: /proc/<pid>/exe)
#   -t N      list the N largest mappings (default 12; 0 = none)
#   -l LABEL  a label echoed on every output line (default: the command's
#             basename), so two runs can be joined
# Run the VM DIRECTLY (not under /usr/bin/time or a wrapper script): the pid
# the script watches is the one it started. Its pagemap is readable because it
# is the script's child; with -p on a process that is not, a kernel with
# yama ptrace_scope >= 1 refuses pagemap and the gc spans then read 0.
#
# Output (stdout): a human table, then machine lines
#   rss-summary label=.. source=marker|peak|pid|file vmrss_kb= vmhwm_kb= rss_anon_kb= rss_file_kb= rss_shmem_kb= threads= thp=..
#   rss-class label=.. class=<name> maps=<n> vsz_kb= rss_kb= pss_kb= priv_dirty_kb= anon_huge_kb= swap_kb=
#   rss-gc label=.. span=<arena|start_bits|mark_bits|old_bits> lo= hi= vsz_kb= resident_kb=
#   rss-derived label=.. anon_total_kb= gc_heap_kb= gc_bitmaps_kb= stacks_kb= anon_exec_kb= anon_rest_kb=
#               exe_kb= jimage_kb= jmod_kb= libs_kb=
#   rss-top label=.. rss_kb= <start> <perms> <size> <path> [<class>]   (the -t largest mappings)
# The command's exit status is the script's exit status.

set -u

marker='zr7rs-hold ready'
interval=0.05
outdir=''
spans_file=''
exe_override=''
top=12
label=''
pid=''
smaps_in=''

usage() { awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 2; }

while getopts 'm:i:o:g:e:t:l:p:s:h' opt; do
    case "$opt" in
        m) marker="$OPTARG" ;;
        i) interval="$OPTARG" ;;
        o) outdir="$OPTARG" ;;
        g) spans_file="$OPTARG" ;;
        e) exe_override="$OPTARG" ;;
        t) top="$OPTARG" ;;
        l) label="$OPTARG" ;;
        p) pid="$OPTARG" ;;
        s) smaps_in="$OPTARG" ;;
        *) usage ;;
    esac
done
shift $((OPTIND - 1))
[ "${1:-}" = "--" ] && shift

if [ -z "$pid" ] && [ -z "$smaps_in" ] && [ $# -eq 0 ]; then
    usage
fi
if [ "$(uname -s)" != "Linux" ] && [ -z "$smaps_in" ]; then
    echo "rss-by-mapping: Linux only (/proc/<pid>/smaps)" >&2
    exit 2
fi

keep=1
if [ -z "$outdir" ]; then
    outdir="$(mktemp -d "${TMPDIR:-/tmp}/rss-by-mapping.XXXXXX")" || exit 2
    keep=0
fi
mkdir -p "$outdir" || exit 2
snap="$outdir/smaps.snapshot"
status_snap="$outdir/status.snapshot"
gcres="$outdir/gc-spans.resident"
: > "$gcres"

thp="$(cat /sys/kernel/mm/transparent_hugepage/enabled 2>/dev/null | sed 's/.*\[\(.*\)\].*/\1/')"
[ -n "$thp" ] || thp=unknown

# The spans line, newest first: `name lo hi` per range (hex, no 0x).
read_spans() {
    local f="$1"
    [ -f "$f" ] || return 0
    grep -a '\[GC\] zgc-footprint-spans:' "$f" 2>/dev/null | tail -n 1 |
        tr ' ' '\n' | sed -n 's/^\(arena\|start_bits\|mark_bits\|old_bits\)=0x\([0-9a-f]*\)-0x\([0-9a-f]*\)$/\1 \2 \3/p'
}

# Resident 4 KiB pages among `n` pages from page number `first` of $1's
# address space, from its pagemap (bit 63 = present; without CAP_SYS_ADMIN the
# PFN bits read 0, the flags do not).
count_present() {
    local p="$1" first="$2" n="$3"
    [ "$n" -gt 0 ] || { echo 0; return; }
    dd if="/proc/$p/pagemap" bs=1M iflag=skip_bytes,count_bytes \
        skip=$((first * 8)) count=$((n * 8)) 2>/dev/null |
        od -An -v -t x8 -w8 |
        awk '{ c = substr($1, 1, 1); if (c ~ /[89a-f]/) n++ } END { print n + 0 }'
}

# What each gc span holds, from a smaps snapshot ($1) and the spans ($2,
# `name lohex hihex` lines): a mapping wholly inside a span contributes its
# smaps Rss exactly (`X name kb`); a resident mapping that only overlaps one
# (the kernel merged the range with a neighbour, as an allocator's huge block
# usually is) contributes the overlap's page range for the pagemap count
# (`P name first_page n_pages`). Mappings with no Rss cost nothing.
spans_work() {
    awk -v sf="$2" '
    function hex(s,    i, v) { v = 0; s = tolower(s); sub(/^0x/, "", s)
        for (i = 1; i <= length(s); i++) v = v * 16 + index("0123456789abcdef", substr(s, i, 1)) - 1
        return v }
    function flush(    i, s, a, b) {
        if (!have || r == 0) { have = 0; return }
        for (i = 1; i <= ns; i++) {
            s = nm[i]
            if (lo >= slo[s] && hi <= shi[s]) { printf "X %s %d\n", s, r; continue }
            if (lo < shi[s] && hi > slo[s]) {
                a = (lo > slo[s] ? lo : slo[s]); b = (hi < shi[s] ? hi : shi[s])
                printf "P %s %.0f %.0f\n", s, int(a / 4096), int((b + 4095) / 4096) - int(a / 4096)
            }
        }
        have = 0
    }
    BEGIN { while ((getline l < sf) > 0) { split(l, f, " "); nm[++ns] = f[1]; slo[f[1]] = hex(f[2]); shi[f[1]] = hex(f[3]) } }
    /^[0-9a-f]+-[0-9a-f]+ / { flush(); split($1, a, "-"); lo = hex(a[1]); hi = hex(a[2]); r = 0; have = 1; next }
    $1 == "Rss:" { r = $2 }
    END { flush() }' "$1"
}

# One snapshot of $1: smaps, status, and the gc spans' resident pages
# (`name lohex hihex resident_kb` lines in $gcres).
take_snapshot() {
    local p="$1" errf="$2"
    cat "/proc/$p/smaps" > "$snap.tmp" 2>/dev/null || return 1
    [ -s "$snap.tmp" ] || return 1
    mv "$snap.tmp" "$snap"
    cat "/proc/$p/status" > "$status_snap" 2>/dev/null
    : > "$gcres"
    local f="${spans_file:-$errf}"
    read_spans "$f" | awk '!($2 == "0" && $3 == "0")' > "$outdir/spans.list"
    [ -s "$outdir/spans.list" ] || return 0
    local name lo hi kind a b
    declare -A kb=()
    while read -r name lo hi; do kb[$name]=0; done < "$outdir/spans.list"
    while read -r kind name a b; do
        if [ "$kind" = "X" ]; then
            kb[$name]=$((kb[$name] + a))
        else
            kb[$name]=$((kb[$name] + 4 * $(count_present "$p" "$a" "$b")))
        fi
    done < <(spans_work "$snap" "$outdir/spans.list")
    while read -r name lo hi; do
        echo "$name $lo $hi ${kb[$name]}" >> "$gcres"
    done < "$outdir/spans.list"
    return 0
}

status_kb() { awk -v k="$1:" '$1 == k { print $2; exit }' "$2" 2>/dev/null; }

rc=0
source=''
exe=''
if [ -n "$smaps_in" ]; then
    cp "$smaps_in" "$snap"
    source=file
    exe="$exe_override"
    # No live process: no pagemap, so the spans are listed with resident -1
    # and the gc classes fall back to the mappings wholly inside them.
    [ -n "$spans_file" ] && read_spans "$spans_file" | awk '!($2 == "0" && $3 == "0")' |
        while read -r name lo hi; do
            echo "$name $lo $hi -1" >> "$gcres"
        done
elif [ -n "$pid" ]; then
    exe="${exe_override:-$(readlink "/proc/$pid/exe" 2>/dev/null)}"
    take_snapshot "$pid" "${spans_file:-/dev/null}" || { echo "rss-by-mapping: cannot read /proc/$pid/smaps" >&2; exit 2; }
    source=pid
else
    [ -n "$label" ] || label="$(basename "$1")"
    out="$outdir/stdout"
    err="$outdir/stderr"
    "$@" > "$out" 2> "$err" &
    pid=$!
    best=0
    marked=0
    while kill -0 "$pid" 2>/dev/null; do
        if [ -z "$exe" ]; then
            exe="${exe_override:-$(readlink "/proc/$pid/exe" 2>/dev/null)}"
        fi
        if [ "$marked" -eq 0 ] && [ -n "$marker" ] &&
            grep -aEq "$marker" "$out" "$err" 2>/dev/null; then
            if take_snapshot "$pid" "$err"; then
                marked=1
                source=marker
                cp "$snap" "$snap.marker"
                cp "$status_snap" "$status_snap.marker"
                cp "$gcres" "$gcres.marker"
            fi
        fi
        if [ "$marked" -eq 0 ]; then
            # A new snapshot only when VmRSS grew by 1 MiB or more: a boot
            # grows on almost every poll, and each snapshot copies smaps.
            cur="$(status_kb VmRSS "/proc/$pid/status")"
            if [ -n "$cur" ] && [ "$cur" -ge $((best + 1024)) ]; then
                if take_snapshot "$pid" "$err"; then
                    best="$cur"
                    source=peak
                fi
            fi
        fi
        sleep "$interval"
    done
    wait "$pid"
    rc=$?
    if [ "$marked" -eq 1 ]; then
        mv "$snap.marker" "$snap"
        mv "$status_snap.marker" "$status_snap"
        mv "$gcres.marker" "$gcres"
    fi
    if [ ! -s "$snap" ]; then
        echo "rss-by-mapping: no snapshot (the command exited before the first poll; rc=$rc)" >&2
        [ "$keep" -eq 1 ] || rm -rf "$outdir"
        exit "$rc"
    fi
fi
[ -n "$label" ] || label="${pid:-file}"

vmrss="$(status_kb VmRSS "$status_snap")"
vmhwm="$(status_kb VmHWM "$status_snap")"
ranon="$(status_kb RssAnon "$status_snap")"
rfile="$(status_kb RssFile "$status_snap")"
rshm="$(status_kb RssShmem "$status_snap")"
threads="$(status_kb Threads "$status_snap")"

echo "rss-summary label=$label source=$source vmrss_kb=${vmrss:--1} vmhwm_kb=${vmhwm:--1} rss_anon_kb=${ranon:--1} rss_file_kb=${rfile:--1} rss_shmem_kb=${rshm:--1} threads=${threads:--1} thp=$thp rc=$rc"

awk -v exe="$exe" -v label="$label" -v top="$top" -v gcres="$gcres" '
function hex(s,    i, v, d) {
    v = 0; s = tolower(s); sub(/^0x/, "", s)
    for (i = 1; i <= length(s); i++) {
        d = index("0123456789abcdef", substr(s, i, 1)) - 1
        v = v * 16 + d
    }
    return v
}
function base(p,    n, a) { n = split(p, a, "/"); return a[n] }
function classify(    c) {
    if (path == "[heap]") return "anon:brk"
    if (path == "[stack]") return "stack:main"
    if (path ~ /^\[(vdso|vvar|vsyscall|vvar_vclock)\]$/) return "kernel"
    if (path ~ /memfd:java_heap/) return "java-heap(memfd)"
    if (path != "" && path !~ /^\[anon/) {
        if (exe != "" && path == exe) {
            if (perm ~ /x/) return "exe:text"
            if (perm ~ /w/) return "exe:data"
            return "exe:rodata"
        }
        if (base(path) ~ /^libjvm\.so/) return "lib:jvm"
        if (path ~ /\.so(\.[0-9.]+)?( \(deleted\))?$/) return "libs:other"
        if (path ~ /\/lib\/modules( \(deleted\))?$/) return "jdk:jimage"
        if (path ~ /\.jsa( \(deleted\))?$/) return "jdk:cds"
        if (path ~ /\.jmod( \(deleted\))?$/) return "jdk:jmod"
        if (path ~ /\.jar( \(deleted\))?$/) return "files:jar"
        if (path ~ /^\/memfd:/ || path ~ /^\/dev\/shm\//) return "shmem:other"
        return "files:other"
    }
    # Anonymous.
    if (in_span(lo, hi, "arena") == 2) return "gc:java-heap"
    if (in_span(lo, hi, "start_bits") == 2 || in_span(lo, hi, "mark_bits") == 2 || in_span(lo, hi, "old_bits") == 2) return "gc:bitmaps"
    if (in_span(lo, hi, "arena") == 1 || in_span(lo, hi, "start_bits") == 1 || in_span(lo, hi, "mark_bits") == 1 || in_span(lo, hi, "old_bits") == 1) return "anon:holds-gc-span"
    if (perm ~ /x/) return "anon:exec(jit-code)"
    if (perm ~ /^---/) return "anon:reserved"
    if (perm ~ /^rw/ && prev_anon && prev_perm ~ /^---/ && prev_hi == lo && (prev_hi - prev_lo) <= 1048576) return "anon:thread-stack"
    return "anon:other(malloc...)"
}
# 2 = [lo,hi) inside the named span, 1 = overlaps it, 0 = disjoint.
function in_span(lo, hi, name) {
    if (!(name in slo)) return 0
    if (lo >= slo[name] && hi <= shi[name]) return 2
    if (lo < shi[name] && hi > slo[name]) return 1
    return 0
}
function flush(    c) {
    if (!have) return
    c = classify()
    maps[c]++; vsz[c] += (hi - lo) / 1024; rss[c] += r; pss[c] += ps; pdirty[c] += pd; ahp[c] += ah; swp[c] += sw
    if (path == "" || path ~ /^\[(heap|stack|anon)/ || path ~ /memfd:java_heap/) anon_total += r
    nm++; mrss[nm] = r; mdesc[nm] = sprintf("%s %s %dK %s [%s]", lo_s, perm, (hi - lo) / 1024, (path == "" ? "-" : path), c)
    prev_lo = lo; prev_hi = hi; prev_perm = perm; prev_anon = (path == "")
    have = 0
}
BEGIN {
    while ((getline line < gcres) > 0) {
        split(line, f, " ")
        slo[f[1]] = hex(f[2]); shi[f[1]] = hex(f[3]); shex_lo[f[1]] = f[2]; shex_hi[f[1]] = f[3]; sres[f[1]] = f[4]; sorder[++ns] = f[1]
    }
}
/^[0-9a-f]+-[0-9a-f]+ / {
    flush()
    split($1, a, "-"); lo = hex(a[1]); hi = hex(a[2]); lo_s = a[1]; perm = $2
    path = ""; for (i = 6; i <= NF; i++) path = path (i > 6 ? " " : "") $i
    r = 0; ps = 0; pd = 0; ah = 0; sw = 0; have = 1
    next
}
$1 == "Rss:" { r = $2 }
$1 == "Pss:" { ps = $2 }
$1 == "Private_Dirty:" { pd = $2 }
$1 == "AnonHugePages:" { ah = $2 }
$1 == "Swap:" { sw = $2 }
END {
    flush()
    printf "%-26s %6s %12s %10s %10s %12s %12s %8s\n", "class", "maps", "vsz_kb", "rss_kb", "pss_kb", "priv_dirty_kb", "anon_huge_kb", "swap_kb"
    n = 0
    for (c in maps) cls[++n] = c
    # sort by rss, descending (insertion sort: a few dozen classes)
    for (i = 2; i <= n; i++) { k = cls[i]; j = i - 1; while (j > 0 && rss[cls[j]] < rss[k]) { cls[j + 1] = cls[j]; j-- } cls[j + 1] = k }
    for (i = 1; i <= n; i++) {
        c = cls[i]; trss += rss[c]
        printf "%-26s %6d %12d %10d %10d %12d %12d %8d\n", c, maps[c], vsz[c], rss[c], pss[c], pdirty[c], ahp[c], swp[c]
    }
    printf "%-26s %6s %12s %10d\n", "TOTAL", "", "", trss
    for (i = 1; i <= n; i++) {
        c = cls[i]
        printf "rss-class label=%s class=%s maps=%d vsz_kb=%d rss_kb=%d pss_kb=%d priv_dirty_kb=%d anon_huge_kb=%d swap_kb=%d\n", label, c, maps[c], vsz[c], rss[c], pss[c], pdirty[c], ahp[c], swp[c]
    }
    gheap = -1; gbits = 0; known = 0
    for (i = 1; i <= ns; i++) {
        s = sorder[i]; res = sres[s]
        printf "rss-gc label=%s span=%s lo=0x%s hi=0x%s vsz_kb=%d resident_kb=%s\n", label, s, shex_lo[s], shex_hi[s], (shi[s] - slo[s]) / 1024, res
        if (res >= 0) { known = 1; if (s == "arena") gheap = res; else gbits += res }
    }
    if (!known) { gheap = rss["gc:java-heap"] + rss["java-heap(memfd)"]; gbits = rss["gc:bitmaps"] }
    if (gheap < 0) gheap = 0
    stacks = rss["anon:thread-stack"] + rss["stack:main"]
    aexec = rss["anon:exec(jit-code)"]
    rest = anon_total - gheap - gbits - stacks - aexec
    printf "rss-derived label=%s anon_total_kb=%d gc_heap_kb=%d gc_bitmaps_kb=%d stacks_kb=%d anon_exec_kb=%d anon_rest_kb=%d exe_kb=%d jimage_kb=%d jmod_kb=%d libs_kb=%d\n", label, anon_total, gheap, gbits, stacks, aexec, rest, rss["exe:text"] + rss["exe:rodata"] + rss["exe:data"], rss["jdk:jimage"], rss["jdk:jmod"], rss["libs:other"] + rss["lib:jvm"]
    if (top > 0) {
        # Selection of the `top` largest: O(mappings x top), fine for the
        # thousands of one-method JIT mappings a run can hold.
        for (t = 1; t <= top && t <= nm; t++) {
            b = 0
            for (i = 1; i <= nm; i++) if (!(i in used) && (b == 0 || mrss[i] > mrss[b])) b = i
            used[b] = 1
            printf "rss-top label=%s rss_kb=%d %s\n", label, mrss[b], mdesc[b]
        }
    }
}' "$snap"

if [ "$keep" -eq 0 ]; then
    rm -rf "$outdir"
else
    echo "rss-by-mapping: raw files in $outdir" >&2
fi
exit "$rc"
