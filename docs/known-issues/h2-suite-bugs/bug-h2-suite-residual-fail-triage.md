# H2 suite — residual FAIL triage (2026-07-21): reproduced, narrowed, not fully root-caused

## Status
**OPEN, mixed, mostly closed after seven follow-up sessions.** Of the
original 11 items: **8 now confirmed FIXED, high confidence** (`TestPreparedStatement`,
`TestShell`, `TestRandomMapOps` [confirmed via a clean 2-full-pass, ~4
CPU-hour re-run with zero incidents — see the final "definitive re-run"
section for the corrected account; an earlier attempt had genuinely been
OOM-killed by an unrelated concurrent session and a prior version of this
doc incorrectly described that as a clean completion], `TestLinkedTable`, `TestAlter`,
`TestDataUtils` [now fully fixed — see the "Follow-up session (2026-07-22,
third pass)" section], plus the `SecurityException` half of `TestUpgrade`),
**1 root-caused as a genuine performance-margin issue rather than a
discrete bug, now with two confirmed instances** (`TestBnf` and, as of the
fifth pass, the separate `Bnf`/`RuleElement`-NPE doc — both the exact same
`Sentence.MAX_PROCESSING_TIME` budget-exhaustion mechanism; joining
`TestFileLock`/`TestTransaction` in that category), and **3 root-caused,
performance-margin, no fix expected/attempted** (`TestFileLock`,
`TestTransaction`, plus `TestFuzzOptimizations` which is
inconclusive/likely-not-CratonVM-specific). **One genuine open residual
remains requiring further VM work: `TestUpgrade`'s secondary
`NoSuchMethodError`** — narrowed much further across the third and fifth
passes (see those sections) but still not closed; the fifth pass pinned it
to a specific polymorphic inline-cache-miss correlation with a concrete
next instrumentation step. A **separate, previously-undiscovered systemic
bug was found and fixed** in the process (the JIT compiled-code cache was
keyed by class NAME only, with no loader/`ClassId` component — see the
third-pass section for the full writeup) — real and worth keeping, but
confirmed **not sufficient by itself** to close `TestUpgrade` (the
NoSuchMethodError reproduces identically with `--nojit`). A **ninth item is
now also FIXED**: the `TestPreparedStatement.testDate8` 1-hour-offset
residual discovered during the third pass (distinct from the
already-fixed Julian/Gregorian cutover bug in the same test class) — see
the "Follow-up session (2026-07-22, fourth pass)" section. See each item
below, and the "Follow-up session (2026-07-22, second pass)", "third
pass", "fourth pass", and "fifth pass" summaries further down, for full
detail.

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
  workload. **Second confirmed instance (2026-07-22, fifth pass)**:
  `TestWeb.testWebApp()`'s `autoCompleteList.do?query=select 'abc`
  empty-body failure — originally tracked as a separate doc
  (`bug-h2-bnf-ruleelement-link-null-npe-autocomplete.md`, a `RuleElement
  .link` NPE hypothesis from an incomplete isolated repro that skipped
  `linkStatements()`) — turned out to be this exact same budget-exhaustion
  mechanism for a query with an unclosed string literal; see that doc (now
  closed/reclassified) and the "fifth pass" section below for the full
  faithful-repro writeup. No targeted fix attempted, matching `TestFileLock`/
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

**Outcome (2026-07-22, same session) — CORRECTED (2026-07-22, fifth pass):**
the original writeup here claimed the run "completed its `timeout` wrapper
without ever exiting on its own — killed at the 7200s mark." **That is
factually wrong** — checked directly against the Azure host's kernel log
(`dmesg`) during the fifth-pass session: PID `1793972` was **OOM-killed by
the Linux kernel at 23:27:12 UTC**, i.e. **~99 minutes** into the run
(started 21:48), not reaped by the `timeout 7200` wrapper at the 2-hour
mark:
```
kernel: Out of memory: Killed process 1793972 (cratonvm-h2fina) total-vm:5700264kB, anon-rss:1962172kB, ...
```
This happened because the Azure host was concurrently running several other
sessions' heavy workloads (multiple WildFly node clusters, other H2/Tomcat
fixture runs, several `cargo build`/`cargo test` invocations) at the same
time, not because of anything specific to `TestRandomMapOps` itself — the
host's `free -g` showed 25GB reclaimed immediately after the kill, i.e. a
genuine system-wide memory-pressure event, not a leak in the test or the VM.
The **qualitative conclusion is still likely correct** (~99 minutes of
continuous CPU-bound execution with no assertion failure or crash before the
OOM kill is still meaningful evidence the original fast, deterministic
`rev (1654, null)` bug is gone) but it is **weaker evidence than the
original writeup claimed**, and a truly definitive exit-0 confirmation is
still outstanding. Whoever picks this up next should re-run with a
generous timeout **on a host that is not concurrently oversubscribed**
(check `free -g` and `ps aux --sort=-%mem` first — this Azure host runs many
concurrent orchestrated sessions and can OOM-kill an otherwise-healthy long
run), or run it with a lower per-process memory footprint / under `systemd-run
--scope -p MemoryMax=...` isolation so an unrelated session's memory spike
can't take it down.



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

## Follow-up session (2026-07-22, fifth pass): `TestUpgrade` narrowed to a
## specific inline-cache-miss correlation (still open); `Bnf`/`RuleElement`
## NPE doc reclassified as the same `TestBnf` family (closed)

Worktree `/data/wt-h2-testupgrade-20260722` on the Azure host, branch
`fix/h2-testupgrade-rootreference-20260722`, branched from `origin/dev`
(`ffe407a5f`).

### `TestUpgrade`'s secondary `NoSuchMethodError` — narrowed further via a
### precise inline-cache-miss correlation, still OPEN

Re-ran the existing `CRATONVM_DBG_LOADER_TRACE=1 --nojit` repro (confirms
the bug is unchanged/still open) and, this time, grepped the full
`[LOADER-TRACE]` output (100k+ lines) systematically instead of sampling,
cross-referencing `execute_invokevirtual_cached`'s HIT-CHECK lines against
the `compare_and_swap_field`/`set_field_volatile` lines by line-number
adjacency. Found a precise, reproducible correlation that narrows the
search significantly:

- **Confirmed (again, via a fresh trace) that every single `new
  RootReference(...)` allocation in the failure window resolves its target
  class correctly relative to its OWN referencing class's loader** — e.g.
  `referencing_class_id=ClassId(1619) referencing_loader=Some(UserDefined(5))
  target_class_id=ClassId(1619)`, and the one Application-context `new`
  observed in the same window (`referencing_class_id=ClassId(1168)
  referencing_loader=Some(Application) target_class_id=ClassId(1168)`) is
  likewise self-consistent. This rules out `Instruction::New`'s
  `resolve_class_loader_aware` call as the mechanism — reconfirms the
  third-pass session's finding, now with a wider sample.
- **New finding**: immediately before/after the corrupting
  `compare_and_swap_field` writes (a `cid=1168` `RootReference` landing in an
  `AtomicReference` holder whose entire history otherwise shows `cid=1619`
  objects — e.g. holder `0x200c647f4b0` gets 10 consecutive `cid=1619`
  writes, then one `cid=1168` write), the `execute_invokevirtual_cached`
  HIT-CHECK immediately preceding it shows a **polymorphic inline-cache
  miss** at the *same* call site:
  ```
  execute_invokevirtual_cached HIT-CHECK method=org/h2/mvstore/MVMap.compareAndSetRoot(...)Z
    cached.declaring=org/h2/mvstore/MVMap cached_receiver_class_id=ClassId(1162)
    actual_class_id=ClassId(1613) match=false
  compare_and_swap_field SUCCESS holder_obj=0x200c647f4b0 ... new_cid=1168
  execute_invokevirtual_cached HIT-CHECK method=org/h2/mvstore/RootReference.removeUnusedOldVersions(J)V
    cached.declaring=org/h2/mvstore/RootReference cached_receiver_class_id=ClassId(1619)
    actual_class_id=ClassId(1168) match=false
  ```
  and separately, aggregating every HIT-CHECK for `RootReference`'s
  package-private chained-update methods
  (`tryLock`/`updatePageAndLockedStatus`/`tryUnlockAndUpdateVersion`) across
  the whole run: **1 single occurrence** (out of ~300) of
  `RootReference.tryUnlockAndUpdateVersion(JI)... cached_receiver_class_id=
  ClassId(1619) actual_class_id=ClassId(1168) match=false` — i.e. a call
  site whose inline cache had been warmed by a `UserDefined(5)` receiver
  suddenly sees an `Application` receiver (or vice versa) exactly once, right
  in the failure window.
- **Ruled out**: the `actual_class_id != receiver_class_id` guard in
  `execute_invokevirtual_cached` (`vm/src/runtime/interpreter.rs`, the
  `CachedInvokeTarget::VirtualBytecode` arm) is itself correct — on a
  mismatch it unconditionally returns `CachedCallResult::CacheMiss`, forcing
  the slow path to re-resolve against the ACTUAL receiver's own class. By
  inspection this cannot be the mechanism that lets a wrong-class method body
  execute; the corruption has to be happening either in what the slow path
  resolves TO after a miss, or upstream of this guard (i.e. the receiver
  object itself, at the point it's pushed onto the operand stack for one of
  these calls, is already the wrong object — not a dispatch bug on a correct
  receiver, but a wrong receiver reaching a correct dispatcher).
- **Working hypothesis for the next session**: given both (new) is
  confirmed sound and (cache-miss guard) is confirmed sound, the remaining
  candidates are narrower than the third-pass session's list: (a) the
  SLOW-PATH re-resolution invoked after a `CacheMiss` on one of
  `RootReference`'s package-private chained-update methods
  (`tryLock`/`updatePageAndLockedStatus`/`tryUnlockAndUpdateVersion`/
  `updateRootPage`) — does it correctly re-cache keyed by the receiver's
  OWN `ClassId`, or could two different `RootReference` classes'
  call sites alias the same `thread.invoke_cache` slot
  (`(caller_class_id, cp_index, is_special)`)? (b) whether `AtomicReference
  .get()` (`native_atomic_ref_get`, `native-builtins/src/lib.rs`) can, under
  a race with a concurrent GC/compaction or a background MVStore thread,
  return a stale/wrong-generation value for a specific field-slot-0 read —
  not yet directly instrumented. **Concrete next step**: add a trace
  specifically at `native_atomic_ref_get`'s call site printing the
  `caller_class_id`/`cp_index` of the *calling* bytecode (not just the class
  of the returned value, which the existing trace already covers) for every
  `AtomicReference.get()` on a holder whose class is `RootReference`'s
  atomic root field — this would directly confirm or refute the
  `thread.invoke_cache` key-aliasing hypothesis in (a) above without another
  full investigative pass.

Doc updated in place, no code changes landed this session for `TestUpgrade`
itself (investigation-only pass). `CRATONVM_DBG_LOADER_TRACE=1` remains in
the tree, zero cost when unset, and is confirmed to reproduce the full trace
in well under 5 minutes.

### `Bnf`/`RuleElement.link` NPE doc — reclassified and closed

Investigated `bug-h2-bnf-ruleelement-link-null-npe-autocomplete.md` (the
`TestWeb.testWebApp()` autocomplete empty-body finding). The doc's own
`RuleElement.link` NPE hypothesis turned out to be a red herring from an
incomplete isolated repro that skipped the required `linkStatements()` call
(`docs-known-issue-doc-hypothesis-can-be-wrong-not-just-stale` applies). A
faithful repro replicating `WebSession.loadBnf()` exactly — all 7
`updateTopic()` calls, a real `readContents()`-populated `DbContents`
against a live H2 connection, then `linkStatements()` — shows the NPE does
**not** reproduce at all (`BnfProbe4.java`, worktree
`/data/wt-h2-testupgrade-20260722/apps/h2database/h2/BnfProbe4.java`).

The real mechanism: `getNextTokenList("select 'abc")` (unclosed string
literal) returns an empty result instead of suggesting the closing `'` —
root-caused (via the same "widen `Sentence.MAX_PROCESSING_TIME` in a scratch
rebuild" technique already used for `TestBnf` above) to the **exact same
100ms budget-exhaustion mechanism**: widening the budget to 30000ms makes
this query correctly return `{1#anything=Hello World, 1#'='}`. Not a
dispatch/NPE/loader bug — the same interpreter-throughput performance-margin
family as `TestBnf`/`TestFileLock`/`TestTransaction`. See the updated
`bug-h2-bnf-ruleelement-link-null-npe-autocomplete.md` for the full
corrected writeup; no code fix attempted or needed (matches this family's
existing "no targeted fix, broader interpreter throughput work" stance).

### Regression check

Ran `TestWeb` (the `select 'abc` case only, via `BnfProbe4`) and confirmed
the reverted/clean binary (all diagnostic edits to `Sentence.java`,
`WebSession.java`, `WebApp.java` backed out — those files aren't
git-tracked in this repo, see below) still fails `TestWeb.testWebApp()`
identically to `origin/dev`, i.e. this session made no code changes, purely
investigation + doc updates. Note: `apps/h2database/` is not tracked by
git in this repo (each worktree gets its own untracked local copy of the H2
source/build); any source-level scratch edits made while investigating
(temporary debug prints, the `Sentence.MAX_PROCESSING_TIME` widening) were
reverted in-place in this worktree's copy and are not part of any commit.

Fix commit landed on `dev` via branch
`fix/h2-testdate8-1hour-offset-20260722`.

## Follow-up session (2026-07-22, sixth pass): `TestUpgrade` — ruled out
## dispatch/resolution *and* cross-thread races; corruption traced to a
## stale (non-freshly-constructed) argument value, still OPEN

Same worktree/branch as the fifth pass
(`/data/wt-h2-testupgrade-20260722`, `fix/h2-testupgrade-rootreference-20260722`).
Picked up the fifth pass's concrete next step: added targeted tracing to
`native_atomic_ref_get`/`native_atomic_ref_cas`
(`native-builtins/src/lib.rs`) printing the *caller's* `ClassId` (via
`NativeContext::frame_class_ids()`) and thread id (via
`NativeContext::thread_id()`/`JvmThread::thread_id`) alongside the existing
held-object-class trace, plus a thread id on the `Instruction::New` trace
(`vm/src/runtime/interpreter.rs`) — all gated behind the existing
`CRATONVM_DBG_LOADER_TRACE` env var, zero cost when unset, **left in the
tree** (uncommitted as of this writeup — see "Status of this session's
changes" below).

### Two prior hypotheses now directly refuted

1. **Cross-thread race (the fifth pass's leading theory)**: refuted with
   hard evidence. For every "mixed" `AtomicReference` holder found (one
   whose CAS history shows objects from *both* `RootReference` classes —
   e.g. holder `0x200c624b2b0`: a healthy `cid=1617` (UserDefined(5)) write
   followed by a corrupting `cid=1168` (Application) write), **every single
   CAS on that holder happened on `thread=0`** — the same thread, no
   interleaving from thread 6/7/10 (which *do* exist and *do* call
   `compareAndSetRoot`, but only ever self-consistently within their own
   world, confirmed separately). This rules out a genuine data race between
   the main thread and `MVStore`'s background auto-commit thread(s) as the
   mechanism — a real, useful negative result, since the doc's third/fifth
   pass sections had flagged this as a leading candidate.
2. **Wrong-class dispatch on a correctly-identified receiver**: still
   refuted (reconfirmed) — `execute_invokevirtual_cached`'s mismatch guard
   is sound, and, new this pass, `resolve_class_by_name`'s "GLOBAL-FIRST
   fallback" path (`vm/src/runtime/interpreter.rs` ~L17880-17910, the
   function containing the `[LOADER-TRACE] name=... resolved via
   GLOBAL-FIRST fallback` line seen throughout every trace so far) was
   directly checked and is **not** the mechanism either: `grep`-ing every
   trace run for a `GLOBAL-FIRST fallback` event whose `referencing_loader`
   is `UserDefined(_)` (i.e. a case where UserDefined(5) bytecode should
   have used loader-aware resolution but fell through to the loader-blind
   global path instead) returns **zero hits**, in any of this session's
   three trace runs. Every `GLOBAL-FIRST fallback` observed is `Application`
   resolving `Application` — itself correct, expected behavior (Application
   classes have no reason to use loader-initiated resolution), not a bug.
   `should_use_loader_initiated_resolution`
   (`vm/src/runtime/interpreter.rs` ~L17653) does gate loader-aware
   resolution behind a narrow Groovy/Spring-fork allowlist *unless*
   `CRATONVM_LOADER_AWARE_RESOLUTION` is set — but that env var's own
   default (`vm/src/runtime/env_cache.rs::loader_aware_resolution`,
   `Err(_) => true`) is **on** by default, and the gate's own
   `get_loader_id(referencing_class_id)` direct-hit path (checked before
   ever consulting the narrow allowlist) correctly identifies UserDefined(5)
   classes in every observed case — so this path is sound for this bug,
   despite superficially looking like a promising lead (a stale doc comment
   two lines above the function, claiming the gate "stays off" by default,
   is itself now wrong/outdated and worth a follow-up correction someday,
   but is not connected to this bug).

### New finding: the corrupting object is not freshly constructed nearby

For the specific corrupting CAS events examined line-by-line (e.g. holder
`0x200c624b2b0` at trace line ~94531 in
`/tmp/testupgrade-trace3.log` on the Azure host — not preserved past the
session, re-run `CRATONVM_DBG_LOADER_TRACE=1 --nojit org.h2.test.unit
.TestUpgrade` to reproduce, takes well under 5 minutes): the ~40 lines
immediately preceding the corrupting `native_atomic_ref_cas PRE ...
new_cid=1168` contain **no** `[LOADER-TRACE] new ...RootReference` line at
all — only a burst of ~14 repeated `resolve name=RootReference
referencing_class_id=ClassId(1168) referencing_loader=Some(Application)`
lines (Application's own, unrelated `RootReference` class-name resolution
activity, running sequentially on the *same* thread=0 immediately before,
not concurrently) followed directly by the corrupting CAS. Since every
`new RootReference(...)` allocation observed anywhere in three separate
trace runs this session (and the fifth pass) resolves its target class
correctly relative to its own referencing class, and none appears in this
specific window, the `RootReference` object being passed as `updated` to
UserDefined(5)'s `MVMap.compareAndSetRoot` here was **not just constructed
here** — it must already have existed (most plausibly: it's the object
Application's own, immediately-preceding, unrelated code was just working
with) and is reaching this call site as an already-stale value in some
storage location the interpreter believes holds `tryUpdate`'s fresh
`updatedRootReference` parameter.

### Working hypothesis for the next session: local-variable/argument-slot
### staleness in `RootReference.tryUpdate`'s invokespecial call

Given `RootReference.tryUpdate(RootReference<K,V> updatedRootReference)`
is private (2 local slots: `this`, `updatedRootReference`) and is always
called as `tryUpdate(new RootReference<>(this, ...))` from a sibling
private/package-private method
(`updateRootPage`/`tryLock`/`updatePageAndLockedStatus`/
`tryUnlockAndUpdateVersion`), the most concrete remaining explanation
consistent with every finding so far (sound dispatch, sound `new`
resolution, no cross-thread race, corrupting value not freshly
constructed) is **frame/local-slot reuse**: if the interpreter pools/reuses
`Frame` objects (`Frame::new_pooled_cached`, already flagged as a suspect
in the second-pass session's `TestDataUtils` investigation for a *different*
bug — the `KIND_LONG` tag-loss family, since fixed) and a pooled frame
previously used for an Application-context `tryUpdate` call still has
`updatedRootReference`'s local slot populated with that Application
`RootReference` object, a bug in the NEW call's argument-marshalling that
fails to overwrite that slot (e.g. an early-return/fast-path that assumes
"same slot count, skip re-writing" for some class of invokespecial calls)
would produce exactly this symptom: `tryUpdate` reads its OWN stale local
instead of the freshly-`new`'d argument the caller actually pushed.

**Concrete next step, not yet attempted**: instrument
`execute_invokespecial_cached`/whatever code path marshals arguments into
a newly-entered private-method frame (grep `Frame::new_pooled_cached` and
its callers in `vm/src/runtime/interpreter.rs`) to print, on frame entry
for `RootReference.tryUpdate`/`tryLock`/`updatePageAndLockedStatus`/
`tryUnlockAndUpdateVersion` specifically, (a) the object reference actually
present in local slot 1 immediately after argument marshalling completes,
compared against (b) the object reference that was on top of the operand
stack in the CALLER's frame immediately before the `invokespecial`
instruction executed. A mismatch between (a) and (b) for any call would be
a direct, unambiguous confirmation of this hypothesis and would pinpoint
the exact marshalling function responsible.

### Status of this session's changes

The `native_atomic_ref_get`/`native_atomic_ref_cas`/`Instruction::New`
thread-id-and-caller-class tracing added this session is a small,
`CRATONVM_DBG_LOADER_TRACE`-gated, zero-cost-when-unset diagnostic — same
category as the existing loader trace already in the tree — and is left
**uncommitted** in `/data/wt-h2-testupgrade-20260722` pending review (not
pushed to `dev` as of this writeup; whoever picks this up next should
either commit it as-is or fold it into whatever further instrumentation
the "concrete next step" above requires). The Azure host this session ran
on was under heavy, fluctuating memory pressure from several concurrent
orchestrated sessions for most of this pass (two `cargo build` attempts
were `SIGKILL`'d by the OOM killer before a third succeeded once load
dropped) — worth checking `free -g`/`ps aux --sort=-%mem` before assuming a
build failure here is a code problem rather than host contention.

### Addendum (same session, static check while `TestRandomMapOps` ran):
### `init_locals_pooled`/`Frame::new_pooled_cached` ruled out

Read `vm/src/runtime/frame.rs`'s `Frame::new_pooled_cached` and the
`init_locals_pooled` helper it calls directly (the pooled-frame
construction path the sixth pass's hypothesis pointed at). Both look
correct on inspection: `init_locals_pooled` unconditionally does
`locals.clear(); locals.resize(n, uninitialized); kinds.clear();
kinds.resize(n, LKIND_OTHER);` — a full wipe — before
`copy_args_to_locals` writes the actual call arguments starting at slot 0,
for exactly `args.len()` slots. There is no code path here that could
leave a stale value from a previous pooled use in a slot the new call
should have populated; a pooled `Vec`'s prior contents are discarded, not
selectively overwritten. This rules out the pooled-frame *construction*
step specifically as the mechanism — it does not rule out the hypothesis
generally, since the actual argument values (`args`/`args_slice`) are
built *before* this function is called, by popping the operand stack in
the invoke dispatcher (`execute_invokevirtual_cached`, the
`CachedInvokeTarget::VirtualBytecode` arm, `vm/src/runtime/interpreter.rs`
~L37170-37310 for the cache-hit path, a parallel cache-miss path nearby).
**Narrows the "concrete next step" from the sixth pass**: the remaining
suspect is specifically the operand-stack argument *popping* for an
`invokespecial` call to `RootReference.tryUpdate` (i.e. `is_special=true`
in this dispatcher) — not frame/locals construction, which is now
confirmed clean.

### `TestRandomMapOps` — definitive re-run (2026-07-23, same session as the
### sixth/seventh pass): 2 full passes clean, killed by timeout as expected

Re-ran with a fresh 4-hour `timeout` wrapper (worktree
`/data/wt-h2-testupgrade-20260722`, binary
`target/release/cratonvm-testupgrade3`, started 2026-07-23 00:26:06 UTC) on
a host with confirmed-available memory (checked `free -g` immediately
before launch), specifically to get a real outcome after the OOM-truncated
attempt this doc previously (and incorrectly) described as a clean 2-hour
completion.

**Outcome: clean, unambiguous.** The run completed **two full passes**
with zero assertion failures and zero crashes:
```
02:09:09  1:43:02.634  Done pass #0
03:53:40  3:27:34.053  Done pass #1
```
then continued silently into pass #2 for another ~32 minutes before the
`timeout 14400` wrapper reaped it at the 4-hour mark (00:26:06 + 4h =
04:26:06 UTC; process confirmed gone by the next check at 04:29). Checked
`dmesg` for an OOM kill on this PID — none found this time (contrast with
the prior attempt on this same day, PID `1793972`, which genuinely was
OOM-killed at ~99 minutes by an unrelated concurrent session's memory
spike, as this doc's sixth-pass-adjacent correction already recorded). No
output whatsoever in the log between `Done pass #1` and the process
disappearing — consistent with `timeout` cleanly `SIGTERM`/`SIGKILL`-ing a
still-healthy, still-computing process mid-pass, not with a hang (the
process's CPU time tracked wall-clock 1:1 the entire run, confirmed via
repeated `ps` samples during the run) or a crash (a crash or uncaught
exception would have printed to the log before the process exited; this
log's last line is the routine `Done pass #1` progress message).

**Conclusion**: this is materially stronger evidence than any previous
session obtained — 2 complete passes (not 0, as in every prior attempt)
plus a third partial pass, ~4 full CPU-hours, zero incidents. Upgrading
the characterization from "very likely fixed, not 100% verified" to
**fixed, high confidence** — the original fast, deterministic `rev (1654,
null)` assertion failure this class used to hit immediately is
conclusively gone, and nothing in 4 hours of continued heavy fuzz load
surfaced any other issue. A literal exit-0 (all 100 rounds) is still not
practically obtainable in any reasonable session's timeframe given
`TestAll.big`'s workload size and current interpreter throughput, and
isn't necessary to close this out — treating this as closed.

## Follow-up (2026-07-23): independent reconfirmation via new `apps/h2database-suite-runner`

A new Linux suite runner (`apps/h2database-suite-runner`, one-process-per-class
driver for the whole H2 suite, mirroring the Spring Boot / Elasticsearch
runners but for Linux) ran the full 218-class suite against a `dev` binary
built well after this doc's most recent (sixth) pass, independently
re-hitting `TestBnf`, `TestFileLock`, and `TestTransaction` — all three at
the exact same assertions already characterized above as performance-margin,
no-fix-attempted issues, confirming they're still live on current `dev`, not
stale:

```
org.h2.test.unit.TestBnf.testProcedures (TestBnf.java:138)
  AssertionError: Expected: true got: false

org.h2.test.unit.TestFileLock.testSimple (TestFileLock.java:99)
  JdbcSQLNonTransientConnectionException (wrong error code — expected 90020)

org.h2.test.db.TestTransaction.testMergeUsing (TestTransaction.java:446)
  AssertionError: Expected: 100 actual: 50
```

`TestUpgrade` also still fails, consistent with this doc's characterization
of its secondary `NoSuchMethodError` as the one genuine open residual
requiring further VM work — the fresh run's stack trace bottoms out in
`org/h2/mvstore/MVMap.hasChangesSince` → `MVStore.storeNow`/`store`/
`tryCommit` → `TransactionStore.endTransaction` → `Transaction.commit` →
`Session.commit`, same call shape as this doc's third/fifth/sixth-pass
narrowing.

Full run results, HotSpot-baseline comparison, and a broader FAIL/HANG
triage of the rest of the suite (including 4 newly-found, unrelated
CratonVM bugs) are in `apps/h2database-suite-runner/RESULTS-20260723.md`
and `RESULTS-20260723-hangrerun.md`. No new investigation was done on
`TestBnf`/`TestFileLock`/`TestTransaction`/`TestUpgrade` specifically in
that session beyond reconfirming they still reproduce — this doc's existing
characterization stands.

## Follow-up session (seventh pass, 2026-07-23): two more real dispatch bugs
## found and fixed via new targeted tracing; `TestUpgrade` narrowed further
## but still OPEN

Worktree `/data/wt-h2-round7-20260723` on the Azure host, branch
`fix/h2-testupgrade-round7-20260723`, branched from `origin/dev`
(`893ddbc73`). Picked up the sixth pass's "concrete next step" (compare the
value on the caller's operand stack immediately before an invokespecial/
invokevirtual dispatch against what the callee's frame actually receives)
and went further than prior passes by adding fresh, call-site-specific
tracing rather than re-reading the existing `[LOADER-TRACE]` output.

### Bug found and fixed #1: cached/fast invoke-dispatch paths popped object
### args without the GC-forwarding barrier `execute_invoke_kind` already has

`execute_invoke_kind` (the slow, uncached invoke dispatcher) has a
documented barrier: args popped off the operand stack into a plain
(non-GC-rooted) buffer get `shared.heap.load_and_forward()` applied to every
`Value::Object` before use, because a moving-GC evacuation in the gap
between popping and copying into the callee's locals can otherwise leave a
stale from-space address in that buffer. Auditing every OTHER call-site that
pops args the same way (`execute_invokevirtual_cached`'s `VirtualBytecode`
and `Bytecode` arms, `execute_invokevirtual_vtable_fast`,
`execute_invokestatic_cached`'s `Bytecode`-cache-hit arm,
`pop_coerced_invoke_args_virtual`/`_static`/`_intrinsic`) found **none** of
them had this barrier — only the slow path did. Added a shared
`refresh_stale_object_args` helper and called it at all 8 sites. Verified
harmless via regression spot-checks (`TestAlter`, `TestShell`,
`TestLinkedTable` — all clean) and via a full release build. **This is a
real, generally-applicable correctness fix** (same shape as the third
pass's JIT-cache fix): any hot cached-dispatch call site popping a
newly-allocated or recently-moved object argument was vulnerable to reading
a stale address if a GC evacuation landed in the narrow window between pop
and frame-construction. **However, confirmed NOT sufficient by itself to
fix `TestUpgrade`** — rebuilt and reran; the identical `NoSuchMethodError`
still reproduces. Keeping the fix regardless (same rationale as the third
pass's JIT-cache fix: independently correct, confirmed harmless).

### Bug found and fixed #2: `resolved_private_invokevirtual_target`'s
### loader-blind fallback

Added targeted per-call tracing (gated behind the existing
`CRATONVM_DBG_LOADER_TRACE`) specifically to `RootReference.tryUpdate`
argument popping at every candidate dispatch site, plus confirmed via
`javap -c` that `tryUpdate`'s callers (`updateRootPage`/`tryLock`/
`tryUnlockAndUpdateVersion`/`updatePageAndLockedStatus`) compile their
`this.tryUpdate(new RootReference<>(...))` self-calls as **`invokevirtual`**,
not `invokespecial` — a real (if unusual) javac quirk for this era of
bytecode: private methods can still be encoded with opcode `0xb6`. CratonVM
already has dedicated handling for exactly this shape,
`resolved_private_invokevirtual_target` (`vm/src/runtime/interpreter.rs`),
whose own doc comment explains the general problem correctly. But its
resolution of the target class was:
```rust
let target_class_id = lookup_loader_initiated(shared, current_class_id, method_class_name)
    .or_else(|| shared.class_manager.read().get_loaded_class_id(method_class_name))?;
```
`lookup_loader_initiated` is loader-aware and correct when it hits, but its
`.or_else` fallback, `get_loaded_class_id`, is a **global, name-only,
single-slot "first loaded wins" lookup** — exactly the same bug class as
the third pass's JIT-cache-key gap and the "GLOBAL-FIRST fallback" pattern
already seen elsewhere in this file's history. For two classloaders each
defining their own `org/h2/mvstore/RootReference` (the `Upgrade.loadH2`
shape this whole doc keeps circling back to), a miss in
`lookup_loader_initiated` — plausible here since this is a class resolving
**itself** by name at a private self-call site, not a delegated import,
which loader-initiated-resolution tables are not necessarily indexed for —
falls back to returning whichever copy loaded first (observed to always be
the Application one), silently pinning the private call's dispatch to the
**wrong loader's** method body/constant pool while the receiver stays the
caller's own (correct-loader) object.

**Fix:** since a private method is, by JVM access control, only ever
legally invoked from within the exact class that declares it, the CP-resolved
owner name at any legitimate private-via-invokevirtual call site always
names the caller's own class. Added a `self_match` fast path that resolves
directly against `current_class_id` (zero lookup, zero loader ambiguity)
whenever `current_class_id`'s own name matches `method_class_name`, before
ever consulting `lookup_loader_initiated`/`get_loaded_class_id`. Verified
harmless via the same regression spot-checks and a clean release build.
**This is also a real, generally-applicable correctness fix** — any
private-method self-call compiled as `invokevirtual` under two classloaders
defining the same-named class was vulnerable. **Also confirmed NOT
sufficient by itself to close `TestUpgrade`**: rebuilt and reran with
`--nojit`; the identical `NoSuchMethodError` still reproduces, and the new
`[TRYUPDATE-TRACE/*]` instrumentation (left in the tree, gated behind
`CRATONVM_DBG_LOADER_TRACE`, zero cost when unset) shows every sampled
`tryUpdate` call in a full run was self-consistent (`Application` caller →
`Application` receiver/arg, or vice versa) — the actual corrupting
`UserDefined`-context `tryUpdate` call was never captured in this pass's
sampling window (`UserDefined`-loader `RootReference` activity is rare —
only 1 loader-scoped `resolve name=RootReference` event for the whole
`UserDefined(5)` loader across a full run — so a 300s window can miss it).

### Narrowed further: the corruption is confirmed downstream of `tryUpdate`'s
### own (now more rigorously verified sound) dispatch and frame construction

New direct evidence this pass, not available to any prior pass:

- The failing call's own WARN line names the exact caller precisely:
  `NoSuchMethodError method="org/h2/mvstore/RootReference.hasChangesSince(J)Z"
  caller="org/h2/mvstore/MVMap.hasChangesSince(J)Z @pc=8"`. `MVMap.
  hasChangesSince(long)` (single-arg) only exists in the OLD (1.4.200)
  `MVMap` — current `MVMap`/`RootReference` both take a 2-arg
  `(long,boolean)` `hasChangesSince`. So the CALLING `MVMap` instance is
  unambiguously the `UserDefined`-loader (old) one; `hasChangesSince` on
  `RootReference` is **package-private, not private** — a genuinely
  polymorphic `invokevirtual` dispatched correctly by receiver class (not
  covered by `resolved_private_invokevirtual_target` at all, and not shown
  to be buggy). The `NoSuchMethodError` is a **faithful, correct**
  consequence of dispatching against whatever object is actually sitting in
  `this.root` (an `AtomicReference<RootReference>` field on the OLD `MVMap`)
  at the time of the call — which is an `Application`-loaded (current)
  `RootReference` instance, lacking the 1-arg overload.
- Re-confirmed (sixth pass) that every CAS onto the SPECIFIC `AtomicReference`
  holder that ends up "mixed" (a `UserDefined`-cid write followed later by an
  `Application`-cid write) targets the **same physical holder object**
  throughout — i.e. `MVMap.compareAndSetRoot`'s dispatch itself is NOT
  landing on the wrong `MVMap` instance; the SAME (old) `MVMap`'s own `root`
  field gets a value CAS'd into it that is already wrong by the time the CAS
  runs. Combined with this pass's `tryUpdate`-argument-popping and
  frame-construction fixes both landing clean without closing the bug, the
  remaining candidates narrow to: (a) `MVMap.compareAndSetRoot`'s own
  (package-private, polymorphic) invokevirtual argument marshalling — not yet
  traced with the same rigor `tryUpdate`'s was this pass — or (b) something
  upstream of `tryUpdate` entirely, e.g. `Page.map` (a `final` field set once
  at `Page` construction, `Page.java:131-141`) holding a cross-loader-wrong
  `MVMap` reference for some `UserDefined`-loader `Page`, which would make
  `root.map.compareAndSetRoot(...)` (in `tryUpdate`'s own body — `root` here
  is `RootReference.root`, a `Page`, NOT the `MVMap.root` `AtomicReference`;
  the two same-named fields on different classes are easy to conflate when
  reading this trail) dispatch correctly-per-its-wrong-receiver onto the
  Application MVMap, silently missing the OLD MVMap's actual root field
  entirely.

**Concrete next step, not yet attempted**: instrument `Page`'s constructors
(`Page.java:131,135,140`) to trace `map` field identity (loader) against the
caller's own loader context, specifically for `UserDefined`-loader-context
`Page` construction/copying paths (page splits, `Page.copy()`-style clones)
— the same "does the constructor argument's loader match the constructing
context's loader" question this pass answered for `tryUpdate`, one level
further up the object graph. Second candidate: extend the same
per-call-site tracing technique this pass used for `tryUpdate` to
`MVMap.compareAndSetRoot` itself (it was traced only via the pre-existing,
coarser `execute_invokevirtual_cached` `HIT-CHECK` tag in every prior pass,
never with `describe()`-style before/after argument dumps).
`CRATONVM_DBG_LOADER_TRACE=1 --nojit org.h2.test.unit.TestUpgrade`
continues to reproduce the failure in well under 5 minutes.

### Same-session addendum: `MVMap.compareAndSetRoot`'s own argument
### marshalling directly traced — the corrupting call finally caught

Extended the same per-call `describe()`-style tracing to
`MVMap.compareAndSetRoot` itself (never before traced this precisely — only
via the coarser, cache-hit-only `HIT-CHECK` tag, which never fired for it
in any pass since it apparently never gets served from the cache within a
single `TestUpgrade` run — it dispatches once via
`execute_invokevirtual_vtable_fast` or, rarely, the slow path, and the run
fails before it warms up further). Instrumented all three of
`compareAndSetRoot`'s possible dispatch sites
(`execute_invokevirtual_vtable_fast`, `execute_invokevirtual_cached`'s
`VirtualBytecode` arm, `execute_invoke_kind`) uniformly, printing
`receiver_map` (`args[0]`, the `MVMap` this call executes against),
`expected` (`args[1]`), and `updated` (`args[2]`) — each as
`addr / cid / loader_class`.

**327 calls captured in a full `--nojit` run. 326 are perfectly
self-consistent**: receiver `MVMap` cid `1162`/`1250` (`Application`'s two
observed class-registration numbers within this run — H2 appears to
load/re-resolve `MVMap` more than once) always paired with
`expected`/`updated` cid `1168` (`Application` `RootReference`); receiver
cid `1613` (`UserDefined` `MVMap`) paired with `1619` (`UserDefined`
`RootReference`) 17/18 times. **The 327th (last) call is the anomaly**:
```
[CASROOT-TRACE/vtfast] caller_class_id=ClassId(1168) cp_index=100
  receiver_map(args[0])=cid=ClassId(1613) loader_class=org/h2/mvstore/MVMap
  expected(args[1])=cid=ClassId(1168) loader_class=org/h2/mvstore/RootReference
  updated(args[2])=cid=ClassId(1168) loader_class=org/h2/mvstore/RootReference
```
The **caller** here is `Application`-loaded code (`caller_class_id=1168` —
almost certainly `Application`'s own `RootReference.tryUpdate`, called on an
`Application` `this` with a freshly-constructed `Application`
`updatedRootReference`, both self-consistent per the `expected`/`updated`
pair). But the **receiver** — `this.root.map` read from inside that
`tryUpdate` body (`RootReference.root` is a `Page<K,V>`; `Page.map` is the
`final` field pointing back to the owning `MVMap`) — resolves to the
**`UserDefined` `MVMap`** instead of `Application`'s own. This is the
*opposite* direction from every prior pass's working hypothesis (which
assumed the old/`UserDefined` side reaches into the new/`Application` side);
here it's `Application`'s own migration-time code that ends up holding a
`Page` whose `map` field still points at the **old** `MVMap`.

This single event is a strong, direct, first-of-its-kind capture of the
actual corrupting call (not an inference from a `NoSuchMethodError`
several frames downstream) — and it reframes the search: `Page.map` is
`final`, set once at construction (`Page.java:131/135/140`) and preserved
byte-exact by `clone()` (audited this pass — `native_object_clone` in
`native-builtins/src/lib.rs` allocates the clone via the SOURCE object's own
`class_id` directly, not a name lookup, and copies fields by index; this is
loader-correct and confirmed NOT the bug). So some code path, running in
`Application`'s own context during `Upgrade.upgrade()`'s migration, must be
constructing (or reusing) a `Page` with the `UserDefined` `MVMap` passed as
its `map` constructor argument — plausibly a page-copying step that
legitimately touches both old and new stores during migration, where a
transient/scratch `Page` (correctly holding a reference to the OLD `MVMap`
for the copy) ends up incorrectly wired into `Application`'s own live root
chain instead of being replaced with a properly-`Application`-owned `Page`
before that chain is committed.

**Concrete next step, not yet attempted**: this reframes the search away
from generic VM dispatch machinery (dispatch, argument marshalling, GC
forwarding, and private-invokevirtual resolution are now all either fixed
or ruled out) and toward `Page` **construction call sites specifically
active during `Upgrade.upgrade()`'s migration path** — grep
`org/h2/mvstore/MVStore.java`'s and `MVMap.java`'s page-copy/migration
logic (whatever `Upgrade.upgrade()` calls to move data from the old store
into the new one) for any `new Page<>(...)` or `page.copy(...)` call that
passes a captured/closed-over `MVMap` reference rather than deriving it
fresh from the CURRENT context. Trace `Page`'s three constructors
(`Page.java:131,135,140`) themselves next, gated the same way, printing the
`map` argument's loader identity against the CALLING frame's own loader —
mirroring exactly what this pass did for `tryUpdate` and
`compareAndSetRoot`, one level further into the object graph.
`CRATONVM_DBG_LOADER_TRACE=1 --nojit org.h2.test.unit.TestUpgrade`
continues to reproduce in well under 5 minutes; the new `[CASROOT-TRACE/*]`
tags are left in the tree alongside `[TRYUPDATE-TRACE/*]`, both zero-cost
when the env var is unset.



### Same-session addendum #2: `Page` constructors and `copy(map, ...)` call
### sites ruled out — corruption entry point still not pinned down

Extended tracing to `Page`'s three constructors (`Page.java:131,135,140` —
`<init>` calls, gated on `class_name.contains("Page")`), comparing each
call's own class context against the `map` constructor argument's loader.
**1542 constructor calls captured in a full run — zero mismatches.** Every
`Application`-context `Page`/`Leaf`/`NonLeaf` construction receives an
`Application` `map`; every `UserDefined`-context one receives a
`UserDefined` `map`, without exception. `Page` construction itself is
clean.

Also read (not traced — structurally unambiguous) both call sites of the
abstract `Page.copy(MVMap<K,V> map, boolean eraseChildrenRefs)`:
`MVMap.java:650` (`root = root.copy(this, false)`) and `MVMap.java:1182`
(`source.copy(this, true)`) both pass the **enclosing method's own `this`**
as the `map` argument — this can't independently introduce a cross-loader
value; a bug here would only be a symptom of the enclosing `MVMap` method
already executing against the wrong receiver, which is a dispatch question
already covered (and not found buggy) by this pass's earlier tracing. The
six other `.copy()` call sites in `MVMap.java` are all the no-arg
`Page.copy()` (→ `clone()`, already audited as loader-correct).

**Net result of this pass's four tracing rounds** (`tryUpdate`,
`compareAndSetRoot`, `Page.<init>`, `Page.copy()`'s call sites): the
*single* concretely-caught corrupting event remains the one
`compareAndSetRoot` call recorded in the first addendum above —
`Application`-context code reading `this.root.map` (i.e. `Page.map` on
whatever `Page` `RootReference.root` pointed to at that moment) and getting
`UserDefined`'s `MVMap`. Every upstream construction/assignment path this
pass checked is individually clean, which either means (a) the corrupting
`Page` was legitimately constructed with the `UserDefined` `map` at some
EARLIER point for a legitimate transient reason (H2's own migration logic
touching both stores) and something fails to swap it out before it reaches
`Application`'s live root chain — an H2-semantic/timing bug surfaced by
CratonVM rather than a CratonVM dispatch bug per se — or (b) the actual
mutation happens through a path not yet traced (e.g. `RootReference`'s
`previous`-chain-walking constructor, which copies `r.root` directly rather
than constructing a `Page`; or a `Page` field mutated post-construction via
some non-`<init>` route this session didn't consider).

**Not attempted this session, for whoever picks this up next**: trace
`RootReference`'s "version change" constructor
(`RootReference(RootReference<K,V> r, long version, int attempt)`,
`RootReference.java`, the one that walks `r.previous`) — it's the one
private constructor whose body reads a field (`r.root`) from its argument
rather than only forwarding constructor parameters straight through, making
it structurally different from the four already-clean `tryUpdate`-adjacent
constructors this pass checked. Second: reconsider whether the bug is in H2
itself (does the SAME migration sequence, run under a debugger or with
extra logging on real HotSpot, ever transiently hold a stale old-store
`Page` reference the way this trace shows CratonVM doing? If HotSpot
provably never does, that argues for (b) above rather than (a)).



### Same-session addendum #3: fixed a real receiver GC-forwarding gap too
### (`peek_at` without forwarding) — also NOT sufficient, and this matters

`execute_invokevirtual_vtable_fast` and three arms of
`execute_invokevirtual_cached` (`VirtualBytecode`, `VirtualNative`,
`Intrinsic`) all obtain the dispatch receiver via a bare
`stack.peek_at(num_params)` and immediately call `shared.heap.class_id_of`/
`kind_of` on it to pick the dispatch target — with **no**
`load_and_forward` barrier, unlike `execute_invoke_kind`'s slow path (where
the receiver is `args[0]`, covered by the args-forwarding loop). This is a
real gap of the same shape as addendum #1's fix, and matters specifically
because the compareAndSetRoot corruption (addendum #1) was caught via
`[CASROOT-TRACE/vtfast]` — i.e. `execute_invokevirtual_vtable_fast`, one of
the exact functions with this gap, dispatching on a receiver
(`this.root.map`) obtained via two chained `getfield`s immediately before
the call. Fixed by forwarding the receiver in all 4 sites. Verified
harmless (`TestAlter`, `TestShell`, `TestLinkedTable` — clean) and a clean
release build.

**Also confirmed NOT sufficient to close `TestUpgrade`** — rebuilt, reran
`--nojit`, identical `NoSuchMethodError` still reproduces. This is a
meaningful negative result, not just another miss: it positively rules out
the entire "stale from-space address" theory as the mechanism, for BOTH
the argument-popping window (addendum #1) and the receiver-peek window
(this addendum). The wrong `RootReference`/`MVMap` object reaching
`compareAndSetRoot` is not a dangling/moved pointer being misread — it is
a **genuinely live, correctly-allocated object of the wrong class**
already sitting in the field/slot the interpreter reads. Every GC-timing
hypothesis this doc's history has proposed (this session's addenda, and
the sixth pass's cross-thread-race and stale-argument theories) is now
either fixed-and-ruled-out or directly refuted. The bug is a **logic**
error in which object ends up written where, not a **memory-safety**
error in how an already-correct object reference is read.

**Suggested different approach for whoever continues**: printf-style
tracing keeps requiring a correct a-priori guess of which single call site
to instrument, and this session burned 4 rounds (`tryUpdate`,
`compareAndSetRoot`, `Page.<init>`, `Page.copy()`) narrowing without
closing. Two structurally different approaches likely to be more
productive from here:
1. **Trace every write to `RootReference.root` / every write to
   `Page.map`** unconditionally (not just at hand-picked call sites) for
   the duration of a single `TestUpgrade` run, keyed by object identity, to
   build a complete provenance chain for the ONE `Page` that ends up with
   the wrong `map` — rather than checking individual call sites one at a
   time on each pass.
2. Actually run `Upgrade.upgrade()`'s migration logic under real HotSpot
   with the same kind of instrumentation (temporarily patched into a local
   H2 build) to see whether a transient old-store `Page` reference is
   EVER legitimately reachable mid-migration on HotSpot too — this would
   distinguish "CratonVM corrupts something H2 never exposes" from "H2
   itself relies on some ordering/timing guarantee CratonVM does not
   provide," which have very different fixes.

`CRATONVM_DBG_LOADER_TRACE=1 --nojit org.h2.test.unit.TestUpgrade`
continues to reproduce in well under 5 minutes.

Fix commits (all three dispatch/GC-forwarding fixes plus the tracing
additions) landed on `dev` via branch `fix/h2-testupgrade-round7-20260723`.
