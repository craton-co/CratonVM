# D3-1 — `date_format_fast.rs`'s zone arm: the patch is right, and the fixture that was supposed to prove it cannot

**2026-08-13, lane D3.** Applies wave D's first queue entry (`WAVE-D-QUEUE.md`
row 1, from `C12-1` §5). The patch below is apply-ready and verified byte-for-byte
against the working tree.

**This lane may not write `.rs` and may not build or run the VM.** Everything
below is a source reading plus HotSpot measurements taken on this host
(25.0.3+9-LTS). Two things in it correct the queue entry rather than confirming it.

---

## 0. Verdict, up front

| claim | verdict |
|---|---|
| the defect is real and the nominated shape is right | **CONFIRMED** |
| the `else` arm answers correctly once `SimpleTimeZone` is dropped | **CONFIRMED from the dispatch source** (§2) — this was the open risk and it holds |
| "`format` still ignores a caller's `rawOffset`" today | **TOO STRONG** (§3). The module's own run-time cross-check catches the divergence and returns the BYTECODE answer |
| `RSimpleDateFormatZone`'s `fmtdate`/`routes`/`roundtrip`/`dstrule` are RED for this defect | **WRONG** (§4). All six blocks read GREEN before AND after the patch. The fixture cannot distinguish |

So: **land the patch, and do not use `RSimpleDateFormatZone` as its gate.** §5
gives the vector that does distinguish.

## 1. The site, unchanged

`native-builtins/src/date_format_fast.rs:806` computes the zone offset in Rust
without dispatching, from the receiver's `ID` field via tzdb, whenever the zone's
class is one of `sun/util/calendar/ZoneInfo`, `java/util/SimpleTimeZone`,
`java/util/TimeZone`. For a `SimpleTimeZone` the id is by contract an opaque
LABEL, so that is the wrong number. C12-1 §1 already deleted the four
`java/util/SimpleTimeZone` tzdb natives (`lib.rs:20478` now carries the
reasoning in place of the registration) and `alloc_synth_timezone`'s
`tz_dst_rule` branch (`lib.rs:20018`), so nothing in this VM fabricates a
`SimpleTimeZone` and nothing implements its `getOffset` any more. Every receiver
reaching that arm is one the APPLICATION built. Both halves verified present in
the working tree.

## 2. THE OPEN RISK — what answers instead — is closed, in the dispatch source

The brief's condition for landing this was: *establish what answers once the
class leaves `vm_implemented`; if the fallback is also wrong, dropping it
converts one wrong answer into another.* The `else` arm is

```rust
ctx.invoke_virtual(zone, "getOffset", "(J)I", &[Value::Long(millis)])
```

and the route it takes is fully determined:

| step | where | what happens for a `java.util.SimpleTimeZone` receiver |
|---|---|---|
| `NativeContext::invoke_virtual` | `vm/src/vm/vm_exec.rs:9286` | not a lambda proxy; `class_name` is taken from the RECEIVER's class (`:9812`-`9841`, `resolved_from_receiver = true`) → `"java/util/SimpleTimeZone"` |
| `needs_exact_class_dispatch` | `:9986` | false (`method_name != "toString"`, and the global id for that name IS the receiver's) → falls to `invoke_or_native` |
| direct native lookup | `:16997` `find_with_kind(effective_class, …)` | **MISSES** — C12-1 §1 deleted all four `java/util/SimpleTimeZone` rows |
| superclass climb | `:17137`-`17149` | **SKIPPED.** `has_own_bytecode` is `cls.find_method("getOffset", "(J)I").is_some()`, and `javap -p --module java.base java.util.SimpleTimeZone` shows `public int getOffset(long)` declared on the class itself. So `java/util/TimeZone`'s surviving tzdb native does **not** capture it |
| result | | the real `SimpleTimeZone.getOffset(long)` bytecode: `rawOffset` plus the caller's own DST rule |

**The fallback is correct, and it is correct for the eleven-argument constructor
too** — `SimpleTimeZone.getOffset(long)` applies the caller's supplied rule.
This is the same conclusion C12-1 §2 reached for the interpreter's
`try_stackless_invoke`; the point of re-deriving it here is that the `else` arm
does **not** go through `try_stackless_invoke`, it goes through
`vm_exec::invoke_or_native`, which is a different function with its own
`has_own_bytecode` gate. Both gates say the same thing, for the same reason.

**Synthetic-JDK mode is unaffected, because the intrinsic never runs there.**
`slots()` (`:290`) returns `None` unless it can resolve, among others,
`java/text/SimpleDateFormat.forceStandaloneForm` and
`java/util/GregorianCalendar.gregorianCutover`. Neither name appears anywhere in
the synthetic class definitions (`grep -rn forceStandaloneForm --include=*.rs`
matches only this file), so `slots()` fails and `format_via_bytecode` runs. The
patch is a no-op in that mode.

## 3. WHY THE DEFECT IS QUIETER THAN THE QUEUE SAYS — the module cross-checks itself

`date_format_fast.rs`'s module doc states it, and the code does it: *"the first
time a given `(plan, month, weekday, era, am/pm, dst-flag, negative-offset)`
tuple is formatted, the native result is compared against `format(Date,
StringBuffer, FieldPosition)` run as bytecode. A mismatch returns the BYTECODE
answer and disables the fast path for that plan permanently."* (`:1049`-`1075`).

`format_via_bytecode` (`:686`) reaches the real bytecode via
`invoke_virtual_bytecode_only`, whose inner `GregorianCalendar.computeFields`
calls `zone.getOffset(millis)` through ordinary dispatch — i.e. through the
route §2 just traced. So:

* **before C12-1 §1**, the bytecode reference ALSO answered from the id (the
  natives were registered), the two arms agreed, and the wrong answer was
  verified and published. That is the state C12-3-era records describe.
* **today, with §1 landed and §5 not**, the two arms DISAGREE. The cross-check
  fires, `return Ok(reference)` hands back the correct bytecode answer, the
  formatter is `poisoned` for good, and an unconditional line goes to stderr:

  > `[cratonvm] date-format fast path DISABLED for pattern "…": produced "…" but the JDK produced "…"; falling back to bytecode for this formatter`

**So the ANSWER a caller sees is already right, and the residual damage is
threefold:** the alarming stderr line (whose own comment says "a divergence here
is a real defect in this module" — it is not, it is this zone bug); the
permanent loss of the 557x fast path for that formatter, including for later
formats with a perfectly good `ZoneInfo`; and the hole in §3a.

### 3a. The hole the cross-check does not cover, and it is the real reason to land this

`verified` is keyed by `(formatter identity, shape)` and is invalidated only by a
change of `pattern` or `DateFormatSymbols` identity (`:1007`-`1013`).
**`setTimeZone` invalidates nothing.** So a formatter that has already verified
shape *S* under one zone takes the fast path for *S* under a different zone with
**no cross-check at all**. Constructed instance, all values checked by hand
against the shape key at `:922`:

```java
SimpleDateFormat f = new SimpleDateFormat("yyyy-MM-dd HH:mm:ss Z", Locale.US);
f.setTimeZone(TimeZone.getTimeZone("Etc/GMT-3"));   // +03:00, a ZoneInfo
f.format(new Date(1610712000000L));                 // 1st: declines (zeroDigit==0)
f.format(new Date(1610712000000L));                 // 2nd: fast path, cross-checked, VERIFIED
                                                    //   -> 2021-01-15 15:00:00 +0300
f.setTimeZone(new SimpleTimeZone(10800000, "Asia/Tokyo"));
f.format(new Date(1610712000000L));                 // shape key IDENTICAL -> NO cross-check
```

Correct answer `2021-01-15 15:00:00 +0300`; the fast arm answers
`2021-01-15 21:00:00 +0900`. Shape key both ways is
`(month0=0, dow=FRI, era=0, pm=true, offset!=0=true, offset<0=false)` — the wrong
offset lands on the same tuple, so the memo hit is exact. **That is a silent
wrong answer with no stderr line and no fallback**, and the patch is what removes
it.

The same hole is reachable across threads: `LAST_PLAN` publishes a copy of the
shared `verified` set, and `needs_check = entry.verified.insert(shape)` is on the
shared map, so a shape another thread verified is skipped here too.

## 4. WHY `RSimpleDateFormatZone` CANNOT GATE THIS PATCH

The brief asked which of its blocks the patch flips. The honest answer is
**none**, for two independent reasons, either of which alone is sufficient.

**(a) The first format on any fresh formatter never reaches the fast path.**
`gather` (`:762`) declines unless `zeroDigit == '0'`, and the JDK sets that field
lazily inside `zeroPaddingNumber` on first use. Measured on this host:

```
$ java --add-opens java.base/java.text=ALL-UNNAMED -cp . ZD
fresh zeroDigit=0
fresh forceStandaloneForm=false
format1=2021-01-15 12:00:00 +0000
after1 zeroDigit=48
cal class=java.util.GregorianCalendar zone=java.util.SimpleTimeZone
```

`fmtdate` and `roundtrip` build a fresh `SimpleDateFormat` per row (`sdf(...)`)
and format it exactly once, so **every one of those twenty formats runs the
bytecode**, defect or no defect.

**(b) Where the fast path does engage, the cross-check corrects it.** `routes`
formats `f` four times, of which only the first and fourth are the intercepted
`format(Date)` descriptor; the fourth engages the fast path on a fresh shape →
cross-check → mismatch → bytecode answer. `dstrule` formats twice: first
declines, second is cross-checked. Both blocks end up asserting the correct
strings, with two stderr lines as the only trace.

`control` and `parse` were never in question.

**This is not a criticism of C19-2** — the fixture is well built and its three
mutants do exactly what they claim. It is that the mutants model the defect at
the Java seam (`fmt()`), where it is unconditional, while in the VM the defect
sits behind a lazily-armed gate and a self-check. **A fixture that models a
defect's EFFECT can read green on a VM that has the defect and a safety net.**

## 5. THE VECTOR THAT DOES DISTINGUISH

Add to `RSimpleDateFormatZone` a seventh family, `memo`, driving §3a's shape
collision. It needs no new expected strings — the two answers are already in the
file's tables — and it has an intrinsic mutation check: **remove the two warm-up
formats and it goes green on a broken VM**, which is the property `fmtdate` lacks.

```java
static void memo() throws ParseException {
    SimpleDateFormat f = new SimpleDateFormat(PAT, Locale.US);
    f.setTimeZone(TimeZone.getTimeZone("Etc/GMT-3"));            // +03:00, id IS the zone
    check(TimeZone.getTimeZone("Etc/GMT-3").getOffset(JAN) == 10800000,
            "memo: Etc/GMT-3 must be +03:00 at JAN");
    String warm1 = fmt(f, new Date(JAN));                        // arms zeroDigit
    String warm2 = fmt(f, new Date(JAN));                        // VERIFIES the shape
    check("2021-01-15 15:00:00 +0300".equals(warm1), "memo: warm-up 1, got " + warm1);
    check("2021-01-15 15:00:00 +0300".equals(warm2), "memo: warm-up 2, got " + warm2);
    notVacuous("memo", 10800000, "Asia/Tokyo", JAN);
    f.setTimeZone(new SimpleTimeZone(10800000, "Asia/Tokyo"));   // same offset, different id
    String after = fmt(f, new Date(JAN));
    ob("memo-after-zone-swap", after);
    check("2021-01-15 15:00:00 +0300".equals(after),
            "memo: after setTimeZone(new SimpleTimeZone(10800000, \"Asia/Tokyo\")) the SAME"
                    + " formatter must still answer from the caller's rawOffset, got \"" + after
                    + "\" -- a per-(formatter, shape) memo served a zone it never checked");
    sectionEnd("memo", 6);
}
```

`2021-01-15 15:00:00 +0300` is the HotSpot answer for both zones (the offsets are
equal by construction); the broken VM prints `2021-01-15 21:00:00 +0900`. **Run
it on HotSpot before trusting the number** — this lane could not execute the
fixture, only the `zeroDigit` probe above.

A cheaper, non-fixture check that also distinguishes: run
`RSimpleDateFormatZone` with stderr captured and assert **zero**
`date-format fast path DISABLED` lines. Before the patch there are two (`routes`,
`dstrule`); after it there are none.

## 6. THE PATCH — `native-builtins/src/date_format_fast.rs`

**The file is uniformly CRLF (1,217 `\r\n`, 1,217 `\n`).** Every OLD block below
was verified with `str.count()` against the working tree to occur **exactly
once**, and the three are disjoint (applying them in order leaves each of the
later anchors still unique). Apply in the order given.

### D1 — the field declaration (`:255`-`:263`)

REPLACE:

```rust
    /// The three concrete `TimeZone` classes whose `getOffset(long)` this VM
    /// itself implements (`register_tzdb_offset_natives_for`). For those — and
    /// ONLY those — the offset can be read straight from the tzdb helper
    /// instead of dispatching into Java and paying a native-funnel entry. Any
    /// other receiver may be an application subclass with its own override, so
    /// it keeps the virtual call.
    zoneinfo_class: Option<cratonvm_types::ClassId>,
    simple_tz_class: Option<cratonvm_types::ClassId>,
    timezone_class: Option<cratonvm_types::ClassId>,
```

WITH:

```rust
    /// The `TimeZone` classes whose `getOffset(long)` this VM itself
    /// implements (`register_tzdb_offset_natives_for`). For those — and ONLY
    /// those — the offset can be read straight from the tzdb helper instead of
    /// dispatching into Java and paying a native-funnel entry. Any other
    /// receiver may be an application subclass with its own override, or a
    /// `java.util.SimpleTimeZone` whose id is an opaque LABEL rather than a
    /// zone, so it keeps the virtual call. C12-1 removed
    /// `java/util/SimpleTimeZone` from this set; do not restore it without
    /// reading `docs/known-issues/jdk-only/D3-1-simpledateformat-format-zone-arm.md`.
    zoneinfo_class: Option<cratonvm_types::ClassId>,
    timezone_class: Option<cratonvm_types::ClassId>,
```

*(The two `—` are U+2014 EM DASH in both the old and the new text, as in the file.)*

### D2 — the initializer (`:305`-`:307`)

REPLACE:

```rust
            zoneinfo_class: ctx.class_id_by_name("sun/util/calendar/ZoneInfo"),
            simple_tz_class: ctx.class_id_by_name("java/util/SimpleTimeZone"),
            timezone_class,
```

WITH:

```rust
            zoneinfo_class: ctx.class_id_by_name("sun/util/calendar/ZoneInfo"),
            timezone_class,
```

### D3 — the arm itself (`:806`-`:808`)

REPLACE:

```rust
    let vm_implemented = Some(zone_class) == sl.zoneinfo_class
        || Some(zone_class) == sl.simple_tz_class
        || Some(zone_class) == sl.timezone_class;
```

WITH:

```rust
    // `java/util/SimpleTimeZone` is deliberately NOT here — see
    // `docs/known-issues/jdk-only/D3-1-simpledateformat-format-zone-arm.md`.
    // The fast arm below answers from the zone's `ID` FIELD via tzdb. That is
    // right for a `ZoneInfo` (whose id IS the zone) and for the abstract
    // `TimeZone` itself (an instance of that exact class can only be one this
    // VM fabricated). It is wrong for a `java.util.SimpleTimeZone`, whose id is
    // by contract an opaque LABEL and whose offset is the `rawOffset` its
    // constructor stored: `new SimpleTimeZone(0, "America/Sao_Paulo")` formats
    // an instant at -03:00 here and at +00:00 on HotSpot. Nothing in this VM
    // fabricates a `SimpleTimeZone` any more (`alloc_synth_timezone`) and
    // nothing implements its `getOffset` (`register_tzdb_offset_natives_for`),
    // so every such receiver is one the APPLICATION built. The `else` arm's
    // virtual call reaches its real bytecode, which is correct for it:
    // `SimpleTimeZone` DECLARES `getOffset(J)I` with code, so
    // `invoke_or_native`'s `has_own_bytecode` gate skips the superclass climb
    // and `java/util/TimeZone`'s tzdb native does not capture it either.
    let vm_implemented = Some(zone_class) == sl.zoneinfo_class
        || Some(zone_class) == sl.timezone_class;
```

After D1+D2+D3 the identifier `simple_tz_class` no longer appears in the file
(it had exactly three occurrences: `:262`, `:306`, `:807`), so nothing is left
dangling and no lint fires. `tz_id` and `timezone_class` are still read
(`zone_rules_cached`, `slots()`), so they stay.

## 7. Cost, and what it is NOT

Per `format` on a `SimpleTimeZone` receiver: one virtual call in place of a tzdb
lookup, on an arm previously ~400-760 ns cheaper. **This is a net WIN for the
affected population today**, because those formatters are currently `poisoned`
by the cross-check and lose the fast path entirely; after the patch they keep it
and pay one call. `ZoneInfo` — what `TimeZone.getTimeZone` returns, i.e. the
overwhelmingly common case and the only one the original measurement was taken
on — is untouched, which is what `control` (8 checks) exists to confirm.

## Residuals

1. **`RSimpleDateFormatZone` should NOT be cited as evidence for this patch**
   in either direction. It is good coverage of the CONTRACT and a real guard
   against a regression of C12-1 §1 (`parse`), but it is blind to §5's memo hole
   and blind to the fast path on any formatter it uses once. §5's `memo` family
   is the missing vector and this lane could not execute it.
2. **The stderr line is the currently-available instrument** and nobody has read
   it. If any stored corpus log carries
   `date-format fast path DISABLED`, that is this defect firing, with the
   pattern and both answers printed. Grepping the archive costs nothing and
   would turn every claim in §3 from a source reading into a measurement.
3. **`sun/util/calendar/ZoneInfo` keeps the same shape of exposure** (C12-1
   residual 1, unchanged): its public `(String, int)` constructor could build a
   `ZoneInfo` whose `rawOffset` contradicts its id. Not reachable without
   `--add-exports`; not measured.
4. **The cross-check's memo key is the underlying weakness** and this patch only
   removes one way to reach it. `verified` survives `setTimeZone`,
   `setCalendar`, and `set2DigitYearStart`; only `pattern` and
   `DateFormatSymbols` identity invalidate it. Adding the zone's identity hash
   to the plan-staleness test is the general fix, it is three lines, and it is
   NOT part of this patch because it changes a hot-path check for every
   formatter in the VM and deserves its own measurement.
