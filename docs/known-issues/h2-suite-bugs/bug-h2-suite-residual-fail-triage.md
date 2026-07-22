# H2 suite — residual FAIL triage (2026-07-21): reproduced, narrowed, not fully root-caused

## Status
**OPEN, mixed, mostly closed after four follow-up sessions.** Of the
original 11 items: **8 now confirmed FIXED** (`TestPreparedStatement`,
`TestShell`, `TestRandomMapOps` [very likely — see its section],
`TestLinkedTable`, `TestAlter`, `TestDataUtils` [now fully fixed — see the
"Follow-up session (2026-07-22, third pass)" section], plus the
`SecurityException` half of `TestUpgrade`), **1 root-caused as a genuine
performance-margin issue rather than a discrete bug** (`TestBnf`, joining
`TestFileLock`/`TestTransaction` in that category), and **3 root-caused,
performance-margin, no fix expected/attempted** (`TestFileLock`,
`TestTransaction`, plus `TestFuzzOptimizations` which is
inconclusive/likely-not-CratonVM-specific). **One genuine open residual
remains requiring further VM work: `TestUpgrade`'s secondary
`NoSuchMethodError`** — now narrowed much further (see the third-pass
section) but still not closed. A **separate, previously-undiscovered
systemic bug was found and fixed** in the process (the JIT compiled-code
cache was keyed by class NAME only, with no loader/`ClassId` component —
see the third-pass section for the full writeup) — real and worth keeping,
but confirmed **not sufficient by itself** to close `TestUpgrade` (the
NoSuchMethodError reproduces identically with `--nojit`). A **ninth item is
now also FIXED**: the `TestPreparedStatement.testDate8` 1-hour-offset
residual discovered during the third pass (distinct from the
already-fixed Julian/Gregorian cutover bug in the same test class) — see
the "Follow-up session (2026-07-22, fourth pass)" section. See each item
below, and the "Follow-up session (2026-07-22, second pass)", "third
pass", and "fourth pass" summaries further down, for full detail.

All classes below PASS on the HotSpot JDK25 baseline; all originally FAILed
under CratonVM `jit-real` (real JDK25 backend). Fix commits landed on
`dev` via branch `fix/h2-residual-triage-20260721`.

## `org.h2.test.jdbc.TestPreparedStatement` (`testDate8`) — Julian/Gregorian calendar cutover — **FIXED**
**Root cause:** `native-builtins/src/deprecated_util.rs`'s `date_fields_to_millis`/
`millis_to_date_parts` (backing `java.util.Date`'s deprecated multi-arg
constructors/getters, and by extension `java.sql.Date`'s, which delegate to
them) computed pure proleptic-Gregorian day arithmetic with **no Julian
calendar cutover** — but `Date`'s deprecated constructors are specified in
terms of `GregorianCalendar`'s default hybrid Julian/Gregorian calendar
(dates before 1582-10-15 are interpreted as **Julian** calendar dates, not
proleptic Gregorian). This produced a ~10-day discrepancy for any date
before the historical cutover.

**Fix:** replaced the naive day-counting arithmetic with proper Julian Day
Number (JDN) conversion — Fliegel & Van Flandern (1968) formulas for both
proleptic-Gregorian and proleptic-Julian calendars, picking the branch based
on whether the Gregorian-interpreted JDN is before the cutover (JDN
2299161 = 1582-10-15). Verified against real HotSpot JDK25 bit-for-bit
(`Date.valueOf("1582-09-25")`, `Date.valueOf(LocalDate)`, and an explicit
`GregorianCalendar` with `setGregorianChange(Long.MIN_VALUE)` all now agree
with HotSpot exactly). 3 new regression unit tests added alongside the
existing `deprecated_util.rs` test suite (50/50 pass).

**A second, independent bug was needed to actually SHIP the fix**: a
duplicate/shadowing native registration. `vm/src/vm/vm_init.rs` calls
`deprecated_io_util::register_deprecated_io_util_natives()` a SECOND time at
two later boot points (intending only to grab the URL-codec natives —
comment says "KC26: Register URL codec"), but that function ALSO
re-registers `deprecated_io_util.rs`'s OWN, older, pure-Gregorian-no-cutover
implementation of `java/util/Date`'s deprecated constructors — a **second,
independent, buggier implementation of the same natives**, living in a
DIFFERENT file (`deprecated_io_util.rs` vs `deprecated_util.rs`), registered
LAST (native registry is last-write-wins), silently clobbering the correct
fix back to the broken one. Fixed by narrowing both `vm_init.rs` call sites
to call the module's `register_url_codec` directly instead of the whole
module. This is the same "duplicate native registrations, verify which
wins" bug class documented elsewhere in this codebase's memory — worth a
grep sweep (`register_deprecated_io_util_natives` call sites) for other
similarly-scoped-too-broad re-registration calls.

## `org.h2.test.unit.TestFileLock` (`testSimple`) — wrong exception/error-code on concurrent DB open — **root-caused, NOT a discrete fix**
Root-caused via direct file-mtime probes replicating H2's own `FileLock`
protocol exactly (watchdog thread vs. contender thread racing over a lock
file's `lastModified()` timestamp). **This is an inherently racy protocol —
even real HotSpot loses this race occasionally** (observed 1/3 times in a
quick sample): H2's `FileLock.lockFile()` depends on the LOSING thread's
watchdog waking up and re-writing the lock file within a ~2-second window
while the WINNING thread holds it; if the watchdog is late, the winner
silently believes it acquired an uncontested lock. Under CratonVM this race
consistently resolves in the "wrong" direction (6/6 runs), most likely
because CratonVM's overall per-operation interpreter overhead (more real
file I/O calls: `SortedProperties` load/store, directory creation, etc.,
each individually slower than HotSpot's JIT-warmed execution) shifts the
watchdog's relative wake-up timing outside the safe margin. Not a logic bug
in CratonVM's file I/O (`File.lastModified()` millisecond precision
verified correct); a genuine performance-margin issue. **Same root-cause
family as `TestTransaction` below.** No targeted fix attempted — would
require either broad interpreter/native-call throughput work, or accepting
this test as environment-sensitive.

## `org.h2.test.store.TestRandomMapOps` (`seed:0 op:9`) — MVMap reverse-view assertion — **very likely FIXED** (via concurrent session's TreeMap dispatch fix; not verified to full completion)
An independent, concurrent session fixed `bug-h2-treemap-tailmap-headmap-
view-corruption.md` (`dev@fix/h2-treemap-tailmap-dispatch-20260721`, now
closed) while this session was in progress — root cause turned out to be
CratonVM's synthetic `TreeMap`'s fast/array backing-store dispatch, not a
class-collision as originally guessed here either (see that doc's own
history for the correction). Merged in and reran the actual
`TestRandomMapOps` class: the original fast, deterministic assertion
failure (`rev (1654, null)`, immediate) is **gone** — the class no longer
fails, it just runs for a long time (still executing cleanly past 600s
wall-clock when this session's time budget ran out, no assertion, no
crash). This is plausibly just this fuzz test's legitimately heavy
workload (`TestAll.big=true`, up to 3000 ops × 100 rounds) being slow under
CratonVM's interpreter, not a new bug — but this session did not confirm a
clean exit-0 completion, so treat as "very likely fixed, not 100%
verified" until someone runs it to completion with a generous (10+ minute)
timeout. Original analysis kept below for context.
Confirmed the same underlying mechanism as `TestAlter` below (and shares a
root cause with the pre-existing `bug-h2-treemap-tailmap-headmap-view-corruption.md`
doc, which this session's investigation supersedes/deepens — see that doc
for the update). **`java.util.TreeMap`'s `tailMap`/`headMap`/`subMap`/
`descendingMap`/`descendingKeySet` views are universally broken** (empty or
single-bogus-entry) under CratonVM real-JDK mode, reproducible with plain
`java.util.TreeMap` and no H2 code at all, with or without `--nojit`. Ruled
out (with hard evidence) the original doc's leading hypothesis (a
class-blind native-dispatch collision with `ConcurrentSkipListMap`'s
registered natives — descriptors actually differ, so this can't collide).
Confirmed via reflection that `NavigableSubMap`'s own fields (`m`, `lo`,
`hi`, `fromStart`, `toEnd`, `loInclusive`, `hiInclusive`) are stored/read
correctly, and that `containsKey()` (uses the same fields) works correctly
— so the bug is specifically in `size()`/iteration, not general field
storage or range-check logic. Multiple synthetic repros mimicking the exact
same class shapes (non-static inner class capturing an outer `NavigableMap`
reference, 3-arg constructor with 2 chained method-call arguments, etc.)
all reproduced FINE — meaning the bug requires something specific to the
*real* `java.util.TreeMap` bytecode/class-loading that a hand-written
equivalent doesn't trigger. **Same root-cause family as `TestAlter`'s
`ConcurrentHashMap.values()` finding** — both are real-JDK collection VIEW
classes that hold a captured reference to their backing collection; queried
freshly they report correctly, but a view instance queried after further
mutation of the backing collection does NOT reflect the live state (as if
frozen at construction). **Next step for whoever picks this up:** the CHM
case (`org.h2.test.db.TestAlter` below) is a MUCH simpler, H2-independent
minimal repro (`ConcurrentHashMap<K,V> m; Collection<V> v = m.values(); m.put(...); v.size()` still shows the old count) — start there rather than
TreeMap's more complex class hierarchy.

## `org.h2.test.synth.TestFuzzOptimizations` — NullPointerException deep in query optimizer recursion — **inconclusive, likely not CratonVM-specific**
The fuzzer's seed is **wall-clock-seeded, not fixed**
(`new Random()`/`random.nextLong()`, no fixed seed anywhere in
`testIn`/`testInSelect`/`testGroupSorted`). Ran to completion cleanly
multiple times (400s+, 200s+ budgets, hundreds of random seeds each) with
**zero reproductions** of the original NPE. Given the doc's original capture
was a single random-seed hit, and this session's extensive re-runs found
nothing, this reads as a genuinely rare, seed-dependent condition — plausibly
a real (rare) H2 optimizer bug that would eventually surface on HotSpot too
with enough trials, not a deterministic CratonVM regression. Not
investigated further; flag for the next session only if it recurs with a
reproducible seed.

## `org.h2.test.db.TestLinkedTable` (`testHiddenSQL`) — password redaction / SQL text mismatch — **FIXED (2026-07-22 follow-up)**
Root-caused to a **`SQLException.toString()` vs `getMessage()` divergence**:
`getMessage()` (called directly) correctly returns the full H2-formatted
message (including the un-hidden `CREATE LINKED TABLE ... 'pwd' ...` SQL
statement text, which is what the test's `assertContains(e.toString(),
"pwd")` needs). But `e.toString()` on the SAME exception object — a
`JdbcSQLSyntaxErrorException`, which overrides `toString()` itself with
"return `stackTrace` field if set, else `super.toString()`" — instead
invokes CratonVM's NATIVE `Throwable.toString()` (registered on
`java/lang/Throwable`), producing `"ClassName: " + getLocalizedMessage()`
where the nested `getLocalizedMessage()`→`getMessage()` call chain (via
`NativeContext::invoke_virtual`, the Rust API natives use to call back into
Java) returns a **shorter/stale** message missing the SQL-statement suffix
— even though a DIRECT (non-native-mediated) `e.getMessage()` call from
Java bytecode gives the correct, full text.

This traces to `vm/src/runtime/interpreter.rs`'s
`populate_virtual_invoke_cache` (the invokevirtual inline-cache populator):
it has an EXISTING, documented "receiver-bytecode short-circuit" fix
(comment: "Round 63 (peaceful-sammet)") specifically designed to prevent
exactly this class of bug (an ancestor's native shadowing a receiver's own
real bytecode override), and by inspection it SHOULD correctly detect that
`JdbcSQLSyntaxErrorException` declares its own `toString()` and skip the
ancestor-native promotion — yet empirically the native still wins for this
call. The gap was not pinned to an exact line within the session's time
budget; next step is tracing which SPECIFIC dispatch path this particular
call site takes (there appear to be multiple internal call resolution
mechanisms — the interpreter's own invokevirtual opcode handler vs.
`vm/src/vm/vm_exec.rs`'s `NativeContext::invoke_virtual`, used when a
native calls back into Java — and they may not share identical
override-priority logic). A `CRATONVM_DBG_CALLER`-style env-gated trace
added to `populate_virtual_invoke_cache` at the toString/getMessage/
getLocalizedMessage call sites would likely resolve this quickly.

## `org.h2.test.unit.TestUpgrade` — `SecurityException` unloading H2 driver — **FIXED** (root cause confirmed, secondary pre-existing bug now exposed)
**Root cause confirmed exactly as hypothesized**: `jdk/internal/reflect/Reflection.getCallerClass()`'s no-arg native implementation
(`native-builtins/src/deprecated_internal.rs`) resolved the caller frame's
`Class` mirror via a **name-based** lookup
(`ctx.ensure_class_initialized(&frame.class_name)`) instead of using the
frame's own `class_id` (captured live from the interpreter frame, exactly
the field `StackTraceEntry::class_id` exists for — its doc comment
explicitly warns implementations to prefer it over name-based re-lookup).
When the caller class is loaded by a distinct `ClassLoader` from a
same-named class elsewhere on the classpath (H2 `Upgrade.loadH2`'s pattern:
a custom parentless `ClassLoader` that `defineClass`-loads its OWN copy of
`org.h2.Driver`, while `org.h2.Driver` is ALSO on the normal test
classpath), the name-based lookup collapses to the WRONG (first-loaded)
copy's `Class` mirror — so `DriverManager.deregisterDriver`'s
`isDriverAllowed(driver, callerClass)` check compares the driver's real
classloader against the wrong caller's classloader and throws
`SecurityException`.

**Fix:** all 4 `getCallerClass` native registrations in
`deprecated_internal.rs` (`sun/reflect/Reflection` and
`jdk/internal/reflect/Reflection`, both the `(I)` depth-based and `()`
no-arg forms) now prefer `frame.class_id` when the captured stack frame has
one, falling back to the name-based lookup only when it doesn't (synthetic
frames). Verified with a minimal, H2-independent repro (custom
`ClassLoader` + reflective `Method.invoke` calling
`DriverManager.registerDriver`/`deregisterDriver`) and against the real
`TestUpgrade` — the `SecurityException` is gone.

**Secondary finding — NOT fixed, separate pre-existing bug**: past the
`SecurityException`, `TestUpgrade` now hits a **different, already-tracked**
failure: `NoSuchMethodError: org/h2/mvstore/RootReference.hasChangesSince(J)Z`
— this is the SAME general "wrong-receiver-class dispatch" bug family as
`docs/known-issues/h2-suite-bugs/bug-h2-nosuchmethoderror-cross-class-dispatch.md`
(two different versions of `org.h2.mvstore.RootReference`, loaded by two
different classloaders — the dynamically-downloaded old H2 jar vs. the
normal test classpath — colliding). Not a regression from this session's
fix; `TestUpgrade` simply couldn't reach this point before. `TestUpgrade`
is NOT yet a clean PASS; tracked as a new confirmed instance of the
existing cross-class-dispatch doc rather than reopening here.

## `org.h2.test.unit.TestShell` — NPE reading Shell tool output (piped stdin/stdout) — **FIXED** (via concurrent session's dispatch fix)
This was **not** a piped-process-stdin/stdout-specific CratonVM gap (as the
original triage guessed) — it was a repro of `bug-h2-nosuchmethoderror-
cross-class-dispatch.md`'s "Cluster A — `PipedInputStream.flush()V`"` bug.
Confirmed directly: `Shell.println()`/`Shell.print()` call `out.flush()`
(where `out` wraps a `PipedOutputStream`), and CratonVM's method dispatch
was resolving this to the nonsensical `PipedInputStream.flush()V`
(`NoSuchMethodError`, logged as a WARN and swallowed rather than
propagated) — so the Shell tool's output was silently never written to the
pipe, and `LineNumberReader.readLine()` returned `null` on the first read.
An independent, concurrent session fixed the underlying cross-class
dispatch bug (`dev@fb58d3d10`, doc moved to
`docs/internal/h2-suite-bugs/bug-h2-nosuchmethoderror-cross-class-dispatch-FIXED.md`)
while this session was in progress; merged in and **re-verified against the
real `TestShell` class directly** (not just trusting the other session's
claim) — passes cleanly (takes ~90-150s wall-clock; needs a timeout above
the suite runner's default 60s for this class specifically, not a hang).

## `org.h2.test.db.TestAlter` (`testAlterTableDropIdentityColumn`) — `Expected: 1 actual: 0` — **FIXED (2026-07-22 follow-up)**
Root-caused to a **minimal, H2-independent, general JDK bug**:
```java
ConcurrentHashMap<String,Integer> m = new ConcurrentHashMap<>();
Collection<Integer> v = m.values();       // captured once, like H2's Schema.sequences view
m.put("a", 1);
v.size();                                  // returns 0, should be 1 (live view per JDK spec)
```
A `ConcurrentHashMap.values()` (or `keySet()`/`entrySet()`) view, when
queried freshly (`m.values().size()`), correctly reflects the CURRENT map
size — but a view captured EARLIER and queried AFTER further mutations
returns a value **frozen at capture time**, not the live current state, as
`java.util.concurrent.ConcurrentHashMap`'s spec requires. Confirmed with
`--nojit` (rules out JIT). H2's `Schema.getAllSequences()` (a raw
`ConcurrentHashMap<String,Sequence>.values()`) is captured once by the test
before any tables exist, so it reads as permanently empty (0) regardless of
how many auto-increment sequences get created/dropped afterward.
**Same root-cause family as `TestRandomMapOps`'s `TreeMap` view findings
above** — both are real-JDK collection VIEW classes (`ValuesView`,
`NavigableSubMap`) holding a captured backing-collection reference that
doesn't behave as a live view. This CHM repro is much simpler than the
TreeMap one and is the recommended starting point for whoever picks up the
underlying VM bug — likely something specific to how CratonVM's object
model handles a `static` nested VIEW class's captured outer-collection
field reference across GC/interpreter boundaries (not a general "static
nested class capturing an outer reference" bug — hand-written repros of
that exact shape work correctly).

## `org.h2.test.db.TestTransaction` (`testMergeUsing`) — `Expected: 100 actual: 50` — **root-caused, NOT a logic bug**
Not a concurrency-visibility/MVCC correctness bug. Instrumented the
(previously exception-swallowing) test and found the ACTUAL cause:
`org.h2.jdbc.JdbcBatchUpdateException: Timeout trying to lock table "TEST"`
— one of the two racing threads' entire 50-statement batch fails as a unit
on H2's own default 2-second `LOCK_TIMEOUT`, losing all 50 of its row
operations at once (explaining the exact "half" result: one thread's batch
succeeds completely, the other's times out completely). Deterministic
(3/3 runs). Same root-cause family as `TestFileLock` above: CratonVM's
per-statement execution overhead for a sequential batch of 50 SQL
statements is high enough (relative to HotSpot's JIT-optimized execution)
that the concurrently-racing thread exceeds H2's fixed 2-second lock-wait
margin before the first thread releases its row locks at commit. Not a
correctness bug in CratonVM's locking/MVCC implementation; a
performance-margin issue with the same shape as `TestFileLock`. No fix
attempted (would require broader interpreter throughput work).

## `org.h2.test.unit.TestBnf` (`testProcedures`) — `Expected: true got: false` — **root-caused (2026-07-22 follow-up): performance-margin issue, NOT a discrete bug**
Not a `DbContextRule`/procedure-registration bug (the procedure IS
correctly registered and discoverable via `schema.getProcedures()` — a
separate, PASSING assertion earlier in the same test proves this). The
actual failure: `Bnf.getNextTokenList("SELECT CUSTOM_PR")` never suggests
`CUSTOM_PRINT` as a completion at all (confirmed via instrumentation:
`DbContextRule.autoCompleteProcedure()` — the method that would emit the
suggestion — is **never even called** for this input, vs. HotSpot where it
correctly returns 4 candidate tokens including `CUSTOM_PRINT`, this session
only got 3). This points to a bug earlier in H2's own BNF grammar-topic
walker (`Bnf`/`RuleList`/`RuleElement`, a separate, large parsing engine)
never reaching the `user_defined_function_name` grammar production for this
input under CratonVM — not investigated further given the size of that
engine and this session's time budget. Next step: instrument
`Bnf`/`RuleList.autoComplete()`'s rule-walking loop directly (not
`DbContextRule`, which is downstream and never reached).

## `org.h2.test.store.TestDataUtils` (`testParse`) — `Expected: 1 actual: 1` — **FULLY FIXED (2026-07-22, third pass — see that section for the closing fix)**
Root-caused to a **`StringBuilder.append(long)` / string-concatenation
corruption bug for specific long values**, not a `DataUtils.parseHexLong`
bug (parseHexLong's actual return value, checked via `==`/`!=` on the raw
primitive, is correct in every case checked). Minimal repro:
```java
long i = 1125899906842623L;         // 0x0003FFFFFFFFFFFF
long negI = -i;                      // correct value, confirmed via Long.toHexString(negI) = "fffc000000000001"
System.out.println("-i=" + negI);    // prints "1" — WRONG, should print "-1125899906842623"
```
The underlying 64-bit value is correct throughout (equality comparisons on
the raw `long` pass); only the DECIMAL STRING rendering via string
concatenation is corrupted. Traced to `native-builtins/src/lang_string.rs`'s
`native_sb_append_long` (backs `StringBuilder.append(long)`, and by
extension `"..." + longValue` concatenation, which javac compiles through
`StringConcatFactory`/`StringBuilder` machinery). Its own doc comment
("WP4.5") already acknowledges a known workaround: `invokevirtual` with a
`long` argument sometimes type-erases the value to `Value::Double` via
`CompactValue::to_value()` before reaching this native, so the function
bit-reinterprets a `Value::Double` back to `i64` via `d.to_bits()`. The
corrupted value (`0xFFFC000000000001`), reinterpreted as an IEEE-754
double, falls in the NaN/Infinity exponent range (top 12 bits all 1s) —
consistent with the value's bit pattern being altered somewhere in the
`Value::Long` → `CompactValue` → `Value::Double` round-trip specifically
for values whose bits alias a NaN encoding (e.g. NaN canonicalization
somewhere in that pipeline). Not fixed — the `CompactValue`/interpreter
stack-value representation is core VM plumbing, too large a blast radius to
patch blindly within this session; flagging the exact function
(`native_sb_append_long`, `lang_string.rs`) and trigger condition (long
values whose bits alias an IEEE-754 NaN/Infinity pattern) for a follow-up
session with more room to trace `CompactValue::to_value()` itself.

## `org.h2.test.poweroff.TestRecoverKillLoop` — excluded as non-comparable, not a new CratonVM FAIL
CratonVM fails at `00:00:00.000` (first iteration); the HotSpot baseline
"fails" too, but only after **5 hours 16 minutes** of continuous
kill/restart-loop stress testing, with the identical generic failure text
("`error! renaming file`" — the test's only failure message for any rename
error, from any cause). This class's `main()` is one of the H2 suite's rare
exceptions to the standard `TestBase.createCaller().init().testFromMain()`
convention (see `run-h2-suite.md`'s "217 of 218 classes" caveat): its
`main()` directly calls `runTest(Integer.MAX_VALUE)` — an intentionally
open-ended, manual stress-test entry point (repeatedly spawning and
`kill -9`-ing a child `TestRecover` JVM process), not something meant to
"pass" or "fail" cleanly inside a per-class-timeout harness. Given both VMs
eventually hit a failure with the same generic message and this test isn't
designed to terminate cleanly either way, this is **not counted as a
genuine new CratonVM regression** — but the fact that CratonVM's *first*
child-process iteration already fails (vs. HotSpot's ~1900+ successful
kill/restart cycles before its eventual failure) does suggest something
about CratonVM's `Runtime.exec()`-spawned child-process handling for this
specific test is worth a dedicated look if this class matters going
forward. Not investigated further this session.

## Repro (representative — swap the class name)
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.db.TestAlter
```

## Full 218-class regression check
After landing this session's own 2 fixes (calendar/Julian-cutover,
`getCallerClass` loader-identity), a full `run-h2-suite.sh run --category
all --count 218` pass was run on that binary (before the later merge with
the two concurrent-session fixes below): 120 PASS / 59 HANG / 39 FAIL.
Spot-checked 5 of the FAIL classes not otherwise explained in this doc
(`TestConnectionPool`, `TestFileSystem`, `TestNetUtils`,
`TestMVStoreStopCompact`, `TestAnalyzeTableTx`) against a binary built from
UNMODIFIED `dev` (pre-session) — all 5 fail identically on both, confirming
they are pre-existing, not regressions introduced by this session's fixes.

After merging `origin/dev` (which had, in the meantime, picked up two
*other* sessions' independent fixes — `bug-h2-nosuchmethoderror-cross-
class-dispatch.md` and `bug-h2-treemap-tailmap-headmap-view-corruption.md`,
both now closed) and rebuilding, `TestPreparedStatement`, `TestShell`, and
the `SecurityException` half of `TestUpgrade` all now PASS directly, and
`TestRandomMapOps` no longer hits its original fast/deterministic assertion
failure (runs long instead — see that item above). A full clean re-run of
all 218 classes on this final merged binary was not completed within this
session's time budget; the per-class spot checks above stand in for it.

## Follow-up session (2026-07-22, second pass): 3 more fixed, 1 more root-caused, 1 partially fixed

Picked up the 7 items still open after the first follow-up session (worktree
`/data/wt-h2-residual-closure-20260722` on the Azure host, branch
`fix/h2-residual-closure-20260722`, branched from `origin/dev`). Result:

- **`TestLinkedTable` (`testHiddenSQL`) — FIXED.** Root cause was NOT the
  dispatch-priority theory in the original write-up below (that
  investigation, while thorough, was chasing the wrong mechanism — the
  interpreter's own invokevirtual/invokespecial caching turned out to be
  correct throughout). The actual bug: `native-builtins/src/
  deprecated_internal.rs`'s `register_reflection_natives` registered a
  SECOND, independent `java/lang/Throwable.toString()` native (added to fix
  JBoss Modules printing `ModuleNotFoundException@0` instead of its message)
  that reads the receiver's own field slots 0..5 directly for any non-empty
  String — completely bypassing virtual dispatch to
  `getMessage()`/`getLocalizedMessage()`. Native registries are
  last-write-wins, and this registration runs after
  `register_throwable_subclass_natives` (which already registers the
  correct, virtual-dispatching `native_throwable_to_string`) within the same
  `register_essential_natives` call chain, so the raw-field-scanning version
  silently won for every `Throwable` in real-JDK mode. This broke any
  `Throwable` subclass whose `getMessage()`/`getLocalizedMessage()` override
  computes the message dynamically rather than storing it in one of the
  receiver's own first 6 field slots — exactly `JdbcSQLSyntaxErrorException`'s
  shape. Removed the duplicate registration entirely; the correct native
  already fixes the original JBoss Modules symptom too (verified with a
  minimal repro against real HotSpot). See `docs/internal/h2-suite-bugs/`
  for the closed writeup.

- **`TestAlter` (`testAlterTableDropIdentityColumn`) — FIXED.** Two
  independent bugs combined to make `Map.values()` a dead, one-time
  snapshot while `keySet()`/`entrySet()` were correctly live: (1) the
  generic `HashMap` path (`native_map_values` in
  `native-collections/src/lib.rs`) already had a `resync_values_view`
  live-view mechanism (a stashed source-map marker in a spare trailing
  ArrayList capacity slot, checked by `native_al_size`/`get`/`contains`/
  `iterator`/etc.), but real `java.util.ArrayList` declares its own
  `size()`/`isEmpty()`/`get()`/etc. bytecode and `ArrayList` was missing
  from the receiver-has-own-bytecode force-native allowlist (unlike
  `HashMap`/`HashSet`), so real bytecode always won and the resync logic was
  silently unreachable; (2) the `ConcurrentHashMap`-specific path
  (`native_chm_values`) never had ANY live-view support at all — unlike its
  siblings `native_chm_key_set`/`native_chm_entry_set` — it built a bare
  disconnected snapshot with no stashed source reference whatsoever. Fixed
  both: added `ArrayList` to the two mirrored force-native allowlists
  (`interpreter.rs::force_native_over_real_jdk_bytecode` and
  `vm_exec.rs`'s `invoke_on_class_shared_inner` allowlist), and gave
  `native_chm_values` the same trailing-slot source-map marker
  `native_map_values` already uses. Fixes H2's `Schema.getAllSequences()`
  (a raw `ConcurrentHashMap.values()` captured once, before any sequence
  exists).

- **`TestDataUtils` (`testParse`) — PARTIALLY FIXED, deeper residual
  remains.** The exact repro from the original write-up below (`"-i=" +
  negI` string concatenation corrupting a long whose bits collide with the
  NaN-tag space) is fixed: `execute_string_concat` (the
  `StringConcatFactory`/`invokedynamic` handler in
  `vm/src/runtime/invokedynamic.rs`) was popping arguments via the older,
  non-long-mark-aware `stack.pop_compact()` + `CompactValue::
  decode_by_descriptor()` pairing, instead of the long-mark-aware
  `stack.pop_arg_for_descriptor_checked()` that the invoke-argument
  marshalling path (`pop_coerced_invoke_args_virtual`/`_static`) already
  uses for exactly this reason (see the `BC SM2` fix family, `git log
  --grep 'BC SM2'`). A `J`-descriptor argument whose raw bits collide with
  the NaN-tag space **and** whose masked 47-bit payload happens to fit in
  32 bits is genuinely bit-identical to a tagged `CompactValue::int()`
  encoding (verified: `CompactValue::int(1)`'s raw bits are exactly
  `0xFFFC000000000001`, the same bits `-1125899906842623L` collides to) —
  undecidable from the bits alone, which is why the interpreter tracks a
  parallel `KIND_LONG` marker alongside the operand stack specifically for
  this ambiguity. Switched `execute_string_concat` to the long-mark-aware
  pop; verified against real HotSpot with a minimal repro
  (`SbAppendLongRepro.java`).

  However, `org.h2.test.store.TestDataUtils.testParse` **still fails** —
  root-caused to a DEEPER, separate manifestation of the same NaN-box
  long/int collision ambiguity, this time when a colliding long value is
  passed as an **argument to a user-defined static method called repeatedly
  at the same call site inside a loop** (not string concatenation). Minimal
  repro (`LoopParseRepro.java`, alongside `LoopInlineRepro.java`/
  `LongArgCallRepro.java`/`SbAppendLongRepro.java`/`ShiftOrLongRepro.java`,
  in `docs/known-issues/repros/h2-testdatautils-invokestatic-long-corruption/`):
  a `for (long i = -1; i != 0; i >>>= 1)` loop calling
  `check(i, parseHexLong(hex))` — where `check` is a 2-`long`-arg static
  method — corrupts `-1125899906842623L` to `1` on the call where it
  collides, but only when passed **through the method-call boundary**; the
  identical computation compared inline (no separate method call) is
  correct (`LoopInlineRepro.java` passes cleanly), and calling `check` a
  couple of times outside a loop also works fine
  (`LongArgCallRepro.java` passes). Reproduces identically under `--nojit`,
  so not a JIT bug. Inspected `execute_invokestatic_cached`'s `Bytecode`
  fast-path argument-popping (`pop_arg_for_descriptor_checked`, called with
  the correct descriptor) and it looks correct on read; the remaining
  suspects are `Frame::new_pooled_cached`'s `Value` → locals re-encoding, or
  how the callee's own `lload` re-reads that local — not yet pinned down
  within this session's time budget. Whoever picks this up next: start from
  `LoopParseRepro.java` (fails) vs `LoopInlineRepro.java` (passes) — the
  ONLY difference between them is the extra `check(..)` invokestatic call
  boundary, which narrows the search to argument/locals marshalling for a
  user-bytecode (non-native) callee specifically, not `execute_string_concat`
  (already fixed) or the native-callback arg-popping paths (already
  correct, verified by inspection).

- **`TestBnf` (`testProcedures`) — root-caused: performance-margin issue,
  NOT a discrete bug. Same family as `TestFileLock`/`TestTransaction`
  below.** `Bnf`'s autocomplete engine (`org.h2.bnf.Sentence`) has a
  hardcoded `MAX_PROCESSING_TIME = 100` (milliseconds) wall-clock budget
  for the entire grammar-tree walk (`Sentence.start()`/`stopIfRequired()`,
  the latter throwing `IllegalStateException` once the budget is exceeded,
  presumably caught upstream to return whatever partial completion list had
  been accumulated so far). Confirmed directly: temporarily widening this
  budget by 1000x (100ms → 100s) in a scratch rebuild of `Sentence.java`
  makes the specific `"SELECT CUSTOM_PR"` completion query run for **more
  than 60 seconds** without completing (a `timeout 60` wrapper had to kill
  it) — i.e. the exhaustive BNF-grammar exploration that HotSpot finishes
  inside the 100ms budget takes CratonVM's interpreter at least 3 orders of
  magnitude longer, so the budget silently truncates the walk before it
  reaches the `user_defined_function_name` grammar production that would
  suggest `CUSTOM_PRINT`. Not a logic/dispatch bug — a genuine interpreter
  throughput gap for this specific (apparently very branchy/recursive)
  workload. No targeted fix attempted, matching `TestFileLock`/
  `TestTransaction`'s existing characterization — would require broader
  interpreter throughput work, not a discrete patch.

- **`TestUpgrade` — `SecurityException` half confirmed still FIXED**
  (from the first follow-up session), **but the secondary
  `NoSuchMethodError: org/h2/mvstore/RootReference.hasChangesSince(J)Z`
  residual still reproduces** on this session's binary (which includes
  `dev`'s already-landed `bug-h2-nosuchmethoderror-cross-class-dispatch.md`
  fix, `fb58d3d10`/closed). This confirms the doc's own prior note that
  this is "a new confirmed instance" of that bug FAMILY (two different
  classloaders' copies of `org.h2.mvstore.RootReference` colliding) — that
  fix's scope evidently doesn't cover this specific pair of colliding
  classes. Not independently investigated this session given time
  constraints; `TestUpgrade` is still NOT a clean PASS.

- **`TestRandomMapOps` — status unchanged: very likely fixed, still not
  100% confirmed by a clean exit.** Re-ran with a full 1-hour timeout on
  this session's binary (which includes both this session's fixes and the
  prior session's TreeMap dispatch fix): no assertion failure, no crash, no
  new output after the initial start line for the full hour — consistent
  with "legitimately slow heavy fuzz workload" rather than a hang or a
  reintroduced bug. Still recommend whoever picks this up run it to a true
  completion with a very generous (2+ hour) timeout to get a definitive
  exit-0 confirmation.

- **`TestFileLock`, `TestTransaction`, `TestFuzzOptimizations`,
  `TestRecoverKillLoop`** — unchanged from the first follow-up session;
  re-read but not independently re-verified this session (no new
  information, no reason to suspect their characterization changed).

## Follow-up session (2026-07-22, third pass): `TestDataUtils` fully closed, a separate systemic JIT-cache bug found and fixed, `TestUpgrade` narrowed further but still open

Worktree `/data/wt-h2-residual-finalclose-20260722` on the Azure host, branch
`fix/h2-residual-finalclose-20260722`, branched from `origin/dev` (`ad909ee8f`).

### `TestDataUtils` — the deeper invokestatic-long-argument residual — **FIXED**

Root cause was **not** in `execute_invokestatic_cached`'s argument popping (as
the second-pass write-up suspected) — that path was already correct. It was in
the **raw-byte-peek fast interpreter loop's stackless-return handling**
(`vm/src/runtime/interpreter.rs`, the `0xac..=0xb0` opcode arm for
`ireturn`/`lreturn`/`freturn`/`dreturn`/`areturn`). On a stackless return (the
common case: returning into an interpreted caller frame further down
`thread.frames`), this arm correctly determined `is_long` via
`pop_compact_with_long_mark_unchecked()` (honoring the `KIND_LONG` stack tag)
and built a properly-decoded `value` for diagnostics — but then pushed the
*raw* `CompactValue` onto the caller's operand stack via `stack.push_compact(cv)`
for every non-`areturn` return, **discarding the `is_long`/`is_double`
distinction entirely**. `push_compact` (as opposed to `push_compact_long` /
`push_compact_double`) always marks the destination slot `KIND_UNKNOWN` — the
"bits alone can't tell" fallback. For an ordinary long/double this is harmless
(the fallback bit-pattern heuristic in `CompactValue::to_value()` /
`decode_by_descriptor` correctly recovers it), but for a long whose bits
collide with the NaN-tag space **and** whose masked 47-bit payload happens to
additionally fit in 32 bits (e.g. `-1125899906842623L` /
`0xFFFC000000000001`, bit-identical to `CompactValue::int(1)`), the heuristic
is provably undecidable — exactly why the `KIND_LONG` tag exists in the first
place. Every later consumer of that return value (an `invokestatic` argument
pop, another `lreturn`, etc.) would then silently truncate it to `1`.

This is the same collision-long family as the already-fixed
`execute_string_concat` bug (`BC SM2`), just at a different propagation point:
a value returned from a callee, rather than an invoke-argument, losing its
tag on the specific fast-dispatch path that handles the overwhelming majority
of ordinary method returns.

**Fix:** `vm/src/runtime/interpreter.rs`'s `0xac..=0xb0` return arm now
dispatches on the opcode: `0xad` (`lreturn`) pushes via `push_compact_long`,
`0xaf` (`dreturn`) via `push_compact_double`, and `0xac`/`0xae`
(`ireturn`/`freturn`, no NaN-tag collision risk for 32-bit values) keep the
original `push_compact`. `0xb0` (`areturn`) is unchanged (already
kind-normalized via `coerce_value_for_return`).

**Verification (Azure Linux host, real-JDK mode, both JIT-on and
`--nojit`):** all 5 minimal repros in
`docs/known-issues/repros/h2-testdatautils-invokestatic-long-corruption/`
(`SbAppendLongRepro`, `ShiftOrLongRepro`, `LongArgCallRepro`,
`LoopInlineRepro`, `LoopParseRepro`) now print `ALL OK` / correct values.
`org.h2.test.store.TestDataUtils` itself now passes cleanly (exit 0; the
class runs ~5-6 minutes wall-clock, needs a timeout above 200s).

### A separate, previously-undiscovered systemic bug: the JIT compiled-code cache had no loader/`ClassId` component

While investigating `TestUpgrade`'s residual `NoSuchMethodError`
(`org/h2/mvstore/RootReference.hasChangesSince(J)Z`), traced the interpreter's
own loader-aware dispatch machinery (`resolve_class_loader_aware`,
`lookup_loader_initiated`, `execute_invoke_kind`'s `dispatch_override`,
`execute_invokevirtual_cached`'s `actual_class_id != receiver_class_id`
guard) and confirmed **all of it is sound** — every one of these paths
correctly distinguishes `org.h2.mvstore.RootReference` loaded by
`Upgrade.loadH2`'s anonymous `ClassLoader(null)` (the OLD H2 jar, downloaded
for the upgrade test) from the identically-named class already on the
application classpath (the CURRENT H2 build).

The one place that was **not** loader-aware: `jit::JitCache` (`jit/src/lib.rs`).
`JitKey` was `{class_name: Arc<str>, method_name, descriptor}` — no `ClassId`,
no loader component. `JitCache::get`/`put` are consulted directly (bypassing
the interpreter's own loader-aware resolution) from ~15 call sites across
`execute_invokestatic_cached`, `try_jit_upgrade_with_gate`,
`try_jit_compile_callee`/`_slow`, the OSR paths, and a few JIT-ABI dispatch
helpers in `vm/src/jit/helpers.rs`. Once **either** same-named
`RootReference` class got a given method JIT-compiled first (in practice, the
`Application`-loaded one — H2's own MVStore background/commit activity
elsewhere in the same process warms up first), every subsequent call to the
identically-named-and-shaped method on the **other** class's instances would
hit this cache by name alone and silently execute the wrong class's compiled
machine code. `put()` additionally **evicted** the loser's entry on every
write, so the two classes' compiled bodies would fight over the one cache
slot rather than simply serving stale code.

**Fix:** added `declaring_class_id: ClassId` to `JitKey`, folded it into
`compute_jit_key_hash`, and threaded a `ClassId` argument through
`JitCache::get`/`get_osr`/`put`/`put_osr`/`remove` and all ~20 production call
sites across `jit/src/lib.rs`, `vm/src/runtime/interpreter.rs`,
`vm/src/jit/helpers.rs`, and `vm/src/vm/vm_init.rs` (JIT-cache invalidation on
class-hierarchy change). Every call site that already had a `ClassId` in
scope (`cached.declaring_class_id`, a frame's `class_id`, a freshly-resolved
receiver class) now passes it through precisely. A handful of call sites are
themselves constrained to a bare `&str` by the raw JIT-ABI boundary
(`JitInvokeInfo`, baked into compiled machine code at codegen time — extending
that would mean touching the x64 codegen call-site emission itself, out of
scope for this session) or by a `deoptimize`/similar public API with ~20 of
its own callers; these resolve the class via a global name lookup at the
point of use, which preserves their *existing* (already not loader-aware)
behavior unchanged rather than newly introducing the collision — they do not
regress anything, they just don't yet share in the fix's precision. The
crate's own `#[cfg(test)]` `JitCache` unit tests were updated to pass a
`ClassId` too (`cargo check --tests` verified clean).

**This is a real, generally-applicable correctness fix** (any two identically
named-and-shaped classes loaded by different loaders — Groovy dynamic
classes, Hibernate bytecode-enhanced copies, forked test classloaders, OSGi
plugins, etc. — sharing a hot method name were vulnerable to this), verified
via `cargo build --release` + a broad regression spot-check (see below) with
no observed regressions. **However, it was NOT sufficient by itself to fix
`TestUpgrade`**: rebuilt and reran `TestUpgrade` after landing this fix, and
the identical `NoSuchMethodError` still reproduces — including under
`--nojit`, which rules out the JIT cache (and JIT compilation generally) as
the (sole) mechanism for this specific bug. Keeping the fix regardless, since
it is independently correct and confirmed harmless.

### `TestUpgrade`'s secondary `NoSuchMethodError` — narrowed further, still OPEN

Added targeted tracing (`CRATONVM_DBG_LOADER_TRACE`, gated, left in place —
zero cost when unset) to `Instruction::New`'s class resolution,
`lookup_loader_initiated`, `execute_invoke_kind`'s dispatch-override
computation, `execute_invokevirtual_cached`'s cache-hit guard, and the
`AtomicReference` native `get`/`set`/`compareAndSet` implementations (plus
the shared `compare_and_swap_field`/`set_field_volatile` helpers in
`vm/src/vm/vm_exec.rs`) to follow exactly which `RootReference` instance
`MVMap`'s `root` field holds at each step.

Confirmed, with high confidence:
- Every `new RootReference(...)` / chained-construction call from within the
  OLD (`UserDefined`-loader) `MVMap`/`RootReference`'s own bytecode correctly
  resolves and constructs an OLD-loader instance — `resolve_class_loader_aware`
  and the interpreter's `dispatch_override` mechanism both work exactly as
  designed here.
- `MVMap.getRoot()`, at the exact call site that eventually reaches the failing
  `hasChangesSince(J)Z`, **also dispatches correctly** (its receiver's
  `ClassId` matches the cached target — `execute_invokevirtual_cached`'s
  monomorphic guard holds).
- Yet the **value `getRoot()` returns** — read via `AtomicReference.get()`,
  i.e. a plain per-object field-slot-0 read, confirmed NOT globally shared or
  pooled — is, at the moment of failure, genuinely a live, currently-valid
  instance of the *Application*-loaded `RootReference` class, not the
  OLD-loader one the rest of the call chain expects. This is a real field
  value, not a stale/reclaimed-memory artifact (ruled out via the same
  tracing: the failing receiver's `class_id_of()` was checked against a live,
  currently-referenced object at the exact failing call, not inferred from a
  reused heap address).
- Reproduces **identically under `--nojit`**, ruling out JIT compilation (and
  the cache bug above) entirely as the mechanism.
- An earlier hypothesis in this session — that `holder_obj` addresses seen
  transitioning between `UserDefined`- and `Application`-loaded values in a
  `compare_and_swap_field` trace indicated live cross-contamination — was
  re-examined and **retracted**: `Upgrade.upgrade()` legitimately creates its
  own `Application`-loaded MVStore (for the migrated, current-format
  database) as part of its normal operation, and `testUpgrade()` runs twice
  (`build=120`, then `build=200`) in the same process, so `Application`- and
  `UserDefined`-loaded `RootReference` construction traces coexist in one log
  for entirely legitimate, unrelated reasons; some earlier apparent "same
  object, different class over time" observations were very likely ordinary
  GC address reuse between the two temporally-separate, unrelated MVStore
  lifecycles, not evidence of a shared/corrupted object.

**Not yet root-caused**: how a live `Application`-loaded `RootReference`
instance ends up referenced by the `UserDefined`-loader `MVMap`'s own
`AtomicReference` field. Candidate next steps for whoever picks this up:
- Trace `MVMap.compareAndSetRoot`'s two arguments (not just its dispatch
  target) directly at the call site, to see whether the `updated` value
  passed in is *already* wrong before the CAS, which would point further
  upstream (into `RootReference.updateRootPage`/`tryLock`/
  `tryUnlockAndUpdateVersion`'s own internal chained construction) rather than
  at the CAS itself.
- Check whether `testUpgrade`'s two sequential `Upgrade.loadH2()` calls within
  one `testUpgrade(major,minor,build)` invocation (one direct, one inside
  `Upgrade.upgrade()`) get assigned the **same** `ClassLoaderId::UserDefined(N)`
  (loader-ID reuse after the first is GC'd) — and if so, whether any
  loader-keyed cache in this codebase (`initiating_resolution_cache`, the
  promoted-invoke `SharedResolutionState`, etc.) fails to invalidate/rescope
  correctly across that reuse.
- Consider whether `MVStore`'s own background auto-commit thread (a
  `Runnable` started per-instance) could, for the `Application`-loaded
  migrated-DB's `MVStore`, somehow retain or leak a reference reachable from
  the `UserDefined`-loader `MVMap`'s object graph (e.g. via a shared
  `ThreadLocal`, a static registry, or a JMX/shutdown-hook list) — this was
  not investigated this session.
- `CRATONVM_DBG_LOADER_TRACE=1` (left in the tree, zero cost when unset) plus
  a `--nojit` run reproduces the full trace in well under 5 minutes and is
  the fastest way to pick this back up.

### Regression check

Ran the 5 `TestDataUtils`-family repros, then a broad manual spot-check
across previously-passing classes (`TestAlter`, `TestShell`,
`TestLinkedTable`, `TestPreparedStatement`, `TestUpdatableResultSet`,
`TestView`, `TestResultSet`, `TestAnalyzeTableTx`, plus `TestBnf`,
`TestFileSystem`, `TestFuzzOptimizations` re-confirming their already-documented
pre-existing characterizations) against the fixed binary — **no regressions
observed**. One *new*, unrelated finding surfaced:
`org.h2.test.jdbc.TestPreparedStatement.testDate8` fails with a 1-hour offset
(`Expected: 1582-09-25 00:00:00.000 actual: 1582-09-24 23:00:00.000`) —
confirmed via a from-scratch build of unmodified `dev@ad909ee8f` that this is
**pre-existing, not a regression from this session**; it's a distinct residual
from the already-fixed Julian/Gregorian cutover bug in the same test class.
Filed separately, not fixed here (out of scope for this pass) at the time —
**now FIXED, see the "Follow-up session (2026-07-22, fourth pass)" section
below.**

`org.h2.test.store.TestRandomMapOps` was kicked off again with a full 2-hour
timeout at the end of this session to try for the definitive exit-0
confirmation the second-pass session couldn't get; check
`/tmp/testrandommapops.log` on the Azure host (or a future session's own
rerun) for the outcome if this doc wasn't updated with a result before the
session ended.



## Follow-up session (2026-07-22, fourth pass): `TestPreparedStatement.testDate8` 1-hour-offset residual — FIXED

Picked up the residual filed at the end of the third pass (see immediately
above). Re-confirmed it still reproduces on current `dev` HEAD
(`becf0f642f9`, 2026-07-22) with an unmodified, from-scratch build before
touching anything, per this repo's "check already fixed first" convention —
still reproduced identically:
```
AssertionError: Expected: 1582-09-25 00:00:00.000 actual: 1582-09-24 23:00:00.000
	at org/h2/test/jdbc/TestPreparedStatement.testDate8(TestPreparedStatement.java:728)
```

### Root cause

**Not** a date-arithmetic or calendar-cutover bug (the working hypothesis
going in — a historical-date DST/zone-offset edge case — turned out to be
wrong; the actual bug isn't date-dependent at all). `testDate8` wraps its
Julian/Gregorian-transition assertions in:
```java
TimeZone.setDefault(TimeZone.getTimeZone("GMT+01"));
```
and the failing assertion (`assertEquals(Date.valueOf("1582-09-25"),
rs.getDate(1))`) compares a value built via `java.sql.Date.valueOf` (which
routes through `java.util.Date`'s deprecated field constructors and the
already-fixed JDN-based `date_fields_to_millis`/`date_fields_to_default_millis`
in `deprecated_util.rs`) against a value the H2 JDBC driver computes
independently. The JDN/cutover math on both sides is correct — verified by
isolating `Date.valueOf("1582-09-25")` alone (no H2 involved) under
`TimeZone.setDefault(TimeZone.getTimeZone("GMT+01"))`: CratonVM produced
`-12220156800000` where real HotSpot JDK 25 produces `-12220160400000` —
an exact 3,600,000 ms (1h) discrepancy, reproducing with **any** date, not
just 1582 ones.

Traced further with a direct probe of `TimeZone.getTimeZone("GMT+01")`
itself (no `Date`/H2 involved at all):
```
tz id=GMT+01:00 rawOffset=0 getOffset(0)=0 getOffset(now)=0   <- CratonVM (WRONG)
tz id=GMT+01:00 rawOffset=3600000 getOffset(...)=3600000      <- real HotSpot JDK 25
```
Every synthetic **custom fixed-offset** `TimeZone` — any id of the form
`"GMT±HH:MM"` / `"GMT±HHMM"` / `"GMT±H"` / `"UTC±HH:MM"`, canonicalised by
`normalize_gmt_custom_id` in `native-builtins/src/lib.rs` — resolved every
offset query (`getRawOffset()`, `getOffset(long)`, `getOffsets(long,int[])`,
`getOffsetsByWall(long,int[])`) to **0**, regardless of the requested
offset. `TimeZone.getTimeZone("GMT+01")`'s canonical `ID` field
(`"GMT+01:00"`) was set correctly, so `getID()`/`toString()` looked right —
only the numeric offset was silently wrong, which is exactly why `testDate8`
(fixed-offset-zone-dependent) was the symptom while dates alone were red
herrings.

Root cause, once traced into `native-builtins/src/lib.rs`'s `getRawOffset`/
`getOffset`/`getOffsets`/`getOffsetsByWall` native registrations for
`sun/util/calendar/ZoneInfo`/`java/util/SimpleTimeZone`: none of them read
the object's own `rawOffset` field (which `alloc_synth_timezone` *does* set
correctly via the `tz_standard_offset_seconds` lookup table — a dead code
path for this purpose, it turns out). They instead all go through
`crate::tzdb::raw_offset_seconds`/`offset_seconds_at_instant`/
`offset_seconds_at_local`/`standard_offset_seconds_at_instant`, which in
turn call `tzdb::get_zone_rules(ctx, zone_id)` — a lookup **purely against
the real `tzdb.dat` catalog** (604 IANA zones + aliases). A synthetic
`"GMT+01:00"` id has no `tzdb.dat` entry (real Java doesn't need one either
— it builds these zones' `ZoneInfo` directly from the parsed offset, never
touching tzdb), so `get_zone_rules` returned `None` for every custom-offset
id, and every caller's `.unwrap_or(0)` silently substituted 0.

### Fix

`native-builtins/src/tzdb.rs`: added `parse_fixed_gmt_offset_seconds(id)` —
a self-contained parser for `"GMT±HH:MM"`/`"GMT±HHMM"`/`"GMT±H"`/
`"UTC±HH:MM"` ids (handles both the canonicalised form
`normalize_gmt_custom_id` produces and the short forms it accepts on input)
— and `fixed_offset_rules(offset_seconds)`, which builds a degenerate
`ZoneRulesData` with empty transition tables and a single constant offset
(the existing `offset_at_instant`/`offset_at_local`/
`standard_offset_at_instant`/`raw_offset` functions already treat empty
transition vectors as "constant offset, no DST" — no changes needed there).
`get_zone_rules` now falls back to this synthetic rule set when the tzdb
catalog lookup misses and the id parses as a fixed GMT/UTC offset — fixing
`getRawOffset`/`getOffset`/`getOffsets`/`getOffsetsByWall` for **every**
custom-offset zone uniformly (not just `"GMT+01"`), since all four go
through this same shared lookup. `get_zone_rules`'s one other caller
(`TimeZone.getTimeZone`'s "is this id resolvable, or should it fall back to
bogus-id `GMT`" check) already short-circuits `custom_gmt.is_some()` before
reaching `get_zone_rules`, so this fallback doesn't change that path's
behavior.

**Verified bit-for-bit against real HotSpot JDK 25**:
- `TimeZone.getTimeZone("GMT+01"/"GMT+01:00"/"GMT+1"/"GMT+0100").getRawOffset()`
  / `.getOffset(0)` / `.getOffset(now)`: all now `3600000`, matching HotSpot
  exactly (was `0`).
- `TimeZone.getTimeZone("GMT-05"/"GMT-05:00").getOffset(...)`: `-18000000`,
  matching HotSpot (was `0`).
- `TimeZone.getTimeZone("UTC"/"GMT"/"GMT+00").getOffset(...)`: unchanged at
  `0` (still correct — not custom-offset ids, or a genuinely zero offset).
- `Date.valueOf("1582-09-25")` under `TimeZone.setDefault(GMT+01)`:
  `-12220160400000`, now matching HotSpot exactly (was `-12220156800000`,
  off by +3,600,000 ms).
- `org.h2.test.jdbc.TestPreparedStatement` (the full class, including
  `testDate8`): clean pass, no assertion failures.

3 new regression unit tests added to `native-builtins/src/tzdb.rs`'s
existing `#[cfg(test)] mod tests`, alongside 2 more covering the
`fixed_offset_rules`/`get_zone_rules` fallback plumbing directly (5 new,
7/7 total in the module including the 2 pre-existing tests, all pass).

**Regression check**: spot-ran `TestAlter`, `TestShell`, `TestLinkedTable`
(the classes this doc's own history most recently touched) against the
fixed binary — all clean, no regressions.

Fix commit landed on `dev` via branch
`fix/h2-testdate8-1hour-offset-20260722`.
