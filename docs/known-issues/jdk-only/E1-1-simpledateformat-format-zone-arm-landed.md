# E1-1 — the `format` zone arm is LANDED, and the fixture the brief named cannot gate it

**2026-08-13, lane E1.** Applies the `date_format_fast.rs` patch nominated by
`C12-1` §5 and re-specified by `D3-1` §6. This lane **owns
`native-builtins/src/date_format_fast.rs`** and wrote it; everything else here
is a NOMINATION.

**This lane may not build or run the VM.** Every CratonVM "after" below is
**PREDICTED**. Every HotSpot number was executed on this host against
`openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)` from
`scratchpad/e1/E1Zero.java` and `scratchpad/e1/E1Cls.java`.

This record duplicates `D3-1`'s analysis on purpose — the brief asked for the
verification to be re-derived, not relayed — and it **corrects `D3-1` on one
point** (§5) and **the brief on one point** (§4).

---

## 0. Verdict

| claim | verdict |
|---|---|
| the defect is real and the nominated shape is right | **CONFIRMED** (§1) |
| the `else` arm answers correctly once `SimpleTimeZone` leaves the set | **CONFIRMED from the dispatch source** (§2). This was the brief's stated landing condition; it holds |
| `RSimpleDateFormatZone`'s `fmtdate`/`routes`/`roundtrip`/`dstrule` are RED for this defect | **WRONG** (§4). All six blocks read GREEN before AND after. The fixture cannot distinguish, and this lane's own HotSpot measurement of the `zeroDigit` gate is why |
| the patch's real payload | removing a **silent** wrong answer (§3) and 11 PREDICTED stderr lines, not flipping a fixture block |

## 1. What landed, in `native-builtins/src/date_format_fast.rs`

Three edits, applied in `D3-1` §6's order. The file was and remains uniformly
CRLF (**1,234 `\r\n` / 1,234 `\n`** after the edits — counted, because a
Windows-side edit that introduces a bare LF is a standing hazard in this repo).

1. **`Slots::simple_tz_class` field deleted** (`:255`-`:263`), doc comment
   rewritten to say why `java/util/SimpleTimeZone` is excluded.
2. **its initializer deleted** (`:306`).
3. **the arm itself** (`:806`) narrowed from three classes to two, with the
   reasoning inline so it cannot be "restored" by someone reading only the
   `ZoneInfo` line.

Plus a fourth, not in the nomination: the comment three lines above the arm
still said *"For the **three** `TimeZone` classes this VM implements natively
itself"*. It now says **two**. A stale count directly above the predicate it
describes is how the deleted arm gets argued back in.

`simple_tz_class` now occurs **0 times** in the file (it was 3: `:262`, `:306`,
`:807`), and **0 times anywhere else in the tree** — grepped across `*.rs`,
`*.toml`, `*.sh`. Nothing dangles; no lint fires. `tz_id` and `timezone_class`
are still read (`zone_rules_cached`, `slots()`), so they stay.

The other half of this defect — the four `java/util/SimpleTimeZone` tzdb natives
and `alloc_synth_timezone`'s `tz_dst_rule` branch — was already gone from
`lib.rs` (verified in the working tree: `register_tzdb_offset_natives_for` now
has exactly two callers, `sun/util/calendar/ZoneInfo` at `:20477` and
`java/util/TimeZone` at `:20521`, and `:20478` carries C12-1's reasoning in
place of the third).

## 2. THE LANDING CONDITION — what answers instead — closed from the source

The brief's condition: *establish what will answer once the class is removed; if
the fallback is also wrong, dropping it converts one wrong answer into another.*
The `else` arm is `ctx.invoke_virtual(zone, "getOffset", "(J)I", …)`. Re-derived
here, not taken from `D3-1`:

| step | where | what happens for a `java.util.SimpleTimeZone` receiver |
|---|---|---|
| `NativeContext::invoke_virtual` | `vm/src/vm/vm_exec.rs:9286` | not an array, not a lambda proxy → `class_name` comes from the RECEIVER's `ClassId` (`:9812`-`9840`), `resolved_from_receiver = true` |
| `needs_exact_class_dispatch` | `:9986` | `method_name != "toString"`; the remaining disjunct only fires when the globally-registered id for that name differs from the receiver's, which for a boot-loader JDK class it does not. Either way both branches key on the receiver's own class |
| direct native lookup | `:17120`-`17128` `find(class_name, …)` | **MISSES** — C12-1 deleted all four `java/util/SimpleTimeZone` rows |
| superclass climb | `:17137`-`17152` | **SKIPPED.** `has_own_bytecode` is `cls.find_method(name, desc).is_some()` on the receiver's class, and `javap -p --module java.base java.util.SimpleTimeZone` on this host prints `public int getOffset(long);`. So `java/util/TimeZone`'s surviving tzdb native does **not** capture it |
| result | | the real `SimpleTimeZone.getOffset(long)` bytecode — `rawOffset` plus the caller's own DST rule |

**The fallback is correct**, and correct for the eleven-argument constructor too,
because `SimpleTimeZone.getOffset(long)` applies the caller's supplied rule.

This is the same conclusion `C12-1` §2 reached, but for a **different gate**:
that record read `try_stackless_invoke` in the interpreter; the `else` arm goes
through `vm_exec::invoke_or_native`. Two functions, two `has_own_bytecode`
checks, same answer for the same reason. Both were read.

**Synthetic-JDK mode is untouched**: `slots()` (`:290`) returns `None` unless it
resolves `java/text/SimpleDateFormat.forceStandaloneForm` and
`java/util/GregorianCalendar.gregorianCutover`, and `forceStandaloneForm` appears
nowhere in the synthetic class definitions. The intrinsic never runs there, so
this patch is a no-op in that mode.

## 3. WHAT THE PATCH ACTUALLY BUYS — the cross-check was already catching most of it

`date_format_fast.rs:1049`-`1075` is a run-time self-check: the first time a
formatter produces a given output SHAPE, the native result is compared against
the same format run as bytecode (`format_via_bytecode` at `:686`, which uses
`invoke_virtual_bytecode_only` on the 3-arg descriptor). On a mismatch it
poisons the formatter, prints an **unconditional** stderr line, and
`return Ok(reference)` — i.e. **hands back the correct bytecode answer**.

`GregorianCalendar.computeFields` is not natively intercepted (grepped: the only
`computeFields` hits in `native-builtins` are comments), so the bytecode
reference reaches `SimpleTimeZone`'s real `getOffset` by the §2 route and is
right. Therefore, **in the state this patch was applied to** (C12-1 landed, this
one not), the two arms disagreed, the cross-check fired, and the caller already
saw the correct string. The residual damage was:

* an alarming stderr line whose own comment says *"a divergence here is a real
  defect in this module"* — it was not; it was this zone bug;
* permanent loss of the fast path for that formatter, including for its later
  formats under a perfectly good `ZoneInfo`;
* **and one hole the cross-check does not cover**, below.

### 3a. The hole — and why it is the real reason to land this

`verified` is keyed by `(formatter identity, shape)` and is invalidated only by a
change of `pattern` or `DateFormatSymbols` identity (`:1007`-`1013`).
**`setTimeZone` invalidates nothing.** A formatter that has already verified
shape *S* under one zone takes the fast path for *S* under a different zone with
**no cross-check at all** — a silent wrong answer, no stderr line, no fallback.

Measured on HotSpot (`scratchpad/e1/E1Zero.java`), all values executed:

```
EtcGMT-3 offset at JAN=10800000
warm1=2021-01-15 15:00:00 +0300
warm2=2021-01-15 15:00:00 +0300
Tokyo true offset at JAN=32400000
after-swap=2021-01-15 15:00:00 +0300          <- correct
mutant-after-swap=2021-01-15 21:00:00 +0900   <- what a tzdb-by-id VM prints
```

for

```java
SimpleDateFormat f = new SimpleDateFormat("yyyy-MM-dd HH:mm:ss Z", Locale.US);
f.setTimeZone(TimeZone.getTimeZone("Etc/GMT-3"));   // +03:00, a ZoneInfo
f.format(new Date(JAN));   // 1st: declines (zeroDigit==0) -> bytecode
f.format(new Date(JAN));   // 2nd: fast path, cross-checked, shape VERIFIED
f.setTimeZone(new SimpleTimeZone(10800000, "Asia/Tokyo"));
f.format(new Date(JAN));   // shape key IDENTICAL -> NO cross-check
```

The shape key (`:922`) is
`(month0, day_of_week, era, pm, offset!=0, offset<0)`; the wrong offset
(+09:00) lands on the same tuple as the right one (+03:00), so the memo hit is
exact. The row is non-vacuous by construction: Tokyo's true offset (32,400,000)
differs from the supplied `rawOffset` (10,800,000), and both are positive and
non-zero so the shape cannot separate them.

**PREDICTED, CratonVM before the patch:** `2021-01-15 21:00:00 +0900`, silently.
**PREDICTED, after:** `2021-01-15 15:00:00 +0300`, because `gather` now takes the
`else` arm for that receiver.

## 4. WHY `RSimpleDateFormatZone` CANNOT GATE THIS PATCH — correcting the brief

The brief states `fmtdate`/`routes`/`roundtrip`/`dstrule` are RED for this defect
and asks which the change flips. The answer is **none**, for two independent
reasons, and the first one was measured here rather than reasoned:

**(a) The first `format` on any fresh formatter never reaches the fast path.**
`gather` (`:762`-`:767`) declines unless `zeroDigit == '0'`; the JDK sets that
field lazily inside `zeroPaddingNumber` on first use. Measured on this host:

```
fresh zeroDigit=0
fresh forceStandaloneForm=false
format1=2021-01-15 12:00:00 +0000
after1 zeroDigit=48
cal=java.util.GregorianCalendar zone=java.util.SimpleTimeZone
```

`fmtdate` and `roundtrip` build a fresh `SimpleDateFormat` per row (`sdf(...)`)
and format it **exactly once**, so all twenty of those formats run bytecode,
defect or no defect.

**(b) Where the fast path does engage, the cross-check corrects it (§3).**
`routes` formats each formatter four times, of which the 1st and 4th are the
intercepted `format(Date)` descriptor — the 4th engages on an armed
`zeroDigit`, hits a fresh shape, cross-checks, mismatches, and returns the
bytecode answer. `dstrule` formats twice, with the same outcome on the second.

`control` was never in question — and this lane checked the one way it could
have been. Every zone `control` uses is a `ZoneInfo`, including the custom-offset
idiom, so the patch cannot move it:

```
GMT+05:30 -> sun.util.calendar.ZoneInfo raw=19800000
UTC       -> sun.util.calendar.ZoneInfo raw=0
Etc/GMT-3 -> sun.util.calendar.ZoneInfo raw=10800000
```

`parse` was fixed by C12-1 §1 and is untouched here.

**This is not a criticism of the fixture** (`C19-2`). Its mutants model the
defect at the Java seam (`fmt()`), where it is unconditional; in the VM the
defect sits behind a lazily-armed gate and a self-check. *A fixture that models
a defect's EFFECT can read green on a VM that has the defect and a safety net.*

### 4a. The observable that DOES move — and the number to check

The stderr line at `:1071` is unconditional. PREDICTED counts when
`RSimpleDateFormatZone` runs with stderr captured:

| block | formats that reach the fast path | PREDICTED `date-format fast path DISABLED` lines, before | after |
|---|---|---|---|
| `control` | 5 (2nd+ per formatter: none — one format each) | 0 | 0 |
| `fmtdate` | 0 (one format per fresh formatter) | 0 | 0 |
| `routes` | 10 (the 4th call in each of 10 iterations) | **10** | 0 |
| `roundtrip` | 0 | 0 | 0 |
| `parse` | 0 (no `format` calls) | 0 | 0 |
| `dstrule` | 1 (the 2nd of two formats) | **1** | 0 |
| **total** | | **11** | **0** |

**`11 -> 0` is the gate for this patch**, and it is cheap: run the fixture and
grep stderr. 11 is an upper bound — a `compile_pattern` refusal on
`"yyyy-MM-dd HH:mm:ss Z"` would lower it without meaning anything — so treat a
smaller non-zero "before" as consistent and **any non-zero "after" as a
failure**.

## 5. CORRECTING `D3-1` residual 4 — the memo key is not itself a defect

`D3-1` residual 4 proposes adding the zone's identity to the plan-staleness test
as "the general fix". Reading `gather`'s position in the entry point (`:951`,
before the `LAST_PLAN` steady-state check at `:968`) shows **the offset is
recomputed on every single format** and is never memoized; only the compiled
pattern pieces, the symbols, and the set of verified shapes are. So a stale memo
across a `setTimeZone` produces a wrong answer **only while some arm's offset
computation is wrong**. With this patch both arms are correct for their
populations, and §3a's vector answers correctly *through the memo*.

The memo key is therefore an **amplifier that hid this defect from the
cross-check**, not a second defect. Adding the zone identity to the staleness
test would cost a hot-path check on every formatter in the VM and buy nothing
measurable today. **Not nominated.** If a future arm is ever added to
`vm_implemented`, this paragraph is the reason that arm needs its own proof
rather than the cross-check's.

## 6. Cost

Per `format` on a `SimpleTimeZone` receiver: one virtual call instead of a tzdb
lookup, on an arm previously ~400-760 ns cheaper (the in-file measurement).
**Net WIN for the affected population**, because those formatters are `poisoned`
today and lose the fast path entirely; after the patch they keep it and pay one
call. `ZoneInfo` — what `TimeZone.getTimeZone` returns for every id, i.e. the
overwhelmingly common case and the only one the original measurement was taken
on — is untouched. `control` (8 checks) is the block that confirms it.

## 7. NOMINATIONS

**Line endings, checked before writing these.** `date_format_fast.rs` is
uniformly CRLF; **both nomination targets below are uniformly LF**
(`lib.rs`, and `RSimpleDateFormatZone.java` at 0 CRLF / 540 LF). Apply them with
LF endings — a Windows-side paste that lands CRLF into either file shows up as a
whole-file diff. Each OLD block below was counted against the working tree and
occurs **exactly once**.

### N1 — `native-builtins/src/lib.rs` (a stale comment, now false)

At `:20350`-`:20355`. The comment claims both concrete classes are registered;
C12-1 deleted one of them and this record deletes the corresponding fast arm.

REPLACE:

```
    // `SimpleTimeZone`) goes through the generic `TimeZone.getOffset(long)`.
    // Both concrete classes are covered below so it doesn't matter which one
    // `alloc_synth_timezone` constructed for a given zone id.
```

WITH:

```
    // `SimpleTimeZone`) goes through the generic `TimeZone.getOffset(long)`.
    // Only `ZoneInfo` and the abstract `TimeZone` itself are registered below:
    // C12-1 removed `java/util/SimpleTimeZone`, whose id is an opaque LABEL and
    // whose offset is the caller's `rawOffset`, and `alloc_synth_timezone` no
    // longer constructs one for any id. See
    // `docs/known-issues/jdk-only/E1-1-simpledateformat-format-zone-arm-landed.md`.
```

### N2 — `regression-suite/src/RSimpleDateFormatZone.java` (the vector that DOES distinguish)

The fixture is listed in `run.sh` `CORE_CLASSES` (confirmed) and is good coverage
of the CONTRACT, but §4 shows it is blind to this patch in both directions. Add a
seventh family `memo` driving §3a's shape collision. It needs no new expected
strings and carries its own mutation check: **remove the two warm-up formats and
it goes green on a broken VM** — the property `fmtdate` lacks. The two answers
below are MEASURED on HotSpot in §3a.

Insert before `static final String[] FAMILIES`:

```java
    // -----------------------------------------------------------------------
    // 7. memo -- TARGETS THE FORMAT PATH THROUGH THE PER-FORMATTER MEMO.
    //
    // The other format blocks cannot see this defect: a fresh formatter's
    // first format declines the VM's fast path (its `zeroDigit` field is not
    // armed yet) and the second is cross-checked against bytecode, which
    // corrects it. This block defeats both. It warms ONE formatter under a
    // zone the VM answers correctly for, so the shape is verified, then swaps
    // in a SimpleTimeZone with the SAME offset and a contradicting id: the
    // shape key is unchanged, the memo hits, and no cross-check runs. Removing
    // the two warm-up formats makes this block green on a broken VM, which is
    // its mutation check.
    // -----------------------------------------------------------------------
    static void memo() {
        SimpleDateFormat f = new SimpleDateFormat(PAT, Locale.US);
        f.setTimeZone(TimeZone.getTimeZone("Etc/GMT-3"));
        check(TimeZone.getTimeZone("Etc/GMT-3").getOffset(JAN) == 10800000,
                "memo: Etc/GMT-3 must be +03:00 at JAN");
        String warm1 = fmt(f, new Date(JAN));
        String warm2 = fmt(f, new Date(JAN));
        check("2021-01-15 15:00:00 +0300".equals(warm1), "memo: warm-up 1, got " + warm1);
        check("2021-01-15 15:00:00 +0300".equals(warm2), "memo: warm-up 2, got " + warm2);
        notVacuous("memo", 10800000, "Asia/Tokyo", JAN);
        f.setTimeZone(new SimpleTimeZone(10800000, "Asia/Tokyo"));
        String after = fmt(f, new Date(JAN));
        ob("memo-after-zone-swap", after);
        check("2021-01-15 15:00:00 +0300".equals(after),
                "memo: after setTimeZone(new SimpleTimeZone(10800000, \"Asia/Tokyo\")) the SAME"
                        + " formatter must still answer from the caller's rawOffset, got \"" + after
                        + "\" -- a per-(formatter, shape) memo served a zone it never checked");
        sectionEnd("memo", 6);
    }

```

REPLACE:

```java
    static final String[] FAMILIES = {
        "control", "fmtdate", "routes", "roundtrip", "parse", "dstrule",
    };
```

WITH:

```java
    static final String[] FAMILIES = {
        "control", "fmtdate", "routes", "roundtrip", "parse", "dstrule", "memo",
    };
```

REPLACE:

```java
        } else if ("dstrule".equals(name)) {
            dstrule();
        } else {
```

WITH:

```java
        } else if ("dstrule".equals(name)) {
            dstrule();
        } else if ("memo".equals(name)) {
            memo();
        } else {
```

This takes the fixture from 109 to 115 checks. **Run it on HotSpot before
trusting it** — this lane executed the underlying numbers (§3a) but not the
fixture.

## 8. What the orchestrator must run

1. **Build.** `simple_tz_class` is gone from the struct, the initializer and the
   predicate together; a partial application would not compile, which is the
   cheapest possible first gate.
2. **`RSimpleDateFormatZone`, stderr captured.**
   `grep -c 'date-format fast path DISABLED'` must be **0** (§4a). All six
   blocks must stay green — `control` especially: if it reds, the fix
   over-reached onto the `ZoneInfo` population.
3. **`RSimpleTimeZoneRaw`** (`checks=104`) must stay green — it guards C12-1 §1,
   which this patch depends on and does not re-verify.
4. **N2's `memo` family**, once added: this is the only vector that distinguishes
   before from after.
5. **Any `ZoneInfo`-heavy throughput run** (H2, Spring) as a no-regression check
   on §6's claim that the common case is untouched.

## Residuals

1. **`sun/util/calendar/ZoneInfo` keeps the same shape of exposure** (C12-1
   residual 1, unchanged): its public `(String, int)` constructor could build a
   `ZoneInfo` whose `rawOffset` contradicts its id, and that receiver still
   takes the tzdb arm. Not reachable without `--add-exports`; not measured.
2. **The stderr line is a free instrument nobody has read.** If any stored corpus
   log carries `date-format fast path DISABLED`, that is this defect firing with
   the pattern and both answers printed. Grepping the archive would turn §3 from
   a source reading into a measurement, and would also give the true "before"
   number for §4a's table.
3. **This lane could not build or run.** §2 is a source reading of two dispatch
   gates and §4a is a hand-simulation of the fixture; both are falsifiable by
   step 2 above in one run.
