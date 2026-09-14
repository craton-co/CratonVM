# G23-1 — the nominations that needed `lib.rs`, and the three that turned out to be wrong

**Status:** MIXED. **The vector rewrite is FIXED-MEASURED on both VMs, before
and after.** The four Rust edits are applied and SOURCE-VERIFIED, and each has
a MEASURED "before"; none has a measured "after", because this lane was
forbidden to build (§7). Three of the nominations it was handed were
**falsified by measurement** and are not implemented as written — §2, §4 and
§5 say which, and why implementing them as written would have been wrong.

**Provenance.** Oracle: HotSpot 25.0.3+9-LTS at
`C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot`. VM under test:
`C:/craton/target-fcheck/release/cratonvm.exe`, built from `d2e127930` —
which is now **two commits behind HEAD** (`07a494e1e`), so anything another
lane landed in `logmanager.rs` or `reflect_annotations.rs` since is NOT in it.
`CRATONVM_DISABLE_DEFAULT_WATCHDOG=1` throughout. Probes:
`{ItlProbe,JulN,JulN2,JulN3,TzGen,DstSav}.java`, reproduced or described in
place below.

**Files this lane owns and touched:** `native-builtins/src/lib.rs`,
`regression-suite/src/RSimpleTimeZoneRaw.java`. Nothing else.

---

## 0. The headline

| item | before (MEASURED) | after |
|---|---|---|
| `RSimpleTimeZoneRaw`, lines surviving `extract()` on CratonVM | **0** — it threw on the first divergence | **469** (both modes) |
| same, on HotSpot | **2**, and both are constants the fixture computes about itself | **396**, `fails=0`, `PASS` |
| `RSimpleTimeZoneRaw` discriminating power | `104` compared against `104` | **393 checks, 74 diverging, 319 matching** |
| `InheritableThreadLocal` capture timing, 10 cases | **8 of 10 wrong**, identically in both modes | 9 sites patched; PREDICTED |
| `Logger.setParent(null)` / `new LogRecord(null,"m")`, Compatible | RETURNED; HotSpot throws | fixed; PREDICTED |
| `Logger.severe(Supplier)` null axis, 13 arms | Compatible **8 of 13 wrong**, `--jdk-only` **0 of 13 wrong** | **no edit — already fixed at HEAD**, §5 |
| `alloc_synth_timezone`'s `rawOffset` FIELD | seeded from a ~50-entry hand table | seeded from `tzdb`; PREDICTED |

**The single most useful thing in this record is §2**: three of the six
nominations this lane was handed state something the binary disagrees with.
All three were written by lanes that could not run it, which is exactly what
HANDOFF-20260814 §2 says to expect.

---

## 1. `RSimpleTimeZoneRaw` — the fixture that compared 104 against 104

### 1.1 Two independent blindnesses, and the second survived the first

**MEASURED, before, on the `d2e127930` binary:**

```
OLD vector, lines surviving extract():
  HotSpot                 2
  CratonVM  Compatible    0
  CratonVM  --jdk-only    0
```

Two separate defects produce those numbers.

**The zero.** The fixture reported through `throw new AssertionError(...)`. On
CratonVM it died inside `realZonesStillResolve()` at
`New_York inDaylightTime(JUL)` — **before printing anything at all**, because
the two `System.out.println` calls are the last two statements of `main`. So
the cross-VM diff had nothing to compare.

**The two.** Even on the green path the fixture printed exactly:

```
CK RSimpleTimeZoneRaw checks=104
PASS RSimpleTimeZoneRaw (104 checks)
```

Both are constants the fixture computes **about itself**. Not one answer any
VM gave ever reached the comparison. That is `harness-guard.sh`'s G2 verbatim:
"a constant always matches itself". Fixing the throwing would have left this
one standing.

### 1.2 The rewrite

Every assertion now **publishes the VM's own answer on a `CK` line before it
compares**, nothing throws, failures are counted, and the `PASS` banner is
withheld when the count is non-zero — which is how the vector reports red now
that `run.sh`'s rc check cannot see it (`run.sh:601` greps `^PASS <Class>`;
no banner is a `FAIL "no PASS line"`).

Coverage grew from 104 checks to 393. The `SimpleTimeZone` half is preserved
row for row and the `TimeZone.getTimeZone(id)` half — the block the old
revision died inside — went from 2 zones and 3 observables to **32 zones and 8
observables**, plus an unknown-id fallback block and a six-arg discriminator
on a real database zone.

The 32 ids are chosen so that reading the answer off one rule cannot pass
them, and every expectation is MEASURED (generated from an oracle run by
`TzGen.java`, not written by hand):

* `Australia/Lord_Howe` saves **1800000** ms, `Antarctica/Troll` **7200000** —
  the rows that catch a hard-coded one-hour saving.
* `America/Mexico_City` has `useDaylightTime()==false` and
  `inDaylightTime(JUL 2021)==true`: DST was abolished in 2022, so the CURRENT
  rule and the HISTORICAL transition give opposite answers. `Asia/Tehran` is
  the same shape. `America/Godthab` carries it in the standard offset instead
  (`rawOffset` −02:00, `getOffset(JAN 2021)` −03:00).
* `Africa/Cairo` is in daylight time at NEITHER instant despite
  `useDaylightTime()==true` — DST returned in 2023, after both probe instants.
* five southern-hemisphere zones invert JAN/JUL; five zones carry half-hour or
  three-quarter-hour standard offsets; `Etc/GMT+5` carries the sign inversion.

### 1.3 After, MEASURED on both VMs

```
NEW vector:
  HotSpot                393 checks, fails=0, PASS,  396 lines, rc=0
  CratonVM  Compatible   393 checks, fails=74, no PASS, 469 lines, rc=0
  CratonVM  --jdk-only   393 checks, fails=74, no PASS, 469 lines, rc=0
```

**319 rows match and 74 diverge.** The oracle prints 396 lines and `extract()`
keeps 396 of them, so guard G1 has nothing to complain about.

The 74 are not scattered. Every one is in the DST family:

| observable | diverging zones |
|---|---|
| `getDSTSavings` | 18 |
| `useDaylightTime` | 18 |
| `observesDaylightTime` | 18 |
| `inDaylightTime` (JAN or JUL) | 19 |
| `getOffset(era,y,m,d,dow,ms)` on `America/New_York` | 1 |

and **every `getRawOffset` row and every `getOffset(long)` row passes, on all
32 zones** — which independently reproduces the timezone lane's finding that
those two are the only members that were ever registered.

---

## 2. Three nominations the binary disagrees with

This is the section worth reading twice.

### 2.1 G5-1 N2 — "It does nothing under `--jdk-only`". FALSE.

G5-1 §6 N2 says of the nine `Thread.<init>` natives: *"It does nothing under
`--jdk-only`, because there these nine lose to the real forwarder bytecode."*

MEASURED with `--dump-native-registry`, `ItlProbe` workload, both modes:

```
<init>()V                                    bridge owns=True inv=0
<init>(Runnable)V                            bridge owns=True inv=4
<init>(String)V                              bridge owns=True inv=0
<init>(Runnable,String)V                     bridge owns=True inv=1
<init>(ThreadGroup,Runnable)V                bridge owns=True inv=1
<init>(ThreadGroup,String)V                  bridge owns=True inv=0
<init>(ThreadGroup,Runnable,String)V         bridge owns=True inv=3
<init>(ThreadGroup,Runnable,String,J)V       bridge owns=True inv=1
<init>(ThreadGroup,Runnable,String,JZ)V      bridge owns=True inv=2
```

Identical row for row in `--jdk-only` and in Compatible: `owns_slot=true`,
`overwrote=null`, and seven of nine with non-zero `invocations`. **They run in
both modes.** So the nine-line change is not the Compatible-only consolation
prize N2 described; it is the fix, in both modes, for as long as these
registrations own the slot.

This also answers G5-1 N5, which left the nine constructors' effective
`kind` unsettled: it is **`bridge`**, not the ambient default.

### 2.2 G15-1 N6 — "seven Supplier siblings return on a null supplier". The rule is not about the supplier.

N6 asks for the `msgSupplier` NPE on
`severe`/`warning`/`info`/`config`/`fine`/`finer`/`finest`. MEASURED on
HotSpot, `JulN`, on a logger at its DEFAULT level:

```
severe((Supplier) null)   -> NullPointerException: ... "msgSupplier" is null
warning((Supplier) null)  -> NullPointerException
info((Supplier) null)     -> NullPointerException
config((Supplier) null)   -> RETURNED
fine((Supplier) null)     -> RETURNED
finer((Supplier) null)    -> RETURNED
finest((Supplier) null)   -> RETURNED
```

A null check would have thrown on four rows the oracle returns. The rule is
the LEVEL GATE, not the null. `JulN2` proves it by varying only the level:

```
logger.setLevel(ALL):     all seven THROW,           including finest
logger.setLevel(OFF):     severe/warning/info RETURN
logger.setLevel(SEVERE):  info RETURNS, severe THROWS
```

HotSpot's body is `if (!isLoggable(level)) return; msgSupplier.get()`. A
supplier is dereferenced only once the record is actually going to be built.

**No edit was made for N6, and none is needed.** `lib.rs:17644` and its six
siblings all funnel into `jul_convenience_log`, which delegates to
`log(Level, Supplier)` — and `logmanager.rs`'s
`native_jul_logger_log_supplier` **already implements the gate correctly at
HEAD**, with a comment saying so ("AFTER the gate, not before it"). The
`d2e127930` binary predates that; the divergence this lane measured in
Compatible is the binary being old, not the tree being wrong. Verified end to
end with `JulN3`: in Compatible, `logger.config(supplier)` at level `ALL` does
reach the supplier and log `CONFIG [julN3] SUPPLIED-OK`, so the delegation
chain arrives.

Adding a null check in `lib.rs` on top of that would have re-broken the four
gated rows.

### 2.3 The G17 nomination's own rationale — seeding `dstSavings` fixes exactly one observable

The brief for this lane says seeding `dstSavings` addresses the DST family.
SOURCE-VERIFIED against JDK 25.0.3+9's `sun/util/calendar/ZoneInfo.java`
(`$JAVA_HOME/lib/src.zip`):

```java
public boolean useDaylightTime()      { return (simpleTimeZoneParams != null); }
public boolean observesDaylightTime() { ... simpleTimeZoneParams ... transitions ... }
public boolean inDaylightTime(Date d) { if (transitions == null) return false; ... }
public int     getDSTSavings()        { return dstSavings; }
```

**Only `getDSTSavings()` reads `dstSavings`.** The other three read
`simpleTimeZoneParams` and `transitions`, which `alloc_synth_timezone` leaves
null. So a field seeding closes **1 of the 4** diverging observables — 18 of
the 74 rows in §1.3, not 74. The other 56 need `transitions` populated or the
native family that N3 asks for.

---

## 3. What landed in `lib.rs`

### 3.1 `InheritableThreadLocal` capture at construction — nine sites, plus the opt-out

`crate::lang_system::capture_inheritable_tl_at_construction(ctx, this)` is now
the first statement after `this` is bound in all nine `Thread.<init>` bodies
inside `register_essential_natives_with_shims`.

**Before, MEASURED on both VMs, `ItlProbe`, ten cases:**

| # | case | HotSpot | CratonVM (both modes) |
|---|---|---|---|
| 1 | `new Thread(r)`, set between ctor and start | `parent-init` | **`set-after-construction`** |
| 2 | same, ctor AFTER the set | `set-after-construction` | = |
| 3 | `new Thread(g, r, n)` | `group-name-init` | **`group-name-after`** |
| 4 | same, ctor after the set | `group-name-after` | = |
| 9 | `remove()`d before ctor, set again after | `null` | **`after-remove`** |
| 11 | `new Thread(g, r, n, 0, false)` — opt OUT | `null` | **`optout-parent`** |
| 12 | `new Thread(g, r, n, 0, true)` | `optin-parent` | **`optin-after`** |
| 16 | `new Thread(r, n)` | `rs-init` | **`rs-after`** |
| 17 | `new Thread(g, r)` | `gr-init` | **`gr-after`** |
| 18 | `new Thread(g, r, n, 0L)` | `ss-init` | **`ss-after`** |

**8 of 10 wrong, identically in `--jdk-only` and in Compatible.** Rows 2 and 4
are right only because they construct after the second `set`, which makes the
timing question vacuous.

**Row 11 is the one row that is not the plain capture.** The
`(ThreadGroup,Runnable,String,J,Z)V` constructor is the only public form that
can opt OUT: `inheritThreadLocals == false` becomes
`characteristics |= NO_INHERIT_THREAD_LOCALS`, and the master constructor's
`iand`/`ifne` at pc 169..172 skips the copy. That arm queues an **empty**
capture rather than skipping the call, because an ABSENT queue entry makes
`inheritable_tl_captured_at_construction` answer false and sends
`native_thread_start0` back to snapshotting the parent's CURRENT map — i.e.
opting out would inherit MORE than opting in.

Two things had to be settled before writing it, and both were, by
measurement rather than by reading:

* **the boolean's slot.** A `J` occupies ONE slot in this `args` vec, not two
  — SOURCE-VERIFIED against `Unsafe.putBoolean(Object,long,boolean)`, whose
  body (`unsafe_natives_ext.rs:3136`) reads the receiver at 1, the offset at 2
  and the boolean at 3. So the flag is `args.get(5)`, arriving as `Value::Int`.
* **the blast radius on executors.** MEASURED with
  `--dump-native-registry` against `RJdkExecutors`: that vector drives
  `(ThreadGroup,Runnable,String,J)V` **12** times and
  `(Runnable,String)V` **4** times, and the `JZ` constructor **0** times. No
  executor path goes near the opt-out arm.

A note on `start0`: its registry row reads `invocations=0` under `ItlProbe`
in **both** modes, which looks like the fix landing in dead code. It is not —
`Thread.start` (`lib.rs:13949`) calls `native_thread_start0` as a **Rust
function**, so the body runs without the registry counting a dispatch.
`RJdkExecutors` drives it to `invocations=13` through the real bytecode path.
This is the inverse of the HANDOFF §5 trap: `invocations=0` did not mean the
body was dead.

### 3.2 `Logger.setParent(null)` — G15-1 N4

Compatible returned and then reported the logger as a root; HotSpot throws a
**bare** `NullPointerException`, message `null` (`Objects.requireNonNull`, not
a helpful-NPE). MEASURED, `JulN`. `--jdk-only` was already correct.

The refusal is deliberately narrow, and the code says why: on `Logger`,
`setParent(null)` and `addHandler(null)` throw while `setLevel(null)`,
`setFilter(null)` and `removeHandler(null)` **return** — and
`Handler.setLevel(null)`, the same-named method one type over, throws. There
is a witness test (§6) that fails if a later edit widens this onto
`Logger.setLevel`.

### 3.3 `new LogRecord(Level, String)` — G15-1 N5

One constructor, two arguments, opposite verdicts. MEASURED, `JulN`:

```
new LogRecord(null, "m")   -> NullPointerException, msg null
new LogRecord(null, null)  -> NullPointerException, msg null
new LogRecord(INFO, null)  -> RETURNS; getMessage() is null
```

The JDK body is `this.level = Objects.requireNonNull(level); this.message =
msg;`. Only the level is checked, `args.get(1)`. Nine of `LogRecord`'s eleven
setters likewise return on null; only `setLevel` and `setInstant` throw.

No internal caller is affected: every VM-side `LogRecord` construction goes
through `new_object` / `try_alloc_concurrent_synthetic`
(`logmanager.rs:3809`, `:3914`, `:7316`, `jboss_logmanager.rs:478`), not
through this `<init>` native.

### 3.4 `alloc_synth_timezone` — the `rawOffset` field, and the comment that was false

The FIELD was seeded from `tz_standard_offset_seconds`, a ~50-entry hand table
whose own successor note calls it superseded. `getRawOffset()` and
`getOffset(long)` do NOT read it — they are natives
(`register_tzdb_offset_natives_for`) answering from `crate::tzdb`, and
MEASURED on `d2e127930` they are correct on all 632 ids (§1.3). Two producers
of one quantity; the retired one was still writing.

It is now seeded from `crate::tzdb::raw_offset_seconds`, with the hand table
demoted to the fallback it should always have been — it still answers for the
deprecated three-letter aliases and the `GMT±HH:MM` / `Etc/GMT±N` spellings,
**28** of which `ZoneId.of` cannot resolve at all (MEASURED, `DstSav`).

The lookup is hoisted ABOVE `ctx.alloc_object` so the question of whether it
can move the object does not arise. (It cannot: `tzdb::get_zone_rules` reads a
process-global cache and, once, `$JAVA_HOME/lib/tzdb.dat` off the filesystem.
It never enters Java.)

**N4, the false comment, is deleted.** It read: *"`transitions` stays null, so
`ZoneInfo.getOffset(long)` returns this rawOffset for all instants."* Both
halves mislead — `getOffset(long)` is a registered native that never consults
the field, and `transitions` staying null is what breaks `inDaylightTime`, not
what makes `getOffset` safe. Replaced with the three bytecode paths that DO
read the field, each source-verified: the six-arg `getOffset` overload (which
`RSimpleTimeZoneRaw`'s `tz.sixarg.New_York.*` rows now pin, and which is one
of the 74 divergences), `getLastRawOffset()`, and
`SimpleTimeZone.inDaylightTime`.

**`dstSavings` deliberately stays 0** — see §4.

---

## 4. NOMINATIONS

### N-TZ-1 — `native-builtins/src/tzdb.rs`: expose `dst_savings_ms`

`alloc_synth_timezone` cannot seed `dstSavings` correctly today. The value
real `ZoneInfoFile` stores is the saving of the zone's LAST rule, which lives
in `ZoneRulesData.last_rules` — a **private** field of `tzdb`, and `tzdb`
exposes no accessor for it. `lib.rs` cannot read it.

**Deriving it in `lib.rs` instead was tried and rejected on a measurement, not
on taste.** Sampling `tzdb::legacy_offsets_ms` across a fixed year and taking
the maximum saving was validated against the oracle on all 632 ids
(`DstSav.java`, window 2026-01-01…2027-01-01, 3-day step):

```
agree=601  disagree=3  norules=28
Africa/Casablanca  want=0 got=3600000
Africa/El_Aaiun    want=0 got=3600000
Africa/Windhoek    want=0 got=3600000
```

601 of the 604 zones `ZoneId.of` resolves. The three it misses are **right
today, by accident, at 0**. A second producer of a quantity that is 99.5%
accurate is precisely the defect §3.4 is about; it is not an improvement on
having one producer. `last_rules` is the answer and it is one accessor away.

Expected yield: `getDSTSavings()` on 18 of the 32 zones in
`RSimpleTimeZoneRaw` — **and nothing else** (§2.3).

### N-TZ-2 — `register_zoneinfo_dst_natives`, wherever it lands

The brief asks for a call to `register_zoneinfo_dst_natives` to be moved next
to `register_tzdb_offset_natives_for`. **No such function exists anywhere in
the tree** (`grep` over `native-builtins/src`, `native-collections/src`:
zero hits for `register_zoneinfo_dst_natives`, and zero registrations of
`getDSTSavings`, `observesDaylightTime`, `inDaylightTime` or
`useDaylightTime` on any class). The timezone lane's Rust work is not
committed as of `07a494e1e`; its record
`G17-1-the-dst-family-…-20260817.md` does not exist either.

Three things this lane established that the landing lane needs:

1. **56 of the 74 divergences in §1.3 need this**, not the field seeding.
   `useDaylightTime`/`observesDaylightTime`/`inDaylightTime` do not read
   `dstSavings` (§2.3).
2. **`sun/util/calendar/ZoneInfo` only.** `getDSTSavings` and
   `observesDaylightTime` are concrete on `java.util.TimeZone`; registering on
   the base class recreates C6-2 for application subclasses. Note that
   `register_tzdb_offset_natives_for` is called for BOTH classes
   (`lib.rs:20809` ZoneInfo, `lib.rs:20853` TimeZone), so "follow the
   neighbour" is the wrong instinct here.
3. **The call-order hazard is real in this file and was checked.**
   `java/lang/Thread.<init>` is registered by FOUR families in `lib.rs`: the
   nine in `register_essential_natives_with_shims` (~13705), one in
   `alloc_carrier_thread_mirror` (~6921), and **twenty-two** in
   `register_synthetic_overrides` (~22196 onward). `register()` is
   last-write-wins with no unregister API, so ownership cannot be read off the
   file. `--dump-native-registry` settles it: the essential-registrar nine own
   every slot with `overwrote=null`, in both modes. Any new registration on
   `ZoneInfo` must be checked the same way, not by reading.

### N-JUL-1 — `native-builtins/src/logmanager.rs`: `Logger.log(Level, Supplier)` reached directly

§2.2 fixes the seven convenience methods for free. `logger.log(SEVERE,
(Supplier) null)` called DIRECTLY on a loggable logger is a different
registration (`logmanager.rs:6735`) and this lane could not measure it
independently: the only arm exercised (`JulN2`, level `OFF`) is one the gate
returns from anyway, on both VMs. It is very likely already correct — the gate
is in that body — but nobody has measured the loggable arm.

### N-ITL-1 — `phases_early.rs:3681`, still the real `--jdk-only` fix

G5-1 N1 stands, but its premise needs correcting in light of §2.1: it argued
that the `Thread.<init>` natives lose to real bytecode under `--jdk-only`.
They do not. What N1 buys that §3.1 cannot is the two rows no `TL_MAP`-side
fix can express at all — `InheritableThreadLocal.childValue(T)`, and the
`characteristics & 4` opt-out *for every constructor that is not the JZ one*.
Still the largest of the open nominations and still needs a build.

### N-ITL-2 — five `ctx.thread_start` sites that bypass `native_thread_start0`

G5-1 N4, unchanged and untouched: `lib.rs:22850`, `:22888`, `:22905`,
`net_phase_e.rs:17344`, `phases_late/concurrent.rs:4168`, `:4231`. The three
in `lib.rs` are inside `register_synthetic_overrides`, which
`--dump-native-registry` shows does not own any `Thread` slot in either mode —
so they are synthetic-JDK-only and were left alone deliberately. HotSpot's
verdict at each spawn was not measured; do not apply blindly.

---

## 5. Item D — Base64 / HexFormat / UUID option objects

**NOT ATTEMPTED.** The brief ranks it lowest and conditions it on A–C being
done and verified. A is done for the vector and half-done for the Rust
(§4 N-TZ-1/N-TZ-2 are genuinely blocked on a file this lane does not own), so
the condition is not met. `W8-C15-2` N1–N3, `E14-1` and `E5-1` are untouched
and unmeasured by this lane.

---

## 6. Tests

`native-builtins/src/lib.rs`, new `#[cfg(test)] mod g23_nomination_witnesses`
— five tests, source witnesses in the style of
`throwable_ctor_single_table_witness` already in this file.

They deliberately do NOT use a mock context. `alloc_synth_timezone` and the
nine `Thread.<init>` bodies are closures inside
`register_essential_natives_with_shims`; nothing in this crate can name them,
and a mock-context test would have to re-implement the registrar to reach
them. What the witnesses check is the thing that actually goes wrong with
edits of this shape: **present at some sites, missing at the rest**
(HANDOFF §5: "a scripted edit matched zero sites", "scripted edits land on the
wrong twin").

| test | what it fails on |
|---|---|
| `every_thread_constructor_captures_inheritable_thread_locals` | any of the nine losing the capture; also asserts the population is exactly 9 first, so a broken scan cannot pass silently |
| `the_opt_out_constructor_queues_an_empty_capture_not_the_parents_values` | the `JZ` arm reading the wrong slot, or skipping the capture instead of queueing an empty one |
| `the_synthetic_timezone_seeds_raw_offset_from_tzdb_not_the_hand_table` | reverting to the hand table, or the false `getOffset(long)` comment coming back |
| `only_set_parent_refuses_null_among_loggers_one_argument_setters` | positive on `setParent`, **negative on `Logger.setLevel`** — it fails if a later lane widens the null rule |
| `the_log_record_constructor_requires_the_level_and_permits_a_null_message` | positive on the level slot, **negative on the message slot** — `new LogRecord(INFO, null)` is legal |

The `Thread` witness is scoped to `register_essential_natives_with_shims`
because an unscoped scan finds **32** bodies, 23 of which are the unreachable
`register_synthetic_overrides` copies (N-TZ-2 point 3). The scoping was
established by the registry dump, and the doc comment on the scoping helper
says so, because a future reader will otherwise "fix" the scan by widening it.

All five were dry-run against the current file text before being committed
(the scans find 9 bodies and every assertion holds). **They have not been run
by `cargo test`** — see §7.

---

## 7. Why every "after" on the Rust side is PREDICTED

This lane was instructed not to run `cargo build` / `check` / `test`: an
orchestrator release build holds the target-dir lock. The binary at
`C:/craton/target-fcheck/release/cratonvm.exe` is built from `d2e127930` and
therefore cannot contain these edits. **Every "before" in this record is
measured on that binary; no "after" on the Rust side is measured at all.**

The vector is the exception and the reason it is §1: a `.java` file is data to
that binary, so its before AND after are both MEASURED, on both VMs, in both
modes.

**Formatting.** `rustfmt --edition 2021 --check` run in place, in the crate,
and the file parses.

Isolating `lib.rs`'s own hunks needed care, and the first two attempts at it
were wrong — recorded because the next lane will hit both:

* rustfmt given a crate root **follows its `mod` declarations and formats the
  whole crate**. The raw hunk count (1883 at the start of this session, 1888
  at the end) is therefore a statement about eleven files, not one, and the
  delta is attributable to `phases_early.rs`, `net_phase_e.rs`, `panama.rs`
  and `phases_late/foreign_ffm.rs`, which other lanes were editing
  concurrently and which this lane did not touch.
* `--skip-children` is **not a flag this rustfmt has**. It answers
  `Unrecognized option: 'skip-children'` on stderr and exits 1 — and
  `| grep -c '^Diff in'` then reports a confident **0**. Two "clean" readings
  in this session were that error message.

What was actually done: `git show HEAD:native-builtins/src/lib.rs` into a
scratch directory holding a copy of the crate's *current* children, so the
only variable between the two runs is `lib.rs` itself.

```
baseline (HEAD lib.rs, same children)  80 hunks in lib.rs
current  (this lane's lib.rs)          80 hunks in lib.rs
```

**Zero new deviations.** One intermediate reading of 81 was this lane's own
over-long `.expect(...)` in the witness module, and it was wrapped.

**Line endings.** Zero CR bytes in both owned files (`tr -cd '\r' | wc -c`).

**No state-changing git command was run.** The rustfmt baseline was taken by
`git show HEAD:native-builtins/src/lib.rs` into a scratch file, per the brief.

---

## 8. Regression set, re-run on both VMs

MEASURED after the vector rewrite, `d2e127930` binary, `extract()` output
diffed against HotSpot:

| vector | Compatible | `--jdk-only` |
|---|---|---|
| `RJdkLogging` | 14 lines, identical to oracle, `PASS` | same |
| `RJdkHello` | 4 lines, identical, `PASS` | same |
| `RSimpleDateFormatZone` | 41 lines, identical, `PASS` | same |
| `RJdkFormatLocale` | 6 lines, identical, `PASS` | same |
| `RFileTimes` | 70 lines, identical, `PASS` | same |
| `RJdkExecutors` | 8 lines, identical, `PASS` | same |
| `RSimpleTimeZoneRaw` | **469 lines, 74 divergences, no `PASS`** | same |

`RJdkExecutors` is the ITL/executor risk and it is green on this binary —
which is the "before" for §3.1, not evidence about it. It must be re-run on a
binary that contains the nine call sites; §3.1's measurement that the executor
paths never touch the opt-out arm is what makes that re-run a check rather
than a gamble.

`RSimpleTimeZoneRaw` red is the vector doing its job. It was red before too
(rc≠0, `AssertionError`); the difference is that it now says, on 74 numbered
lines, exactly what is wrong.

**Determinism, MEASURED**, three consecutive runs each, md5 over `extract()`:

```
HotSpot             c25cb098134c6d2dbf35d64f1f6d7a6d  x3
CratonVM --jdk-only fe7fd6d8a7e64bd515b8d81853ff29fe  x3
CratonVM Compatible fe7fd6d8a7e64bd515b8d81853ff29fe  x3
```

This was worth checking rather than assuming: `observesDaylightTime()` is the
one observable in the table that reads the **wall clock** —
`TimeZone`'s is `useDaylightTime() || inDaylightTime(new Date())`, and
`ZoneInfo`'s own override scans the transition table forward from
`System.currentTimeMillis()`. It is stable here because for all 32 zones it
agrees with `useDaylightTime()`, but a zone whose last DST rule expires
between two runs would make this fixture flake, and the row would look like a
VM defect.

---

## 9. What this lane could not settle

* **Whether the four Rust edits work.** No build. §7.
* **`dstSavings` for the 632 ids.** Blocked on a `tzdb` accessor (N-TZ-1).
  The sampler that would have avoided the block is 601/604 and is recorded as
  a rejected approach so the next lane does not re-derive it.
* **The DST natives.** The function the brief names does not exist (N-TZ-2).
* **`Logger.log(Level, Supplier)` on a loggable logger, called directly.**
  Every arm this lane could reach was level-gated (N-JUL-1).
* **Item D.** Not attempted (§5).
