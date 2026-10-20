#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""KIND-MAP GATE — a per-registration freeze of `NativeKind`, so an ambient
`set_category` edit cannot re-tag a thousand natives in silence.

The retired `native-kind-is-ambient-and-defaults-to-syntheticstub` write-up
opens with a warning this gate exists to make false:

> **DANGEROUS: causes silent misclassification, not a clean failure, and it
> misclassifies in both directions.**

A native's kind is not stated at its registration site; it is inherited from a
mutable field the *enclosing* registrar happened to set. One
`set_category(Bridge)` line covers 1,350 registrations in `native-collections`
alone. Both directions have already shipped: over-tagging leaves a fake alive
under `--jdk-only` (invisible), and under-tagging drops a permanent bridge and
surfaces minutes later as an unrelated bootstrap `InternalError` (2026-07-14,
`java.util.Properties`).

## Why the aggregate ratchet cannot catch this, in either direction

`scripts/jdk-only-bridge-ratchet.py` freezes two *counts*:
`bridge.without_acc_native` and `bridge.shadows_bytecode`, slack-free. That
stops the numbers **rising**. Flip one `with_category` line from `Bridge` to
`SyntheticStub` and 1,350 rows leave the `Bridge` population entirely: both
counts fall by four figures, and the bridge ratchet reports **IMPROVED — lock
it in**. The exact 2026-07-14 regression reads as a win.

So this gate asks the complementary question, and only that one:

    for every registration present in BOTH the baseline and the census,
    has its `kind` changed?

## What it does NOT assert, deliberately

* **Not a row count.** Adding a native is not this gate's business — the bridge
  ratchet and `stub_ratchet` own that direction. New rows pass and are
  reported; removed rows pass and are reported.
* **Not a kind's correctness.** Whether a `Bridge` deserves to be one is the
  bridge ratchet's question (does the image declare an `ACC_NATIVE` target?).
  This gate is indifferent to the verdict and pins only *stability*.

The one-way half is adjudication: `kind_stated` and `kind_chosen` may go
`false -> true` freely and **never** `true -> false`. Losing an adjudication is
a regression even when the kind is unchanged, because the next ambient edit
then moves that row silently again.

## `kind_chosen`, and why `kind_stated` could not do this job

`kind_stated` is true only for `register_with_kind` — 699 rows of ~11,900. It
is `false` on every row a deliberate `with_category` scope covers, so it cannot
distinguish "a whole registrar was tagged on purpose" from "nobody ever had an
opinion". `kind_chosen` is the wider column (registry `category_chosen`): true
if anyone chose the kind at all, at the site or in an enclosing scope. It is
what step 3 of the record — *"flip the default last"* — has to drive to zero
`false`, and a census taken by a binary without it is refused here rather than
scored as if every row were chosen.

## Keying

`<jdk-feature>/<os>`, like the bridge ratchet, and for the same two reasons:
the registrars are platform-conditional (`cfg(windows)` blocks in `native-io`,
the process/filesystem families, AWT), and the census carries an image
adjudication that is a statement about one runtime image. A key with no
baseline is a **refusal** (exit 2), never a pass.

The baseline is one file per key — `jdk-only-kind-map-<feature>-<os>.tsv` — and
not one JSON with a key map, because it is ~11,900 lines and a per-key file is
what makes `git diff` on a re-freeze readable. That readability IS the gate's
product: the failure mode being closed is a thousand-row change nobody saw.

## Usage

    python3 scripts/jdk-only-kind-map.py --census census.json --jdk-feature 25
    python3 scripts/jdk-only-kind-map.py --census census.json --jdk-feature 25 \
        --update-baseline --note "L?: reclassified native-collections"
    python3 scripts/jdk-only-kind-map.py --selftest     # hermetic

`regression-suite/bridge-ratchet.sh` is the runner: it takes ONE census and
scores both this gate and the bridge ratchet from it, self-tests first, and
passes `--update-baseline --note` through to both. (This line used to name
`regression-suite/native-kind-map.sh`, which has never existed in the tree.)

That sharing has a consequence worth knowing before you re-freeze: an
`--update-baseline` run REGENERATES this baseline's header from the census,
so any hand-written `# amended:` block in it is replaced. Two such blocks
exist on the linux baseline as of 2026-08-12; if a re-freeze drops them that
is correct, because a re-measured file no longer needs them — but read them
first, they say which rows were derived rather than measured and why.

## Exit codes

  0  pass
  1  THE GATE FIRED — a registration changed kind, or lost an adjudication
  2  refused to adjudicate (no baseline for this key, census missing the
     `kind_chosen` column, mode mismatch, collapsed census) — never a silent pass
  3  a prerequisite is missing
"""

import argparse
import json
import os
import platform
import sys

SCHEMA = 1
# Collapse detector, not a measurement — same reasoning as the bridge ratchet's
# `MIN_TOTAL_ROWS`. A census that lost nine thousand registrations to a wiring
# break has no kind flips either, and would otherwise pass.
MIN_TOTAL_ROWS = 8000
# ...and the same floor expressed against the baseline, which is the shape a
# collapse actually takes here: the surviving rows all still match.
MIN_MATCH_FRACTION = 0.90


def host_os():
    s = platform.system().lower()
    if s.startswith("win"):
        return "windows"
    if s == "darwin":
        return "macos"
    return s or "unknown"


def baseline_path(baseline_dir, jdk_feature, os_name):
    return os.path.join(
        baseline_dir, "jdk-only-kind-map-%s-%s.tsv" % (jdk_feature, os_name)
    )


# --------------------------------------------------------------------- rows


def census_rows(doc):
    """(class, name, descriptor, ordinal) -> (kind, stated, chosen).

    The ordinal disambiguates a triple registered more than once — registration
    is last-write-wins throughout `SharedVm::new` and the census keeps one row
    per `register()` call, in order. Keying on the triple alone would let a
    superseded row and its successor trade kinds invisibly, which is the same
    class of blindness this gate is about.
    """
    rows = {}
    seen = {}
    for r in doc.get("natives", []):
        triple = (r["class"], r["name"], r["descriptor"])
        ordinal = seen.get(triple, 0)
        seen[triple] = ordinal + 1
        rows[triple + (ordinal,)] = (
            r["kind"],
            bool(r["kind_stated"]),
            bool(r["kind_chosen"]),
        )
    return rows


def load_baseline(path):
    meta = {}
    rows = {}
    with open(path, "r", encoding="utf-8") as fh:
        for line in fh:
            line = line.rstrip("\n")
            if not line:
                continue
            if line.startswith("#"):
                if ":" in line:
                    k, v = line[1:].split(":", 1)
                    meta[k.strip()] = v.strip()
                continue
            parts = line.split("\t")
            if len(parts) != 7:
                raise ValueError("malformed baseline row: %r" % line)
            cls, name, desc, ordinal, kind, stated, chosen = parts
            rows[(cls, name, desc, int(ordinal))] = (
                kind,
                stated == "1",
                chosen == "1",
            )
    return meta, rows


def write_baseline(path, rows, meta):
    lines = [
        "# jdk-only kind map baseline — schema %d" % SCHEMA,
        "# THE UNIT IS ONE REGISTRATION. A diff here is a change of kind on a",
        "# native that already existed, which is the thing the record",
        "# native-kind-is-ambient-and-defaults-to-syntheticstub.md says nothing",
        "# could see. Re-freezing without reading the diff defeats the gate.",
    ]
    for k in ("key", "jdk_version", "mode", "workload", "rows", "unchosen", "note"):
        if k in meta:
            lines.append("# %s: %s" % (k, meta[k]))
    lines.append("# class\tname\tdescriptor\tordinal\tkind\tkind_stated\tkind_chosen")
    for key in sorted(rows):
        kind, stated, chosen = rows[key]
        lines.append(
            "%s\t%s\t%s\t%d\t%s\t%s\t%s"
            % (key[0], key[1], key[2], key[3], kind, "1" if stated else "0", "1" if chosen else "0")
        )
    with open(path, "w", encoding="utf-8", newline="\n") as fh:
        fh.write("\n".join(lines) + "\n")


# --------------------------------------------------------------------- gate


def precheck(doc, expect_mode):
    """Refusals. Every one of these would otherwise score as a clean pass."""
    if int(doc.get("schema_version", 0)) < 2:
        return "census schema_version %r; this gate needs 2 or later" % doc.get(
            "schema_version"
        )
    natives = doc.get("natives")
    if not isinstance(natives, list) or not natives:
        return "census carries no `natives` array"
    if "kind_chosen" not in natives[0]:
        return (
            "census has no `kind_chosen` column — it was taken by a binary built "
            "before that column existed. Scoring it would silently treat every "
            "row as adjudicated"
        )
    mode = doc.get("mode")
    if expect_mode and mode != expect_mode:
        return "census mode %r, baseline mode %r" % (mode, expect_mode)
    return None


def gate(current, baseline, meta, out=sys.stdout):
    """0 pass / 1 fired / 2 refused."""
    common = set(current) & set(baseline)
    added = set(current) - set(baseline)
    removed = set(baseline) - set(current)

    if len(current) < MIN_TOTAL_ROWS:
        out.write(
            "REFUSED: census has %d registrations, below the %d collapse floor. "
            "A registry that lost most of its rows has no kind flips either.\n"
            % (len(current), MIN_TOTAL_ROWS)
        )
        return 2
    if baseline and len(common) < MIN_MATCH_FRACTION * len(baseline):
        out.write(
            "REFUSED: only %d of the baseline's %d registrations are present "
            "(%.1f%%, floor %.0f%%). That is a census of something else, not a "
            "ratchet result.\n"
            % (
                len(common),
                len(baseline),
                100.0 * len(common) / max(1, len(baseline)),
                100 * MIN_MATCH_FRACTION,
            )
        )
        return 2

    flips = []
    unstated = []
    unchosen = []
    for key in sorted(common):
        cur_kind, cur_stated, cur_chosen = current[key]
        base_kind, base_stated, base_chosen = baseline[key]
        if cur_kind != base_kind:
            flips.append((key, base_kind, cur_kind))
        if base_stated and not cur_stated:
            unstated.append(key)
        if base_chosen and not cur_chosen:
            unchosen.append(key)

    cur_unchosen = sum(1 for v in current.values() if not v[2])
    cur_stated = sum(1 for v in current.values() if v[1])
    out.write("== kind-map: %s ==\n" % meta.get("key", "?"))
    out.write(
        "  %d registrations   %d matched   %d added   %d removed\n"
        % (len(current), len(common), len(added), len(removed))
    )
    out.write(
        "  kind_stated true on %d   kind_chosen false on %d "
        "(step 3 of the record drives that second number to 0)\n"
        % (cur_stated, cur_unchosen)
    )
    out.write("\n")

    if flips:
        out.write(
            "GATE FIRED: %d registration(s) changed kind. This is the ambient-category\n"
            "defect: one `set_category` edit re-tags everything its scope reaches, and\n"
            "nothing else in the tree would have told you. Read every line:\n"
            % len(flips)
        )
        for key, was, now in flips[:200]:
            out.write("  %s.%s%s [#%d]  %s -> %s\n" % (key[0], key[1], key[2], key[3], was, now))
        if len(flips) > 200:
            out.write("  ... and %d more\n" % (len(flips) - 200))
        out.write(
            "\nIf every one is intended, re-freeze in the SAME change:\n"
            "  python3 scripts/jdk-only-kind-map.py --census <c> --update-baseline "
            "--note \"why\"\n"
        )
    if unstated or unchosen:
        out.write(
            "GATE FIRED: %d registration(s) lost `kind_stated` and %d lost "
            "`kind_chosen`.\nAdjudication is one-way here: a row that someone "
            "decided about must not go\nback to inheriting, or the next ambient "
            "edit moves it in silence again.\n" % (len(unstated), len(unchosen))
        )
        for key in (unstated + unchosen)[:50]:
            out.write("  %s.%s%s [#%d]\n" % (key[0], key[1], key[2], key[3]))

    if flips or unstated or unchosen:
        return 1

    if added or removed:
        out.write(
            "  note: %d added and %d removed registration(s) — not this gate's\n"
            "        question (the bridge ratchet and stub_ratchet own that\n"
            "        direction), reported so a re-freeze is a deliberate act.\n"
            % (len(added), len(removed))
        )
        for key in sorted(added)[:20]:
            out.write("        + %s.%s%s [#%d] %s\n" % (key[0], key[1], key[2], key[3], current[key][0]))
        for key in sorted(removed)[:20]:
            out.write("        - %s.%s%s [#%d] %s\n" % (key[0], key[1], key[2], key[3], baseline[key][0]))
        out.write("\n")

    out.write("KIND-MAP: PASS — no registration changed kind.\n")
    return 0


# ----------------------------------------------------------------- selftest


def _row(cls, name, desc, kind, stated=False, chosen=True):
    return {
        "class": cls,
        "name": name,
        "descriptor": desc,
        "kind": kind,
        "kind_stated": stated,
        "kind_chosen": chosen,
    }


def _synthetic(n=MIN_TOTAL_ROWS + 10, mutate=None):
    rows = [_row("p/C%d" % i, "m", "()V", "bridge") for i in range(n)]
    if mutate:
        mutate(rows)
    return {"schema_version": 3, "mode": "compatible", "natives": rows}


def selftest():
    import io

    results = []

    def check(name, want, doc, base_doc=None, expect_mode="compatible"):
        buf = io.StringIO()
        why = precheck(doc, expect_mode)
        if why is not None:
            code = 2
            buf.write("REFUSED: %s\n" % why)
        else:
            base = census_rows(base_doc if base_doc is not None else _synthetic())
            code = gate(census_rows(doc), base, {"key": "selftest"}, out=buf)
        ok = code == want
        results.append((ok, name, want, code))
        print("  %-4s %-52s (want %d, got %d)" % ("ok" if ok else "FAIL", name, want, code))

    print("== kind-map self-test (hermetic) ==")
    check("unchanged tree passes", 0, _synthetic())

    def flip(rows):
        rows[3]["kind"] = "synthetic-stub"

    check("ONE registration changing kind fails", 1, _synthetic(mutate=flip))

    def mass_flip(rows):
        # The 2026-07-14 shape: a whole registrar's worth, in the direction the
        # aggregate ratchet reports as an improvement.
        for r in rows[:1350]:
            r["kind"] = "synthetic-stub"

    check("a registrar-sized re-tag fails", 1, _synthetic(mutate=mass_flip))

    def add(rows):
        rows.append(_row("p/New", "m", "()V", "bridge"))

    check("a NEW registration passes", 0, _synthetic(mutate=add))

    def drop(rows):
        rows.pop()

    check("a REMOVED registration passes", 0, _synthetic(mutate=drop))

    def lose_stated(rows):
        rows[7]["kind_stated"] = False

    base = _synthetic()
    base["natives"][7]["kind_stated"] = True
    check("losing kind_stated fails", 1, _synthetic(mutate=lose_stated), base_doc=base)

    def gain_stated(rows):
        rows[7]["kind_stated"] = True

    check("GAINING kind_stated passes", 0, _synthetic(mutate=gain_stated))

    def lose_chosen(rows):
        rows[9]["kind_chosen"] = False

    check("losing kind_chosen fails", 1, _synthetic(mutate=lose_chosen))

    check("a collapsed census is refused, not scored", 2, _synthetic(n=12))

    no_col = _synthetic()
    for r in no_col["natives"]:
        del r["kind_chosen"]
    check("a census with no kind_chosen column is refused", 2, no_col)

    schema1 = _synthetic()
    schema1["schema_version"] = 1
    check("a schema-1 census is refused", 2, schema1)

    strict = _synthetic()
    strict["mode"] = "jdk-only"
    check("a mode mismatch is refused", 2, strict)

    # A duplicate triple must not collapse onto its predecessor: the two rows
    # differ by ordinal, so a kind swap between them is still visible.
    def dup_swap(rows):
        rows[0] = _row("p/D", "m", "()V", "bridge")
        rows[1] = _row("p/D", "m", "()V", "synthetic-stub")

    base_dup = _synthetic()
    base_dup["natives"][0] = _row("p/D", "m", "()V", "synthetic-stub")
    base_dup["natives"][1] = _row("p/D", "m", "()V", "bridge")
    check(
        "two registrations of one triple swapping kinds fails",
        1,
        _synthetic(mutate=dup_swap),
        base_doc=base_dup,
    )

    failed = [r for r in results if not r[0]]
    print("\nselftest: %d passed, %d failed" % (len(results) - len(failed), len(failed)))
    return 1 if failed else 0


# --------------------------------------------------------------------- main


def main(argv=None):
    ap = argparse.ArgumentParser(
        description=__doc__.split("\n")[0],
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument("--census")
    ap.add_argument("--baseline-dir", default=None)
    ap.add_argument("--jdk-feature")
    ap.add_argument("--jdk-version", default="")
    ap.add_argument("--os", dest="os_name", default=None)
    ap.add_argument("--workload", default="")
    ap.add_argument("--update-baseline", action="store_true")
    ap.add_argument("--note", default="")
    ap.add_argument("--selftest", action="store_true")
    args = ap.parse_args(argv)

    if args.selftest:
        return selftest()
    if not args.census:
        ap.error("--census is required (or --selftest)")

    with open(args.census, "r", encoding="utf-8") as fh:
        doc = json.load(fh)

    os_name = args.os_name or host_os()
    feature = args.jdk_feature
    if not feature:
        print(
            "REFUSED: --jdk-feature is required. The baseline is keyed by JDK "
            "feature version and guessing it scores against the wrong image.",
            file=sys.stderr,
        )
        return 2
    key = "%s/%s" % (feature, os_name)

    bdir = args.baseline_dir or os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "baselines"
    )
    path = baseline_path(bdir, feature, os_name)

    current = census_rows(doc)
    meta = {
        "key": key,
        "jdk_version": args.jdk_version or doc.get("jdk_version", ""),
        "mode": doc.get("mode", ""),
        "workload": args.workload,
        "rows": str(len(current)),
        "unchosen": str(sum(1 for v in current.values() if not v[2])),
        "note": args.note,
    }

    if args.update_baseline:
        why = precheck(doc, None)
        if why:
            print("REFUSED to freeze: %s" % why, file=sys.stderr)
            return 2
        if len(current) < MIN_TOTAL_ROWS:
            print(
                "REFUSED to freeze: %d registrations, below the %d collapse floor."
                % (len(current), MIN_TOTAL_ROWS),
                file=sys.stderr,
            )
            return 2
        if not args.note:
            print(
                "REFUSED to freeze: --note is required. A baseline whose diff has "
                "no stated reason is the silence this gate replaces.",
                file=sys.stderr,
            )
            return 2
        os.makedirs(bdir, exist_ok=True)
        write_baseline(path, current, meta)
        print("froze %d registrations for %s -> %s" % (len(current), key, path))
        return 0

    if not os.path.exists(path):
        print(
            "REFUSED: no kind-map baseline for %s (looked for %s).\n"
            "         Take one with --update-baseline --note \"...\" from a tree "
            "you trust;\n         scoring against another platform's map would "
            "report flips that are\n         just a different set of registrars."
            % (key, path),
            file=sys.stderr,
        )
        return 2

    bmeta, baseline = load_baseline(path)
    why = precheck(doc, bmeta.get("mode") or None)
    if why:
        print("REFUSED: %s" % why, file=sys.stderr)
        return 2
    return gate(current, baseline, {"key": key})


if __name__ == "__main__":
    sys.exit(main())
