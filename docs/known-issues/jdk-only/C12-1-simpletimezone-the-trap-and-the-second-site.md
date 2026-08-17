# C12-1 — the `SimpleTimeZone` unregistration: the base-class trap answered, and a SECOND site that the unregistration does not reach

**2026-08-12, lane C12.** Takes the handover from
`C6-2-simpletimezone-id-resolved-instead-of-rawoffset.md` and lands both of its
nominations. This record adds three things C6-2 could not:

1. **the base-class trap is answered from the dispatch source**, not guessed —
   and the answer is "the base registration does NOT capture a real
   `SimpleTimeZone`", for a reason that is checkable in one `javap` and two
   files;
2. **a SECOND site with the identical defect that the unregistration cannot
   fix**, because it never goes through native dispatch at all —
   `date_format_fast.rs:806`. This is where the bc-java idiom C6-2 §D quotes
   actually lands, so without it that row stays wrong after this change;
3. **C6-2 §D's predicted number is corrected** by measurement — the skew is
   7,200,000 ms, not 10,800,000, and the reason it is not the standard offset
   is itself informative.

**This lane could not build or run the VM.** Every HotSpot number was executed
on this host against HotSpot 25.0.3+9-LTS; every CratonVM value is **PREDICTED**
and marked. Registry facts are read from `scratchpad/p1/reg.json`
(`--dump-native-registry`, 11,748 natives), not from source order.

---

## 1. What landed, in `native-builtins/src/lib.rs` (this lane's file)

**3B — the registration is gone.**
`register_tzdb_offset_natives_for(registry, "java/util/SimpleTimeZone")` is
deleted, and the site now carries the reasoning below so it cannot be
re-added by someone reading only the `ZoneInfo` line above it.

**3A — `alloc_synth_timezone`'s `tz_dst_rule` branch is gone**, so
`TimeZone.getTimeZone(id)` always takes the `ZoneInfo` path. Both halves were
required and the order matters: 3B alone would leave the fabricated
`SimpleTimeZone` answering from its own (approximate, ~20-zone) constructor
arguments; 3A alone changes nothing observable.

`tz_dst_rule` and `dst_start_year` are now **unreferenced**. They are left in
place so this is one behaviour change rather than two, with a comment saying
they have no callers and must not acquire new ones. `dead_code = "allow"` is set
workspace-wide (`Cargo.toml:120`), so this costs no warning. Deleting them is a
clean follow-up for anyone in this file.

## 2. THE TRAP — answered, and the answer is "no capture"

C6-2 refused to pick between two readings of the comment on the
`java/util/TimeZone` registration three lines below. Neither reading was
necessary: the dispatch code states the rule outright.

**`NativeMethodRegistry::find` does no hierarchy walk** — it is an exact
`(class, method, descriptor)` lookup (`native-api/src/registry.rs:7334`). The
inheritance interception comes entirely from the caller, and
`native-builtins/tests/shim_inheritance_guard.rs:10-25` — a CI gate whose whole
existence depends on getting this right — quotes the caller's shape verbatim:

> `try_stackless_invoke` … passes the RECEIVER's class name, and **when that
> class does not declare the method itself** it climbs the receiver's
> superclass chain.

Three facts close it:

| fact | where |
|---|---|
| for an invokevirtual, `class_name` is the **receiver's runtime class**, taken from `args[0]`'s header — not the call site's CP class | `vm/src/runtime/interpreter/invoke.rs:1051`ff (`invoke_class`) |
| `walk_native_hierarchy` is `false` for every virtual/special call, and the climb is then skipped outright: `if has_own_bytecode { return None; }` | `invoke.rs:2346` (the `false` argument) and `invoke.rs:3271`-3279 |
| the late native-override re-check (step 6) keys on the **declaring class of the resolved bytecode method**, not the call site's | `invoke.rs:3756`-3800 (`class_name_arc`) |

And the receiver **does** declare the methods. `javap -p --module java.base
java.util.SimpleTimeZone`, JDK 25, executed on this host:

```
  public int getOffset(long);
  int getOffsets(long, int[]);
  public int getOffset(int, int, int, int, int, int);
  public int getRawOffset();
  public boolean inDaylightTime(java.util.Date);
```

All three live members of the registered family — `getRawOffset()I`,
`getOffset(J)I`, `getOffsets(J[I)I` — are **declared on `SimpleTimeZone`
itself, with code**. So with the exact-class registration removed, each one
resolves to its own bytecode and `java/util/TimeZone`'s copy is never consulted,
on either the pre-check or the step-6 path. A `TimeZone`-typed call site does
not change this: the static type never reaches the registry.

**`getOffsetsByWall(J[I)I` is the one member `SimpleTimeZone` does not declare**,
and it is the one member that WOULD reach the base through the climb. It cannot
be invoked on a `SimpleTimeZone`: the only caller is `GregorianCalendar`, which
downcasts to `ZoneInfo` first. The registry dump agrees it is dead on the base
too — `java/util/TimeZone.getOffsetsByWall` reads
`real_declaring_method: {loaded: true, declared: false}`, and
`scripts/baselines/jdk-only-dead-everywhere.tsv:163` already records the
`SimpleTimeZone` twin as `method-nowhere`.

**The base registration keeps a real constituency**, which is why it stays: a
receiver whose class has NO bytecode for these methods. That is the
synthetic-JDK-mode stub, and the `ZoneInfo`-less fallback object
`alloc_synth_timezone`'s tail allocates against `java/util/TimeZone` itself. In
those cases `has_own_bytecode` is false, the climb runs, and tzdb answers —
which is the correct answer for an object whose id IS the zone. **Per mode, the
question has different answers and both are served.**

**This is a source reading, and it must still be confirmed by a run** — see §6,
step 1. What it is not is a coin flip: the two readings C6-2 declined to choose
between are not equally supported, and the code says which.

### 2b. The registry dump, on the deletion itself

Four rows, all `owns_slot: true`, all `overwrote: null`, all registered from the
one helper — so deleting the single call removes all four and nothing else
refills the slots:

```
java/util/SimpleTimeZone getOffset        (J)I    owns_slot=true overwrote=null
java/util/SimpleTimeZone getOffsets       (J[I)I  owns_slot=true overwrote=null
java/util/SimpleTimeZone getOffsetsByWall (J[I)I  owns_slot=true overwrote=null
java/util/SimpleTimeZone getRawOffset     ()I     owns_slot=true overwrote=null
```

The same dump independently confirms the base is abstract:
`java/util/TimeZone.getRawOffset ()I` carries
`real_declaring_method: {loaded: true, declared: true, has_code: false}`.

**What the dump CANNOT settle**, and the reason §6 step 1 is not optional: every
`java/util/SimpleTimeZone` row reads `loaded: false` and `invocations: 0`. That
run never loaded the class, so the dump has nothing to say about which body a
real receiver reaches.

## 3. THE SECOND SITE — `date_format_fast.rs:806`, which no unregistration reaches

`native-builtins/src/date_format_fast.rs` registers an intrinsic for
`java/text/DateFormat.format(Ljava/util/Date;)Ljava/lang/String;` and computes
the zone offset **in Rust, without dispatching**:

```rust
    let vm_implemented = Some(zone_class) == sl.zoneinfo_class
        || Some(zone_class) == sl.simple_tz_class      // <- this line
        || Some(zone_class) == sl.timezone_class;
    let offset_ms = if vm_implemented {
        let (rules, id) = zone_rules_cached(ctx, sl, zone)?;   // reads the `ID` FIELD
        ...
    } else {
        ctx.invoke_virtual(zone, "getOffset", "(J)I", ...)     // the correct arm
    };
```

`zone_rules_cached` reads the zone's **`ID` field** and resolves it against
tzdb — the identical mistake, in the identical shape, one layer below dispatch.
The comment on `Slots::simple_tz_class` names the reason it is there: *"The
three concrete `TimeZone` classes whose `getOffset(long)` this VM itself
implements (`register_tzdb_offset_natives_for`)"*. **That premise is exactly
what §1 deletes.** After this change no VM code implements
`SimpleTimeZone.getOffset` and no VM code fabricates a `SimpleTimeZone`, so
every object reaching that line by the `simple_tz_class` arm is, by
construction, one the APPLICATION built — the population whose `ID` is an opaque
label.

**It is reached the same way the natives were:** `SimpleDateFormat` does not
declare `format(Date)` (it is `DateFormat`'s), so the receiver-class lookup
misses, the climb runs, and `java/text/DateFormat`'s intrinsic wins. It is
`format` only; `parse` is not intercepted, which is why the two halves of §4
diverge.

### The oracle (`scratchpad/c12/C12Probe.java`, HotSpot 25.0.3+9-LTS)

Fixed instants `JAN = 1610712000000L` (2021-01-15T12:00:00Z) and
`JUL = 1626350400000L` (2021-07-15T12:00:00Z); pattern
`"yyyy-MM-dd HH:mm:ss Z"`, `Locale.US`; **no host default zone is read**. Every
row pairs a `rawOffset` with an id whose true offset differs from it — no
vacuity, per C6-2's warning about `(-18000000, "America/New_York")`.

| `new SimpleTimeZone(raw, id)` | instant | HotSpot | CratonVM **PREDICTED** |
|---|---|---|---|
| `(0, "America/Sao_Paulo")` | JAN | `2021-01-15 12:00:00 +0000` | `2021-01-15 09:00:00 -0300` |
| `(0, "America/New_York")` | JAN | `2021-01-15 12:00:00 +0000` | `2021-01-15 07:00:00 -0500` |
| `(18000000, "America/Sao_Paulo")` | JUL | `2021-07-15 17:00:00 +0500` | `2021-07-15 09:00:00 -0300` |
| `(-18000000, "Europe/Berlin")` | JUL | `2021-07-15 07:00:00 -0500` | `2021-07-15 14:00:00 +0200` |

The PREDICTED column is measured, not imagined: it is what the same program
prints when the `SimpleTimeZone` is replaced by `TimeZone.getTimeZone(id)`,
which is precisely what `zone_rules_cached` computes. **That substitution is
also this vector's mutation check** — the two columns differ in all four rows,
including the offset TEXT (`Z`), so a probe built on it cannot read green by
accident.

## 4. C6-2 §D's number was wrong, and the reason matters

C6-2 predicted the bc-java parse idiom off by `10800000` ms (the zone's standard
offset). Measured:

```
PARSE 20020122122220 with SimpleTimeZone(0, "America/Sao_Paulo") -> 1011702140000   [HotSpot]
MUT   20020122122220 with TimeZone.getTimeZone("America/Sao_Paulo") -> 1011709340000
```

The gap is **7,200,000 ms (2 h), not 10,800,000 (3 h)**, because São Paulo was
observing daylight saving on 2002-01-22 (UTC−02:00), and the tzdb path applies
the historical transition for the instant rather than the standard offset. Two
consequences:

* **the skew is not a constant** — it is whatever the zone's total offset was at
  that instant, so a test that asserts a fixed delta will pass or fail depending
  on the date it picks (this project's *"a header field is a time series, not a
  constant"* rule, in the timezone domain);
* **this row is `parse`, not `format`**, so it is fixed by §1 alone: `parse`
  goes through `GregorianCalendar` → `TimeZone.getOffset` on the receiver → real
  `SimpleTimeZone` bytecode once the registration is gone. `format` needs the
  §5 nomination as well. **The bc-java-shaped defect therefore only half
  disappears with this change**, which is exactly the kind of partial fix that
  reads as "fixed" in a suite that only round-trips.

## 5. NOMINATION — `date_format_fast.rs` (not this lane's file)

**File:** `native-builtins/src/date_format_fast.rs`.

REPLACE (at `:806`):

```rust
    let vm_implemented = Some(zone_class) == sl.zoneinfo_class
        || Some(zone_class) == sl.simple_tz_class
        || Some(zone_class) == sl.timezone_class;
```

WITH:

```rust
    // `java/util/SimpleTimeZone` is deliberately NOT here — see
    // `docs/known-issues/jdk-only/C12-1-simpletimezone-the-trap-and-the-second-site.md`.
    // The fast arm below answers from the zone's `ID` FIELD via tzdb. That is
    // right for a `ZoneInfo` (whose id IS the zone) and for the abstract
    // `TimeZone` itself (an instance of the exact class can only be one this VM
    // fabricated). It is wrong for a `java.util.SimpleTimeZone`, whose id is by
    // contract an opaque LABEL and whose offset is the `rawOffset` its
    // constructor stored: `new SimpleTimeZone(0, "America/Sao_Paulo")` formats
    // an instant at -03:00 here and at +00:00 on HotSpot. Nothing in this VM
    // fabricates a `SimpleTimeZone` any more (`alloc_synth_timezone`) and
    // nothing implements its `getOffset` (`register_tzdb_offset_natives_for`),
    // so every such receiver is one the APPLICATION built. The `else` arm's
    // virtual call reaches its real bytecode and is correct for it.
    let vm_implemented = Some(zone_class) == sl.zoneinfo_class
        || Some(zone_class) == sl.timezone_class;
```

AND, in the same file, REPLACE:

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

AND REPLACE the field declaration:

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
    /// The `TimeZone` classes whose `getOffset(long)` this VM itself implements
    /// (`register_tzdb_offset_natives_for`). For those — and ONLY those — the
    /// offset can be read straight from the tzdb helper instead of dispatching
    /// into Java and paying a native-funnel entry. Any other receiver may be an
    /// application subclass with its own override, or a `SimpleTimeZone` whose
    /// id is an opaque label, so it keeps the virtual call. C12-1 removed
    /// `java/util/SimpleTimeZone` from this set; do not restore it without
    /// reading that record.
    zoneinfo_class: Option<cratonvm_types::ClassId>,
    timezone_class: Option<cratonvm_types::ClassId>,
```

**Cost of the change is one virtual call per `format` on this receiver shape**,
on the arm that was previously ~400-760 ns cheaper. `ZoneInfo` — which is what
`TimeZone.getTimeZone` returns, i.e. the overwhelmingly common case, and the
only one the measurement in that comment was taken on — is untouched.

## NOMINATION — `regression-suite/run.sh` (unchanged from C6-2, restated)

The fixture `regression-suite/src/RSimpleTimeZoneRaw.java` exists, passes on
HotSpot (`CK RSimpleTimeZoneRaw checks=104`), and is mutation-checked
(`red=20 / green=4`). It is still **unlisted**, so it compiles and never runs.

REPLACE (end of the `CORE_CLASSES` line):

```
RJdkFormatLocale RJdkStrictMath RJdkByteOrder RJdkIntrinsics"
```

WITH:

```
RJdkFormatLocale RJdkStrictMath RJdkByteOrder RJdkIntrinsics RSimpleTimeZoneRaw"
```

**That fixture does not cover §3.** It exercises the accessors, not
`SimpleDateFormat.format`, so it will read green while the second site is still
wrong. A `format`-shaped block using the four rows of §3 is the missing vector;
whoever adds it must keep the mutation check (replace the `SimpleTimeZone` with
`TimeZone.getTimeZone(id)` and watch it go red).

## 6. WHAT THE ORCHESTRATOR MUST RUN — in this order

**Step 1 — does the base capture the subclass? (the trap)**
```
cratonvm --jdk-only -cp regression-suite/classes RSimpleTimeZoneRaw
```
* `RESULT ... PASS`, `checks=104` → **§2 is confirmed**: the receiver's own
  bytecode won and the base registration did not capture it. Nothing further.
* the `MISMATCHED` block still red → **§2 is refuted** and the base IS capturing.
  The fix is then C6-2's step 3, in `register_tzdb_offset_natives_for`: guard all
  four bodies at the top so that when the receiver's class is
  `java/util/SimpleTimeZone` they run its own bytecode —
  `ctx.invoke_virtual_bytecode_only(this, "getRawOffset", "()I", &[])`, the
  helper already used a few hundred lines above for
  `SimpleTimeZone.setStartYear(int)`. **Do not add that guard pre-emptively**:
  if §2 holds it is unreachable code on the hot path of every `ZoneInfo` offset
  query.
* anything else red that was green before → check §7 residual 2 first.

**Step 2 — did anything regress on the zone path?** The `ZoneInfo` shape is what
3A now returns for every id, and the DST-boundary cases the deleted branch
existed for are the ones to watch:
```
cratonvm --jdk-only -cp <hibernate fixture> ...LocalDateTimeTest
```
The deleted branch's stated purpose was the `"expected 2018-10-28T01:00 but was
2018-10-28T00:00"` skew. tzdb carries full transitions and the approximation did
not, so this should improve or hold — **but that is an argument, not a
measurement**, and it is the one row where 3A could plausibly regress.

**Step 3 — the second site**, once §5 lands:
```
javac -d <out> scratchpad/c12/C12Probe.java && cratonvm --jdk-only -cp <out> C12Probe
```
Section A must match the HotSpot column of §3, not the PREDICTED one. Section A5
must print `1011702140000`. Sections B and C belong to C12-2.

**Step 4 — refresh the baselines.** Four rows in
`scripts/baselines/jdk-only-kind-map-25-linux.tsv:7255-7258` and one in
`scripts/baselines/jdk-only-dead-everywhere.tsv:163` describe registrations that
no longer exist. Neither file is read by a `cargo test` (checked: the only
in-tree readers are doc references and `vm/tests/stub_ratchet.rs`, which reads
`vm_init.rs`), so nothing goes red — they are simply stale until regenerated.

## 7. Residuals

1. **`sun/util/calendar/ZoneInfo` keeps the same shape of exposure**, one step
   further from reach. `ZoneInfo` has a public `(String, int)` constructor, so
   an application with `--add-exports` could build one whose `rawOffset`
   contradicts its id and get the same wrong answer. Not measured; not reachable
   from ordinary code, since the package is not exported.
2. **3A changes what `TimeZone.getTimeZone(id)` RETURNS** for the ~20 zones in
   the old allowlist — a `ZoneInfo` instead of a `SimpleTimeZone`. Code doing
   `instanceof SimpleTimeZone` on the result changes behaviour. It matches
   HotSpot (§4C of C6-2: `sun.util.calendar.ZoneInfo`, always), so the change is
   toward the oracle, but it is a behaviour change and step 2 is where it shows.
3. **`inDaylightTime` was never registered and is fixed only indirectly** — by
   `getOffset` becoming honest. If step 1 goes red, this one goes red with it,
   and it is the row that matters most: it is a self-contradictory object
   (`inDaylightTime()==true` while `useDaylightTime()==false`), not just a wrong
   number.
4. **The `getOffsets(J[I)I` package-private question from C6-2 residual 4 is now
   moot** for `SimpleTimeZone` (the registration is gone) but still stands for
   `java/util/TimeZone`, where the row survives and the method is package-private.
