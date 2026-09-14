#!/usr/bin/env python3
"""verify-compare - the one diff command for VERIFY-01 app-suite baselines.

Reads two per-class results files (a checked-in baseline and a fresh run,
in either order the caller finds natural) and prints three sets: regressed,
fixed, still-failing. Nothing else - no green/red summary, no wall-clock
judgement.

Accepts the native output of any of the app-suite runners in apps/, not
just the canonical 2-column baseline.tsv, by sniffing column layout. See
--help and docs/internal or scripts/verify-README.md for the format list
and the rationale (docs/known-issues/c2/verify-01-differential-harness.md
before it was retired).
"""
import argparse
import csv
import sys
from collections import OrderedDict

# Status vocabulary observed across the suite runners as of 2026-08. HANG is
# split out from the other non-PASS statuses deliberately: a PASS->HANG
# delta is a timeout artifact until it's confirmed by a rerun at a longer
# per-class timeout on a quiet host (see RESULTS-20260721-hang-rootcause.md
# for the H2 case this is modeled on), not a regression to act on directly.
HANG_STATUSES = {"HANG", "TIMEOUT"}
PASS_STATUSES = {"PASS", "OK"}

# (field_count, separator) -> (class_index, status_index, label), for the
# no-header native formats. Add a row here rather than teaching a suite
# runner to emit a different shape - the whole point of this tool is to
# not require that.
KNOWN_SHAPES = {
    (2, "\t"): (0, 1, "canonical baseline (class\\tstatus)"),
    (4, ","): (0, 3, "tomcat-style CSV (class,rc,secs,status)"),
    # Tomcat's runner grew a 5th column (the host's 1-minute load average as
    # the class finished) on 2026-08-23, so both widths are live: old result
    # files keep the 4-field shape and are still read. The class and status
    # indices are the same either way - the column was appended, not inserted.
    (5, ","): (0, 3, "tomcat-style CSV (class,rc,secs,status,loadavg1)"),
    (9, "\t"): (1, 2, "h2-style TSV (idx,class,status,rc,ms,tests,mode,log,note)"),
}

CLASS_NAMES = {"class", "cls", "classname", "test", "testclass"}
STATUS_NAMES = {"status", "result"}


def sniff_sep(line):
    return "\t" if "\t" in line else ","


def read_results(path, class_col=None, status_col=None, sep=None):
    """Returns OrderedDict[class] = status (uppercased), last row wins."""
    with open(path, "r", encoding="utf-8", errors="replace") as f:
        raw_lines = [ln.rstrip("\n").rstrip("\r") for ln in f]
    lines = [ln for ln in raw_lines if ln.strip() and not ln.lstrip().startswith("#")]
    if not lines:
        die(f"{path}: empty (no data rows)")

    first = lines[0]
    the_sep = sep or sniff_sep(first)
    header_fields = [c.strip().lower() for c in first.split(the_sep)]

    ci, si = class_col, status_col
    start = 0
    if ci is None or si is None:
        # Try a real header row first (works for any column count/order -
        # this is the escape hatch for suite formats not in KNOWN_SHAPES).
        name_to_idx = {name: i for i, name in enumerate(header_fields)}
        found_class = next((name_to_idx[n] for n in CLASS_NAMES if n in name_to_idx), None)
        found_status = next((name_to_idx[n] for n in STATUS_NAMES if n in name_to_idx), None)
        if found_class is not None and found_status is not None:
            ci, si = found_class, found_status
            start = 1
        else:
            shape = KNOWN_SHAPES.get((len(header_fields), the_sep))
            if shape is None:
                die(
                    f"{path}: can't identify class/status columns "
                    f"({len(header_fields)} {the_sep!r}-separated fields, no recognized header). "
                    f"Pass --class-col/--status-col/--sep explicitly."
                )
            ci, si, _label = shape

    data = OrderedDict()
    order_hint = []
    for ln in lines[start:]:
        fields = ln.split(the_sep)
        if len(fields) <= max(ci, si):
            continue  # malformed row (e.g. a note field with an embedded separator) - skip, don't crash the diff
        cls = fields[ci].strip()
        status = fields[si].strip().upper()
        if not cls:
            continue
        if cls not in data:
            order_hint.append(cls)
        data[cls] = status  # last write wins - resumable runners append, later rows are the retry
    return data, order_hint


def die(msg):
    print(f"verify-compare: error: {msg}", file=sys.stderr)
    sys.exit(2)


def bucket(status):
    if status in PASS_STATUSES:
        return "PASS"
    if status in HANG_STATUSES:
        return "HANG"
    return "OTHER"


def fmt_list(pairs, show_transition):
    out = []
    for cls, old, new in pairs:
        out.append(f"  {cls}\t{old} -> {new}" if show_transition else f"  {cls}")
    return "\n".join(out)


def main():
    ap = argparse.ArgumentParser(
        description="Diff two app-suite results files: regressed / fixed / still-failing.",
        epilog="Refuses to print a single pass/fail verdict - see the class-level sets below.",
    )
    ap.add_argument("baseline", help="checked-in baseline (or any earlier run) - with --emit-baseline, the sole source file")
    ap.add_argument("current", nargs="?", default=None, help="fresh run to compare against the baseline (omit with --emit-baseline)")
    ap.add_argument("--class-col", type=int, default=None, help="0-based column index for the class name (overrides sniffing)")
    ap.add_argument("--status-col", type=int, default=None, help="0-based column index for the status (overrides sniffing)")
    ap.add_argument("--sep", default=None, help="field separator override (default: sniff tab vs comma)")
    ap.add_argument("--quiet", action="store_true", help="print counts only, not class lists")
    ap.add_argument(
        "--emit-baseline",
        action="store_true",
        help="read the 'baseline' argument in any known native format and print the canonical "
             "class<TAB>status baseline.tsv to stdout, sorted by class - the item-1 half of "
             "verify-01 (a suite's raw results.tsv/csv is not itself the checked-in artifact; "
             "this is how you produce that artifact from a run)",
    )
    args = ap.parse_args()

    if args.emit_baseline:
        if args.current is not None:
            die("--emit-baseline takes a single source file, not two")
        src, _order = read_results(args.baseline, args.class_col, args.status_col, args.sep)
        for cls in sorted(src):
            print(f"{cls}\t{src[cls]}")
        return

    if args.current is None:
        die("current run file is required (or pass --emit-baseline with just one file)")

    base, base_order = read_results(args.baseline, args.class_col, args.status_col, args.sep)
    cur, cur_order = read_results(args.current, args.class_col, args.status_col, args.sep)

    all_classes = list(OrderedDict.fromkeys(base_order + cur_order))

    regressed_hard = []   # PASS -> FAIL/CRASH/etc (not HANG)
    regressed_hang = []   # PASS -> HANG (rerun at a longer timeout before trusting this)
    fixed = []             # not-PASS -> PASS
    still_failing = []     # not-PASS -> not-PASS (transition shown)
    missing = []           # in baseline, absent from current
    new = []                # in current, absent from baseline

    for cls in all_classes:
        in_base = cls in base
        in_cur = cls in cur
        if in_base and not in_cur:
            missing.append(cls)
            continue
        if in_cur and not in_base:
            new.append((cls, cur[cls]))
            continue
        b, c = base[cls], cur[cls]
        bb, cb = bucket(b), bucket(c)
        if bb == "PASS" and cb == "PASS":
            continue
        if bb == "PASS" and cb == "HANG":
            regressed_hang.append((cls, b, c))
        elif bb == "PASS" and cb == "OTHER":
            regressed_hard.append((cls, b, c))
        elif bb != "PASS" and cb == "PASS":
            fixed.append((cls, b, c))
        else:
            still_failing.append((cls, b, c))

    print(f"baseline: {args.baseline}  ({len(base)} classes)")
    print(f"current:  {args.current}  ({len(cur)} classes)")
    print()

    sections = [
        ("REGRESSED (was PASS, now failing)", regressed_hard, True),
        ("REGRESSED-HANG (was PASS, now HANG - confirm with a longer per-class timeout on an idle host before treating as real; see verify-01)", regressed_hang, True),
        ("FIXED (was not PASS, now PASS)", fixed, True),
        ("STILL-FAILING (not PASS in both)", still_failing, True),
        ("MISSING (in baseline, absent from current run)", [(c, "", "") for c in missing], False),
        ("NEW (in current run, absent from baseline)", [(c, "", s) for c, s in new], True),
    ]

    for title, rows, show_transition in sections:
        print(f"== {title}: {len(rows)} ==")
        if rows and not args.quiet:
            print(fmt_list(rows, show_transition))
        print()

    hard_regression = len(regressed_hard) > 0 or len(missing) > 0
    sys.exit(1 if hard_regression else 0)


if __name__ == "__main__":
    main()
