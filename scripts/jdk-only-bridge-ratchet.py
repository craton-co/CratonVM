#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""BRIDGE-RATCHET GATE — a one-way ratchet on unadjudicated `Bridge` natives.

Contract §1.5 defines a `Bridge` as what an `ACC_NATIVE` method binds to. The
schema-4 census (`--dump-native-registry` with `--explain-jdk-only`) makes
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
2. `bridge.shadows_bytecode_anywhere <= baseline` with `SLACK = 0`. The same ratchet on
   the subgroup that has already produced a defect: a `Bridge` shadowing
   concrete bytecode can reach §7 step 3's decline, which used to fall through
   to `UnsatisfiedLinkError` instead of to the bytecode (that is how
   `--jdk-only` came to be unable to start a thread). 4,796 rows can reach it,
   plus roughly 1,600 more that shadow bytecode they INHERIT — invisible until
   schema 4 resolved `image_declaring_method` up the hierarchy.
3. `bridge.stated_shadows_bytecode <= baseline` with `SLACK = 0`. The two
   ratchets above count rows regardless of `kind_stated`, and `kind_stated` is
   the column every reader treats as "somebody checked this against the image".
   Nothing asked whether a STATED claim is true, so 24 rows shadowing concrete
   bytecode sat under a §1.5 claim for months, visible only to a `print`.
4. `superseded.kind_disagreements <= baseline` and
   `superseded.stub_lost_to_admitted <= baseline`, both `SLACK = 0`. A
   superseded registration can never be dispatched, so its kind normally
   decides nothing — EXCEPT when it disagrees with the winner's, because then
   the kind that ships was chosen by call order in `vm_init` rather than by
   anyone. 52 rows disagree; 4 of them are a `SyntheticStub` shipping as a
   `Bridge`, which `--jdk-only` then admits.
5. `total_rows >= MIN_TOTAL_ROWS` — a **collapse detector, not a measurement**.
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

# The census schema that carries `image_declaring_method` WITH its four
# `inherited_*` keys. Anything else cannot answer the question this gate asks.
#
# Schema 3 carried the column but asked it of ONE class name, so a triple
# declared `ACC_NATIVE` on a supertype came back `declared: false` and was
# counted as unadjudicated. Measured on JDK 25.0.4+7/linux, 2026-08-10: 19 rows
# — eighteen `sun/nio/ch/FileDispatcherImpl.*` inheriting the syscall surface
# from `UnixFileDispatcherImpl`, plus `ComponentSampleModel.initIDs()V` from
# `SampleModel`. A schema-3 census scored with this file's arithmetic would be
# 19 too high, in the direction that reads as "more work outstanding", so the
# older shape is REFUSED rather than degraded.
# 4 -> 5 on 2026-08-17: the census writer now emits `invocations_complete`
# per row and `slots_with_incomplete_invocations` in the header, so that a
# reader can tell that an `invocations` figure is a FLOOR rather than a count.
# This pin is an equality test, so it has to move in the same commit as the
# writer or this gate refuses every dump. Schema 5 is a superset of 4 --
# image_declaring_method, which is the question this gate actually asks, is
# unchanged.
REQUIRED_CENSUS_SCHEMA = 5

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


# Both counts are asked of the WHOLE HIERARCHY, not of the named class. A
# native over a method the receiver's class inherits is reached by dispatch
# exactly as one over a method it declares, so scoring only the own-class
# columns let an inherited shadow in free and counted an inherited ACC_NATIVE
# bridge as outstanding work.
RATCHETS = (
    ("bridge.without_acc_native", "bridge_without_acc_native",
     "Bridge registrations with no ACC_NATIVE target anywhere in the hierarchy"),
    ("bridge.shadows_bytecode_anywhere", "bridge_shadows_bytecode",
     "Bridge registrations shadowing concrete bytecode (declared or inherited)"),
    ("bridge.stated_shadows_bytecode", "bridge_stated_shadows_bytecode",
     "Bridge registrations that STATE their kind and shadow concrete bytecode"),
    ("superseded.kind_disagreements", "superseded_kind_disagreements",
     "superseded registrations whose KIND disagrees with the winner's"),
    ("superseded.stub_lost_to_admitted", "superseded_stub_lost_to_admitted",
     "SyntheticStub registrations superseded by a Bridge/Intrinsic (admitted under --jdk-only)"),
)


def _at(block, path):
    """`block["bridge"]["rows"]` for the path `"bridge.rows"`."""
    cur = block
    for part in path.split("."):
        cur = cur[part]
    return cur


def _img(row):
    """The image verdict for a row, as a dict.

    `image_declaring_method` is `null` when the adjudication pass did not run.
    Callers must have checked `image_adjudication` first — this returning `{}`
    would otherwise score every row as "class absent", which reads exactly like
    a clean result.
    """
    return row.get("image_declaring_method") or {}


# `java.lang.Object` declares `hashCode`, `clone`, `getClass`, `notify`,
# `notifyAll` and `wait` ACC_NATIVE, and EVERY class and interface inherits
# them. Resolving a triple up the hierarchy therefore lands on Object for any
# registration of one of those names on any receiver — measured, 4 of the 23
# hierarchy credits on JDK 25.0.4/linux are `java/lang/reflect/*Type.hashCode()I
# -> java/lang/Object`.
#
# That resolution is FACTUALLY correct (JVMS §5.4.3.3 reaches Object's public
# methods for an interface too), which is why the census states it. It is not an
# ADJUDICATION: a native on `ParameterizedType.hashCode` is not binding Object's
# native, it stands in front of whatever the real implementor overrides
# `hashCode` with. Crediting it would let any `X.hashCode()I` Bridge, on any
# class, discharge its §1.5 claim by pointing at a method every object has.
#
# So the fact lives in the census and the policy lives here.
_OBJECT = "java/lang/Object"


def _inherits_from_object(img):
    return img.get("inherited_from") == _OBJECT


def adjudicate(doc):
    """Reduce a schema-4 census to the machine-readable adjudication block.

    The seven `bridge` buckets are disjoint and sum to `bridge.rows`. They are
    disjoint by construction, not by convention: the emitter derives `has_code`
    as `!native && !abstract` (JVMS §4.6), so `acc_native` and `has_code` can
    never both be true, `abstract` is what is left once both are false, and the
    three `inherited_*` flags are only ever set on a row whose own class does
    not declare the method.

    Each bucket names WHERE the declaration is, so "the class declares it
    ACC_NATIVE" and "a supertype does" stay countable apart even though both
    discharge a `Bridge` claim.
    """
    rows = doc.get("natives") or []
    kinds = Counter(r.get("kind") for r in rows)

    bridges = [r for r in rows if r.get("kind") == "bridge"]
    acc_native = shadow = abstract_ = undeclared = absent = 0
    inh_native = inh_shadow = inh_abstract = 0
    stated_shadow = 0
    for row in bridges:
        img = _img(row)
        if row.get("kind_stated") and (img.get("has_code") or img.get("inherited_has_code")):
            stated_shadow += 1
        if not img.get("image_has_class"):
            absent += 1
        elif img.get("declared"):
            if img.get("acc_native"):
                acc_native += 1
            elif img.get("has_code"):
                shadow += 1
            else:
                abstract_ += 1
        elif img.get("inherited_acc_native") and not _inherits_from_object(img):
            inh_native += 1
        elif img.get("inherited_has_code"):
            inh_shadow += 1
        elif img.get("inherited_abstract"):
            inh_abstract += 1
        elif img.get("inherited_acc_native"):
            # Reached only via java.lang.Object — see `_inherits_from_object`.
            # Still unadjudicated work, and it intercepts every implementor,
            # so it is counted with the abstract-interception population.
            inh_abstract += 1
        else:
            # Genuinely nowhere in the hierarchy. THIS is the bucket that used
            # to be called `class_present_method_undeclared` and swallowed the
            # three above it.
            undeclared += 1

    buckets = (acc_native, shadow, abstract_,
               inh_native, inh_shadow, inh_abstract, undeclared, absent)
    if sum(buckets) != len(bridges):  # pragma: no cover - structural invariant
        raise AssertionError(
            f"bridge buckets {buckets} sum to {sum(buckets)}, not {len(bridges)} — "
            "the census emitter changed the meaning of image_declaring_method"
        )

    # --- the superseded population ------------------------------------------
    #
    # A registration that no longer owns its slot can never be dispatched, so
    # its KIND normally decides nothing and it is not reclassification work.
    # There is one exception and it is the whole reason this is counted: when
    # the superseded row and the winner state DIFFERENT kinds, the kind that
    # ships was decided by CALL ORDER in `vm_init`, and kind drives three
    # policies (`--jdk-only` refuses a SyntheticStub, `CRATONVM_NO_STUBS` drops
    # one, and only a SyntheticStub is subject to the yield arbitration).
    #
    # Measured on JDK 25.0.4/linux, 2026-08-11: 1,215 superseded rows, of which
    # 1,163 agree with the winner and 52 do not. Four of the 52 are the
    # dangerous direction — a registrar tagged the triple SyntheticStub and a
    # later one ships it as a Bridge, so `--jdk-only` ADMITS a registration
    # somebody classified as a fake.
    superseded = [r for r in rows if not r.get("owns_slot", True)]
    winners = {}
    for r in rows:
        if r.get("owns_slot", True):
            winners[(r.get("class"), r.get("name"), r.get("descriptor"))] = r
    disagreements = 0
    stub_lost = 0
    for r in superseded:
        w = winners.get((r.get("class"), r.get("name"), r.get("descriptor")))
        if w is None or w.get("kind") == r.get("kind"):
            continue
        disagreements += 1
        if r.get("kind") == "synthetic-stub" and w.get("kind") in ("bridge", "intrinsic"):
            stub_lost += 1

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
            "inherited_acc_native": inh_native,
            "inherited_shadows_bytecode": inh_shadow,
            "inherited_abstract_method": inh_abstract,
            "class_present_method_undeclared": undeclared,
            "class_absent": absent,
            # The ratchet population. An inherited ACC_NATIVE declaration
            # discharges a §1.5 claim exactly as an own one does — dispatch
            # reaches the registration either way — so it is subtracted here
            # too. Schema 3 could not see those rows and counted them as work.
            "without_acc_native": len(bridges) - acc_native - inh_native,
            # The §1.4 shadow population, likewise over the whole hierarchy. A
            # native over a method the class inherits concretely shadows just as
            # much bytecode as one over a method it declares.
            "shadows_bytecode_anywhere": shadow + inh_shadow,
            # Rows whose kind is STATED (a deliberate `set_category`/
            # `register_with_kind`, not an ambient default) and whose target is
            # concrete bytecode on this image: a §1.4 shadow wearing a §1.5
            # claim. Neither ratchet above could see these -- both count rows
            # regardless of `kind_stated`, and `kind_stated` is the column every
            # reader treats as "somebody checked this against the image".
            #
            # Measured on JDK 25.0.4+7/linux, 2026-08-10: 24 registrations /
            # 12 triples, all `ForkJoinTask.{fork, invokeAll x3, quietly*}`
            # plus `RecursiveTask.fork` and `RecursiveAction.fork` in
            # `native-builtins/src/phases_late/concurrent.rs`. Those are
            # deliberate, load-bearing shadows (the site comments record that
            # `RJdkForkJoin` hangs without them) and they belong to the shadow
            # population and ITS blocker, not to a quick fix -- so the baseline
            # holds them rather than a lane deleting them. What must not happen
            # is a 25th arriving unnoticed.
            "stated_shadows_bytecode": stated_shadow,
        },
        "superseded": {
            "rows": len(superseded),
            "kind_disagreements": disagreements,
            "stub_lost_to_admitted": stub_lost,
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
        ("INHERITED ACC_NATIVE — a bridge, on a supertype", "inherited_acc_native"),
        ("INHERITED bytecode — a shadow of what it inherits", "inherited_shadows_bytecode"),
        ("INHERITED abstract — intercepts every implementor", "inherited_abstract_method"),
        ("nowhere in the hierarchy", "class_present_method_undeclared"),
        ("class absent from the image", "class_absent"),
    ):
        n = b[key]
        lines.append(f"  {label:<48}{n:>7}{100.0 * n / rows:>7.0f}%")
    lines.append(
        f"  {'BRIDGE rows with no ACC_NATIVE target':<48}"
        f"{b['without_acc_native']:>7}{100.0 * b['without_acc_native'] / rows:>7.0f}%"
    )
    sup = block.get("superseded")
    if sup:
        lines.append("")
        lines.append(
            f"  superseded registrations (own no slot, never dispatch): {sup['rows']}"
        )
        lines.append(
            f"    …whose KIND disagrees with the winner's:              "
            f"{sup['kind_disagreements']}"
        )
        lines.append(
            f"    …of those, a SyntheticStub shipping as Bridge/Intrinsic: "
            f"{sup['stub_lost_to_admitted']}"
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

    missing = [b for _, b, _ in RATCHETS if b not in entry]
    if missing:
        # A baseline that predates a ratchet cannot score it, and defaulting the
        # frozen value (to 0, or to the observed number) would either fire on
        # every run or freeze whatever is there today as acceptable. Refuse and
        # say which key is absent.
        return 2, [
            f"REFUSING: the baseline for {key} has no "
            f"{', '.join(missing)} entry, so {'that ratchet' if len(missing) == 1 else 'those ratchets'} "
            "cannot be scored. Re-freeze in the same change that added it: "
            "python3 scripts/jdk-only-bridge-ratchet.py --census <census.json> "
            f"--jdk-feature {jdk_feature} --update-baseline --note '<why>'"
        ]

    for block_key, base_key, human in RATCHETS:
        observed = _at(block, block_key)
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

def _row(kind, cls, verdict, owns_slot=True, name="m"):
    return {
        "class": cls, "name": name, "descriptor": "()V", "kind": kind,
        "registered_by": None, "overwrote": None, "invocations": 0,
        "kind_stated": False, "real_declaring_method": None,
        "owns_slot": owns_slot, "image_declaring_method": verdict,
    }


def _verdict(has_class=True, declared=False, acc_native=False, has_code=False,
             inherited_from=None, inh_native=False, inh_code=False, inh_abstract=False):
    return {
        "image_has_class": has_class, "declared": declared,
        "acc_native": acc_native, "has_code": has_code,
        "inherited_from": inherited_from, "inherited_acc_native": inh_native,
        "inherited_has_code": inh_code, "inherited_abstract": inh_abstract,
    }


_NATIVE = _verdict(declared=True, acc_native=True)
_CODE = _verdict(declared=True, has_code=True)
_ABSTRACT = _verdict(declared=True)
# The three shapes schema 3 could not tell apart. All three answer
# `declared: false`; only the `inherited_*` keys separate a §1.5 bridge on a
# supertype from a §1.4 shadow of inherited bytecode from a genuinely dead row.
_INH_NATIVE = _verdict(inherited_from="p/Super", inh_native=True)
_INH_CODE = _verdict(inherited_from="p/Super", inh_code=True)
_INH_ABSTRACT = _verdict(inherited_from="p/Super", inh_abstract=True)
_UNDECL = _verdict()
_ABSENT = _verdict(has_class=False)


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
        "schema_version": REQUIRED_CENSUS_SCHEMA, "image_adjudication": True,
        "mode": "compatible", "counts": {}, "invocations": {}, "natives": natives,
    }


def _selftest_baseline():
    return {
        "schema": BLOCK_SCHEMA, "slack": 0, "min_total_rows": MIN_TOTAL_ROWS,
        "jdk": {"25/linux": {"mode": "compatible", "os": "linux",
                             "bridge_without_acc_native": 8,
                             "bridge_shadows_bytecode": 4,
                             "bridge_stated_shadows_bytecode": 0,
                             "superseded_kind_disagreements": 0,
                             "superseded_stub_lost_to_admitted": 0}},
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

    # Schema 3 carried `image_declaring_method` but asked it of ONE class, so
    # scoring it here would count every inherited row as unadjudicated. It is
    # refused, not degraded -- the whole point of the bump.
    pre_hierarchy = _synthetic_census()
    pre_hierarchy["schema_version"] = 3
    check("schema 3 (pre-hierarchy) census is refused", 2, pre_hierarchy)

    check("unknown JDK feature is refused", 2, _synthetic_census(), feature=21)

    # A baseline that predates a ratchet must REFUSE, not default the frozen
    # value. Defaulting to 0 fires on every run; defaulting to the observed
    # number freezes whatever is there today as acceptable, silently.
    stale_baseline = _selftest_baseline()
    del stale_baseline["jdk"]["25/linux"]["superseded_kind_disagreements"]
    check("a baseline missing a ratchet key is refused", 2, _synthetic_census(),
          baseline=stale_baseline)
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
    identity = (b["without_acc_native"]
                == b["rows"] - b["acc_native"] - b["inherited_acc_native"]
                == b["shadows_bytecode"] + b["abstract_method"]
                + b["inherited_shadows_bytecode"] + b["inherited_abstract_method"]
                + b["class_present_method_undeclared"] + b["class_absent"])
    checks.append((identity, "the seven image buckets are disjoint and sum to the total",
                   True, identity, []))

    # 9. THE ITEM-1 REGRESSION, shown failing both ways round.
    #
    #  (a) a Bridge that inherits an ACC_NATIVE supertype method is ADJUDICATED
    #      -- it must not trip the ratchet. Under schema 3 it did: this is the
    #      nineteen `FileDispatcherImpl`/`ComponentSampleModel` rows.
    check("a Bridge inheriting ACC_NATIVE does not trip the ratchet", 0,
          _synthetic_census(extra=[_row("bridge", "p/INH", _INH_NATIVE)]))

    #      …but NOT through java.lang.Object, which declares hashCode/clone/
    #      wait/notify ACC_NATIVE and is inherited by everything. Crediting that
    #      would discharge any X.hashCode()I Bridge on any class.
    obj_inh = _verdict(inherited_from="java/lang/Object", inh_native=True)
    check("a Bridge inheriting ACC_NATIVE from java.lang.Object still trips it", 1,
          _synthetic_census(extra=[_row("bridge", "p/OBJINH", obj_inh)]))

    #  (b) a Bridge that inherits CONCRETE BYTECODE is a shadow and must trip
    #      both ratchets. Under schema 3 it landed in `undeclared` and tripped
    #      only the aggregate one, so the shadow ratchet could not see it.
    check("a Bridge inheriting concrete bytecode trips the ratchet", 1,
          _synthetic_census(extra=[_row("bridge", "p/INHC", _INH_CODE)]))
    tripped_inh = sum(1 for ln in checks[-1][4] if ln.startswith("BRIDGE-RATCHET REGRESSION"))
    checks.append((tripped_inh == 2,
                   "the inherited-shadow injection trips BOTH ratchets", 2, tripped_inh, []))

    #  (c) an inherited ABSTRACT declaration is still unadjudicated work.
    check("a Bridge inheriting an abstract method trips the aggregate ratchet", 1,
          _synthetic_census(extra=[_row("bridge", "p/INHA", _INH_ABSTRACT)]))

    # 10. THE STATED-CLAIM RATCHET. A row that STATES Bridge over concrete
    #     bytecode is a §1.4 shadow wearing a §1.5 claim, and no gate watched
    #     that direction: `kind_stated` is exactly the column readers trust.
    stated = _row("bridge", "p/STATED", _CODE)
    stated["kind_stated"] = True
    check("a STATED Bridge over concrete bytecode trips the stated ratchet", 1,
          _synthetic_census(extra=[stated]))
    tripped_stated = sum(1 for ln in checks[-1][4]
                         if ln.startswith("BRIDGE-RATCHET REGRESSION")
                         and "STATE their kind" in ln)
    checks.append((tripped_stated == 1, "the stated ratchet is the one that names it",
                   1, tripped_stated, []))

    #     An inherited shadow under a stated claim counts too -- the whole
    #     reason the hierarchy pass exists is that it was invisible.
    stated_inh = _row("bridge", "p/STATEDINH", _INH_CODE)
    stated_inh["kind_stated"] = True
    check("a STATED Bridge over INHERITED bytecode trips it too", 1,
          _synthetic_census(extra=[stated_inh]))

    # 11. THE SUPERSEDED RATCHETS. A superseded row is normally inert -- it can
    #     never be dispatched -- so an AGREEING pair must not fire anything.
    agree = [_row("bridge", "p/AGREE", _NATIVE, owns_slot=False, name="dup"),
             _row("bridge", "p/AGREE", _NATIVE, owns_slot=True, name="dup")]
    check("a superseded row agreeing with the winner fires nothing", 0,
          _synthetic_census(extra=agree))

    #     A DISAGREEING pair does fire: which kind ships was decided by call
    #     order, and kind drives three policies.
    disagree = [_row("intrinsic", "p/DIS", _NATIVE, owns_slot=False, name="dup"),
                _row("bridge", "p/DIS", _NATIVE, owns_slot=True, name="dup")]
    check("a superseded row DISAGREEING with the winner trips the ratchet", 1,
          _synthetic_census(extra=disagree))

    #     And the dangerous direction trips BOTH superseded ratchets: a
    #     registrar called it a stub, a later one ships it as a Bridge, and
    #     `--jdk-only` then ADMITS it.
    stub_lost = [_row("synthetic-stub", "p/STUBLOST", _NATIVE, owns_slot=False, name="dup"),
                 _row("bridge", "p/STUBLOST", _NATIVE, owns_slot=True, name="dup")]
    check("a SyntheticStub superseded by a Bridge trips both superseded ratchets", 1,
          _synthetic_census(extra=stub_lost))
    tripped_sup = sum(1 for ln in checks[-1][4]
                      if ln.startswith("BRIDGE-RATCHET REGRESSION") and "superseded" in ln)
    checks.append((tripped_sup == 2, "the stub-lost injection trips both superseded ratchets",
                   2, tripped_sup, []))

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
    ap.add_argument("--census",
                    help=f"schema-{REQUIRED_CENSUS_SCHEMA} native census JSON "
                         "(--dump-native-registry --explain-jdk-only)")
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
            "observed": block,
        }
        # Every RATCHET's frozen value, from the ONE list that defines them.
        # The three used to be written out by hand here, so adding a ratchet
        # left its baseline key absent and `gate` raised a KeyError on the next
        # run — and `bridge_shadows_bytecode` was being frozen from the
        # own-class count while the gate scored the hierarchy-wide one, which
        # would have fired on a tree nobody had changed.
        for block_key, base_key, _ in RATCHETS:
            entry[base_key] = _at(block, block_key)
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
