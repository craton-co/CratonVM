#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""BRIDGE-RATCHET GATE — a one-way ratchet on unadjudicated `Bridge` natives.

Contract §1.5 defines a `Bridge` as what an `ACC_NATIVE` method binds to. The
schema-3 census (`--dump-native-registry` with `--explain-jdk-only`) makes
"does this registration's target actually carry `ACC_NATIVE` in the image?" a
machine-readable fact, and the answer measured on JDK 25 is that **10,084 of
10,844 `Bridge` registrations have no `ACC_NATIVE` target**. Nothing stopped
that number rising. This pins it.

Modelled on `native-builtins/tests/stub_ratchet.rs`: an exact baseline with
`SLACK = 0`, plus a vacuity floor so an empty census cannot pass.

## Which of the two hosting options this is, and why

`L6-unadjudicated-bridge-ratchet-DONE-20260805.md` (the lane brief,
retired when this landed) offers two, and asks that the choice be stated here:

* **Regression-suite gate (CHOSEN).** The census needs a real JDK image at
  measurement time, which a `cargo test` unit test does not have. The gate
  therefore runs where a JDK is available — `regression-suite/bridge-ratchet.sh`
  boots the VM, takes the census and calls this script — and compares against
  the baseline committed beside it in `scripts/baselines/`.
* Committed census artefact. Rejected: an 11,909-row census keyed to one JDK
  build is a snapshot that rots, and a rotting baseline is worse than none.

Because the baseline is *taken* rather than *shipped*, it is keyed by
**`<jdk-feature>/<os>`**. A census this file has no baseline for is a
**refusal** (exit 2), never a pass.

Both halves of that key earn their place. `image_declaring_method` is a
statement about one runtime image, so scoring JDK 21's image against JDK 25's
numbers would report a ratchet result for a question nobody asked. And the
registrars are platform-conditional — `native-io`, the process and filesystem
families and the AWT cluster all register different sets under `cfg(windows)`
— so a Linux baseline scoring a Windows census is the same "silently combines
two different worlds" defect that `scripts/jdk-only-census.sh`'s header exists
to not repeat.

## What it asserts

1. `bridge.without_acc_native <= baseline` with `SLACK = 0` — the ratchet.
2. `bridge.shadows_bytecode <= baseline` with `SLACK = 0`. The same ratchet on
   the subgroup that has already produced a defect: a `Bridge` shadowing
   concrete bytecode can reach §7 step 3's decline, which used to fall through
   to `UnsatisfiedLinkError` instead of to the bytecode (that is how
   `--jdk-only` came to be unable to start a thread). 4,796 rows can reach it.
3. `total_rows >= MIN_TOTAL_ROWS` — a **collapse detector, not a measurement**.
   Same reasoning as `essential_registry_is_populated` in `stub_ratchet.rs`:
   the ratchet is a ratio argument and the denominator was never asserted, so a
   wiring break that dropped nine thousand registrations would leave the
   numerator green. Do not cite this floor as a fact about the registry size.

A count that goes *down* passes and prints a re-freeze instruction. It is not
auto-lowered: the ratchet is slack-free by design, and silently absorbing an
improvement would re-admit exactly that many new unadjudicated rows.

## Usage

    # take the census (see regression-suite/bridge-ratchet.sh, which does this)
    cratonvm --real-jdk --java-home $JDK --explain-jdk-only \
        --dump-native-registry census.json -cp <probe-classes> JdkOnlyCensusLoadProbe

    # gate it
    python3 scripts/jdk-only-bridge-ratchet.py --census census.json --jdk-feature 25

    # re-freeze after a change that legitimately moves the number
    python3 scripts/jdk-only-bridge-ratchet.py --census census.json --jdk-feature 25 \
        --update-baseline --note "L5: native-io migrated to register_with_kind"

    # the machine-readable block on its own (step 1 of the lane doc)
    python3 scripts/jdk-only-bridge-ratchet.py --census census.json --emit-json -

    # hermetic — no JDK, no VM, no baseline file touched
    python3 scripts/jdk-only-bridge-ratchet.py --selftest

## Exit codes

    0  the gate passed
    1  THE GATE FIRED — an unadjudicated Bridge registration was added
    2  refused to adjudicate (see the message; never a silent pass)
    3  usage / I/O error
"""
import argparse
import json
import os
import sys
from collections import Counter

# Schema of the machine-readable block this script emits and of the baseline
# file it reads. Bumped only when a *consumer* would misread the old shape.
BLOCK_SCHEMA = 1

# The census schema that carries `image_declaring_method`. Anything else cannot
# answer the question this gate asks.
REQUIRED_CENSUS_SCHEMA = 3

# Slack on top of the observed count when freezing a baseline. Zero, and it
# stays zero: see `stub_ratchet.rs`'s SLACK for the same argument.
SLACK = 0

# Vacuity floor. A COLLAPSE DETECTOR, NOT A MEASUREMENT — see the module
# docstring. Sits well below the live count (11,909 on JDK 25, 2026-08-05) so
# ordinary churn does not trip it, but a wiring break does.
MIN_TOTAL_ROWS = 8_000

DEFAULT_BASELINE = os.path.join(
    os.path.dirname(os.path.abspath(__file__)), "baselines", "jdk-only-bridge-ratchet.json"
)

# The two counts under ratchet, in the order they are reported. `(block key,
# baseline key, human name)`.
def host_os():
    """The baseline key's OS half, from the interpreter that is running.

    This script runs in the same process tree as the VM that produced the
    census (see `regression-suite/bridge-ratchet.sh`), so the interpreter's
    platform IS the census's platform. Override with `--os` only when scoring
    a census file carried over from another machine.
    """
    if sys.platform.startswith("win") or sys.platform == "cygwin":
        return "windows"
    if sys.platform == "darwin":
        return "macos"
    if sys.platform.startswith("linux"):
        return "linux"
    return sys.platform


RATCHETS = (
    ("without_acc_native", "bridge_without_acc_native",
     "Bridge registrations with no ACC_NATIVE target"),
    ("shadows_bytecode", "bridge_shadows_bytecode",
     "Bridge registrations shadowing concrete bytecode"),
)


def _img(row):
    """The image verdict for a row, as a dict.

    `image_declaring_method` is `null` when the adjudication pass did not run.
    Callers must have checked `image_adjudication` first — this returning `{}`
    would otherwise score every row as "class absent", which reads exactly like
    a clean result.
    """
    return row.get("image_declaring_method") or {}


def adjudicate(doc):
    """Reduce a schema-3 census to the machine-readable adjudication block.

    The five `bridge` buckets are disjoint and sum to `bridge.rows`. They are
    disjoint by construction, not by convention: the emitter derives `has_code`
    as `!native && !abstract` (JVMS §4.6), so `acc_native` and `has_code` can
    never both be true, and `abstract` is what is left once both are false.
    """
    rows = doc.get("natives") or []
    kinds = Counter(r.get("kind") for r in rows)

    bridges = [r for r in rows if r.get("kind") == "bridge"]
    acc_native = shadow = abstract_ = undeclared = absent = 0
    for row in bridges:
        img = _img(row)
        if not img.get("image_has_class"):
            absent += 1
        elif not img.get("declared"):
            undeclared += 1
        elif img.get("acc_native"):
            acc_native += 1
        elif img.get("has_code"):
            shadow += 1
        else:
            abstract_ += 1

    buckets = (acc_native, shadow, abstract_, undeclared, absent)
    if sum(buckets) != len(bridges):  # pragma: no cover - structural invariant
        raise AssertionError(
            f"bridge buckets {buckets} sum to {sum(buckets)}, not {len(bridges)} — "
            "the census emitter changed the meaning of image_declaring_method"
        )

    return {
        "schema": BLOCK_SCHEMA,
        "census_schema_version": doc.get("schema_version"),
        "mode": doc.get("mode"),
        "image_adjudication": bool(doc.get("image_adjudication")),
        "partial": bool(doc.get("partial", False)),
        "total_rows": len(rows),
        "registrations": {
            "intrinsic": kinds.get("intrinsic", 0),
            "bridge": kinds.get("bridge", 0),
            "synthetic-stub": kinds.get("synthetic-stub", 0),
        },
        "bridge": {
            "rows": len(bridges),
            "acc_native": acc_native,
            "shadows_bytecode": shadow,
            "abstract_method": abstract_,
            "class_present_method_undeclared": undeclared,
            "class_absent": absent,
            "without_acc_native": len(bridges) - acc_native,
        },
    }


def render_block(block):
    """The lane doc's table, as text, from the block."""
    b = block["bridge"]
    rows = b["rows"] or 1  # only ever a divisor
    lines = [
        f"  census schema {block['census_schema_version']}  mode {block['mode']}  "
        f"rows {block['total_rows']}",
        f"  registrations   {block['registrations']}",
        "",
        f"  {'what the image says about the target':<48}{'rows':>7}{'share':>8}",
    ]
    for label, key in (
        ("ACC_NATIVE — a genuine bridge (§1.5)", "acc_native"),
        ("concrete bytecode — a shadow", "shadows_bytecode"),
        ("abstract method — intercepts every implementor", "abstract_method"),
        ("class present, method not declared", "class_present_method_undeclared"),
        ("class absent from the image", "class_absent"),
    ):
        n = b[key]
        lines.append(f"  {label:<48}{n:>7}{100.0 * n / rows:>7.0f}%")
    lines.append(
        f"  {'BRIDGE rows with no ACC_NATIVE target':<48}"
        f"{b['without_acc_native']:>7}{100.0 * b['without_acc_native'] / rows:>7.0f}%"
    )
    return "\n".join(lines)


def load_baseline(path):
    try:
        with open(path, encoding="utf-8") as fh:
            return json.load(fh)
    except FileNotFoundError:
        return {"schema": BLOCK_SCHEMA, "slack": SLACK, "min_total_rows": MIN_TOTAL_ROWS, "jdk": {}}


def baseline_key(jdk_feature, os_name):
    return f"{jdk_feature}/{os_name}"


def gate(block, baseline, jdk_feature, os_name):
    """Score `block` against `baseline`. Returns `(exit_code, [lines])`.

    Every refusal path returns 2 and says what is missing. None of them returns
    0: a gate that cannot answer must not report the answer it would like.
    """
    out = []
    key = baseline_key(jdk_feature, os_name)

    if block["census_schema_version"] != REQUIRED_CENSUS_SCHEMA:
        return 2, [
            f"REFUSING: census schema_version is {block['census_schema_version']}, not "
            f"{REQUIRED_CENSUS_SCHEMA}. Only schema {REQUIRED_CENSUS_SCHEMA} carries "
            "image_declaring_method, which is the whole question this gate asks."
        ]
    if not block["image_adjudication"]:
        return 2, [
            "REFUSING: image_adjudication is false — re-run the census with "
            "--explain-jdk-only. Every image_declaring_method is null because the "
            "pass did not run, not because the image lacks the method, and scoring "
            "it would print a table of zeroes that reads exactly like a clean result."
        ]
    if block["partial"]:
        return 2, [
            'REFUSING: the census is marked "partial": true — it was written on a '
            "path that could not read the class store, so its counts describe less "
            "than one booted VM."
        ]

    entry = (baseline.get("jdk") or {}).get(key)
    if entry is None:
        have = ", ".join(sorted((baseline.get("jdk") or {}))) or "(none)"
        return 2, [
            f"REFUSING: no committed baseline for {key} (have: {have}).",
            "  The key is <jdk-feature>/<os> because image_declaring_method is a "
            "statement about ONE runtime image and the registrars are "
            "platform-conditional. Scoring this census against another image's or "
            "another platform's numbers would answer a question nobody asked.",
            f"  Take one:  --census <file> --jdk-feature {jdk_feature} --os {os_name} "
            '--update-baseline --note "<why>"',
        ]
    if entry.get("mode") != block["mode"]:
        return 2, [
            f"REFUSING: the {key} baseline was taken in mode "
            f"{entry.get('mode')!r} but this census is mode {block['mode']!r}. "
            "The two policies register different things; comparing them silently "
            "combines two different worlds.",
        ]

    slack = baseline.get("slack", SLACK)
    floor = baseline.get("min_total_rows", MIN_TOTAL_ROWS)
    failed = False

    # The vacuity floor first: if it trips, every other number below is being
    # reported about a registry that is not there.
    if block["total_rows"] < floor:
        failed = True
        out.append(
            f"COLLAPSE: the census holds only {block['total_rows']} registrations, "
            f"below the {floor} floor. This floor is a collapse detector, not a "
            "measurement — a wiring break that dropped whole registration modules "
            "would make every ratchet below pass for the wrong reason."
        )

    for block_key, base_key, human in RATCHETS:
        observed = block["bridge"][block_key]
        frozen = entry[base_key]
        if observed > frozen + slack:
            failed = True
            out.append(
                f"BRIDGE-RATCHET REGRESSION: {human}: {observed}, exceeding the "
                f"frozen baseline of {frozen} (slack {slack}) by {observed - frozen}."
            )
            out.append(
                "  A change ADDED a Bridge registration whose target the JDK image "
                "does not declare ACC_NATIVE. Contract §1.5 defines a Bridge by that "
                "target. Fix the registration — state its kind with "
                "register_with_kind() and make it SyntheticStub or Intrinsic, or "
                "delete it so the real bytecode runs — do NOT just raise the baseline."
            )
        elif observed < frozen:
            out.append(
                f"IMPROVED: {human}: {observed}, {frozen - observed} below the frozen "
                f"baseline of {frozen}. Lock it in in the SAME change: re-run with "
                "--update-baseline. The ratchet is slack-free, so leaving the baseline "
                f"at {frozen} silently re-admits {frozen - observed} new ones."
            )
        else:
            out.append(f"ok  {human}: {observed} (baseline {frozen}, slack {slack})")

    if not failed:
        out.append(
            f"ok  vacuity floor: {block['total_rows']} rows >= {floor} "
            "(collapse detector, not a measurement)"
        )
    return (1 if failed else 0), out


# ---------------------------------------------------------------------------
# SELF-TEST — the gate shown to fail, hermetically, on every run.
#
# "A guard never shown to fail is decoration": three shipped inert in this
# feature. The lane doc asks for a one-off injection of a real unadjudicated
# Bridge registration (done, and recorded in the lane doc); this is the
# permanent version of the same check, and it needs no JDK, no VM and no build.
# ---------------------------------------------------------------------------

def _row(kind, cls, verdict):
    return {
        "class": cls, "name": "m", "descriptor": "()V", "kind": kind,
        "registered_by": None, "overwrote": None, "invocations": 0,
        "kind_stated": False, "real_declaring_method": None,
        "image_declaring_method": verdict,
    }


_NATIVE = {"image_has_class": True, "declared": True, "acc_native": True, "has_code": False}
_CODE = {"image_has_class": True, "declared": True, "acc_native": False, "has_code": True}
_ABSTRACT = {"image_has_class": True, "declared": True, "acc_native": False, "has_code": False}
_UNDECL = {"image_has_class": True, "declared": False, "acc_native": False, "has_code": False}
_ABSENT = {"image_has_class": False, "declared": False, "acc_native": False, "has_code": False}


def _synthetic_census(extra=()):
    """A miniature census with the same shape as the real one: 10 bridges, of
    which 2 are genuine, plus enough filler to clear the vacuity floor."""
    natives = [_row("bridge", f"p/N{i}", _NATIVE) for i in range(2)]
    natives += [_row("bridge", f"p/C{i}", _CODE) for i in range(4)]
    natives += [_row("bridge", f"p/A{i}", _ABSTRACT) for i in range(2)]
    natives += [_row("bridge", "p/U", _UNDECL), _row("bridge", "p/X", _ABSENT)]
    natives += [_row("intrinsic", f"p/I{i}", _CODE) for i in range(MIN_TOTAL_ROWS)]
    natives += list(extra)
    return {
        "schema_version": 3, "image_adjudication": True, "mode": "compatible",
        "counts": {}, "invocations": {}, "natives": natives,
    }


def _selftest_baseline():
    return {
        "schema": BLOCK_SCHEMA, "slack": 0, "min_total_rows": MIN_TOTAL_ROWS,
        "jdk": {"25/linux": {"mode": "compatible", "os": "linux",
                             "bridge_without_acc_native": 8,
                             "bridge_shadows_bytecode": 4}},
    }


def selftest():
    checks = []

    def check(name, want_code, doc, baseline=None, feature=25, os_name="linux"):
        base = _selftest_baseline() if baseline is None else baseline
        code, lines = gate(adjudicate(doc), base, feature, os_name)
        ok = code == want_code
        checks.append((ok, name, want_code, code, lines))
        return ok

    # 1. The baseline tree passes.
    check("clean tree passes", 0, _synthetic_census())

    # 2. THE INJECTION. One extra Bridge whose target has concrete bytecode --
    #    exactly the shape the lane doc asks to be shown failing. It must trip
    #    BOTH ratchets, because a shadow is also a row without ACC_NATIVE.
    check("injected shadowing Bridge fails", 1,
          _synthetic_census(extra=[_row("bridge", "p/INJECTED", _CODE)]))
    tripped = sum(1 for ln in checks[-1][4] if ln.startswith("BRIDGE-RATCHET REGRESSION"))
    checks.append((tripped == 2, "the shadowing injection trips BOTH ratchets", 2, tripped, []))

    # 3. An injected Bridge onto an ABSENT class trips only the aggregate
    #    ratchet -- proof the two are not the same assertion wearing two names.
    check("injected absent-class Bridge fails", 1,
          _synthetic_census(extra=[_row("bridge", "p/GONE", _ABSENT)]))

    # 4. An injected Bridge onto a genuine ACC_NATIVE target is NOT a
    #    regression. The gate must not fire on adjudicated work.
    check("injected genuine Bridge passes", 0,
          _synthetic_census(extra=[_row("bridge", "p/REAL", _NATIVE)]))

    # 5. Vacuity: a collapsed registry fails rather than passing with fewer
    #    unadjudicated rows than the baseline.
    collapsed = _synthetic_census()
    collapsed["natives"] = [r for r in collapsed["natives"] if r["kind"] == "bridge"]
    check("collapsed registry fails the vacuity floor", 1, collapsed)

    # 6. Refusals -- each must be 2, never 0.
    no_image = _synthetic_census()
    no_image["image_adjudication"] = False
    for r in no_image["natives"]:
        r["image_declaring_method"] = None
    check("image_adjudication:false is refused, not scored", 2, no_image)

    partial = _synthetic_census()
    partial["partial"] = True
    check("partial census is refused", 2, partial)

    old = _synthetic_census()
    old["schema_version"] = 2
    check("schema 2 census is refused", 2, old)

    check("unknown JDK feature is refused", 2, _synthetic_census(), feature=21)
    check("a census from another OS is refused, not scored", 2,
          _synthetic_census(), os_name="windows")

    strict = _synthetic_census()
    strict["mode"] = "jdk-only"
    check("mode mismatch is refused", 2, strict)

    # 7. An improvement passes and says to re-freeze.
    better = _synthetic_census()
    better["natives"] = [r for r in better["natives"] if r["class"] != "p/C0"]
    code, lines = gate(adjudicate(better), _selftest_baseline(), 25, "linux")
    checks.append((code == 0 and any("IMPROVED" in ln for ln in lines),
                   "an improvement passes and asks for a re-freeze", 0, code, lines))

    # 8. The block itself: buckets disjoint, and the doc's identity holds.
    block = adjudicate(_synthetic_census())
    b = block["bridge"]
    identity = (b["without_acc_native"] == b["rows"] - b["acc_native"]
                == b["shadows_bytecode"] + b["abstract_method"]
                + b["class_present_method_undeclared"] + b["class_absent"])
    checks.append((identity, "the five image buckets are disjoint and sum to the total",
                   True, identity, []))

    failed = 0
    for ok, name, want, got, lines in checks:
        print(f"  {'ok  ' if ok else 'FAIL'} {name}   (want {want}, got {got})")
        if not ok:
            failed += 1
            for ln in lines:
                print(f"         | {ln}")
    print(f"\nselftest: {len(checks) - failed} passed, {failed} failed")
    return 1 if failed else 0


def main(argv=None):
    ap = argparse.ArgumentParser(
        description="Ratchet the unadjudicated Bridge registrations (lane L6).",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    ap.add_argument("--census", help="schema-3 native census JSON (--dump-native-registry)")
    ap.add_argument("--baseline", default=DEFAULT_BASELINE,
                    help=f"committed baseline (default: {DEFAULT_BASELINE})")
    ap.add_argument("--jdk-feature", type=int,
                    help="JDK feature version the census was taken against")
    ap.add_argument("--os", dest="os_name", default=host_os(),
                    help="OS half of the baseline key (default: this host, "
                         f"{host_os()}). Override only when scoring a census file "
                         "carried over from another machine.")
    ap.add_argument("--emit-json", metavar="FILE",
                    help="write the machine-readable block here ('-' for stdout) and exit")
    ap.add_argument("--update-baseline", action="store_true",
                    help="re-freeze the baseline for --jdk-feature to the observed counts")
    ap.add_argument("--note", default="", help="why the baseline moved (recorded in the file)")
    ap.add_argument("--jdk-version", default="",
                    help="full JDK version string, recorded beside the baseline")
    ap.add_argument("--workload", default="",
                    help="the workload the census came from, recorded beside the baseline")
    ap.add_argument("--selftest", action="store_true",
                    help="hermetic self-test: no JDK, no VM, no baseline file read or written")
    args = ap.parse_args(argv)

    if args.selftest:
        return selftest()

    if not args.census:
        ap.error("--census is required (or use --selftest)")
    try:
        with open(args.census, encoding="utf-8") as fh:
            doc = json.load(fh)
    except (OSError, ValueError) as exc:
        print(f"ERROR: cannot read census {args.census}: {exc}", file=sys.stderr)
        return 3

    block = adjudicate(doc)

    if args.emit_json:
        text = json.dumps(block, indent=2, sort_keys=True) + "\n"
        if args.emit_json == "-":
            sys.stdout.write(text)
        else:
            with open(args.emit_json, "w", encoding="utf-8") as fh:
                fh.write(text)
        return 0

    print(f"== bridge-ratchet: {args.census} ==")
    print(render_block(block))
    print()

    if args.jdk_feature is None:
        print("ERROR: --jdk-feature is required. The image adjudication is a statement "
              "about one JDK image, so the baseline is keyed by feature version and "
              "guessing it would score the census against the wrong one.", file=sys.stderr)
        return 3

    baseline = load_baseline(args.baseline)

    if args.update_baseline:
        if not block["image_adjudication"] or block["partial"] or \
                block["census_schema_version"] != REQUIRED_CENSUS_SCHEMA:
            code, lines = gate(block, baseline, args.jdk_feature, args.os_name)
            for ln in lines:
                print(ln)
            print("REFUSING to freeze a baseline from a census the gate would refuse to read.",
                  file=sys.stderr)
            return 2
        if block["total_rows"] < baseline.get("min_total_rows", MIN_TOTAL_ROWS):
            print(f"REFUSING to freeze a baseline from a census of only "
                  f"{block['total_rows']} rows — see the vacuity floor.", file=sys.stderr)
            return 2
        baseline.setdefault("schema", BLOCK_SCHEMA)
        baseline.setdefault("slack", SLACK)
        baseline.setdefault("min_total_rows", MIN_TOTAL_ROWS)
        key = baseline_key(args.jdk_feature, args.os_name)
        entry = {
            "mode": block["mode"],
            "os": args.os_name,
            "jdk_version": args.jdk_version,
            "workload": args.workload,
            "note": args.note,
            "bridge_without_acc_native": block["bridge"]["without_acc_native"],
            "bridge_shadows_bytecode": block["bridge"]["shadows_bytecode"],
            "observed": block,
        }
        baseline.setdefault("jdk", {})[key] = entry
        os.makedirs(os.path.dirname(os.path.abspath(args.baseline)), exist_ok=True)
        with open(args.baseline, "w", encoding="utf-8") as fh:
            fh.write(json.dumps(baseline, indent=2, sort_keys=True) + "\n")
        print(f"baseline for {key} frozen in {args.baseline}:")
        for _, base_key, human in RATCHETS:
            print(f"  {human}: {entry[base_key]}")
        if not args.note:
            print("NOTE: no --note recorded. A baseline that moved for an unstated "
                  "reason is indistinguishable from one that moved by accident.")
        return 0

    code, lines = gate(block, baseline, args.jdk_feature, args.os_name)
    for ln in lines:
        print(ln)
    if code == 0:
        print("\nBRIDGE-RATCHET: PASS")
    elif code == 1:
        print("\nBRIDGE-RATCHET: FAIL")
    else:
        print("\nBRIDGE-RATCHET: REFUSED (this is not a pass)")
    return code


if __name__ == "__main__":
    sys.exit(main())
