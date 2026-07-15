## Resolution (2026-07-15)

**Status: FIXED / RETIRED.** The failure was not an `ArrayDeque` depth-count
bug. A cyclic map reached the real `Map.forEach` default body, which built an
entry-set iterator and hashed a self-referential entry before
`JsonValueWriter` could reach its own depth guard. CratonVM now forces the
registered `Map.forEach` bridge and snapshots concrete map entries without
hashing that cyclic value. The same closure found a second masked case:
`Iterable` method-reference lambdas were intercepted by an ArrayList-shaped
interface iterator bridge and silently yielded no elements. `Iterable` now
uses normal lambda dispatch, with the synthetic collection fallback retained.

Validation on the rebuilt release VM (`bin-jsonvalue-nesting-closure-20260715-003.exe`):

- The exact cyclic-map and cyclic-Iterable methods pass with JIT off and on.
- The complete `JsonValueWriterTests` class passes in both modes: 33 tests,
  0 failed, 0 aborted (one expected Windows-disabled test skipped).

## Historical incident

# `JsonValueWriterTests` CRASHED: native `EXCEPTION_STACK_OVERFLOW` — self-referential-collection nesting-depth guard initially appeared not to stop recursion

**Original status: OPEN.** CRITICAL severity — this was a process-fatal native crash
(not a test `FAIL` or Java `StackOverflowError`), 0 tests executed/reported.
Found triaging the `crashfail-20260714` shard run (`core/spring-boot`,
shard2).

## Symptom

`org.springframework.boot.json.JsonValueWriterTests` (`core/spring-boot`)
crashes with a CratonVM fatal-error banner (this run's fatal-error text
landed in the `.out.log` file, not `.err.log`):

```
[2m2026-07-14T13:52:47.152331Z[0m [33m WARN[0m cratonvm_vm::vm::vm_util: Post-clinit fixup: Unsafe ARRAY_*_BASE_OFFSET/INDEX_SCALE populated (18/18)
[2m2026-07-14T13:52:47.176037Z[0m [33m WARN[0m cratonvm_vm::vm::vm_util: Post-clinit fixup: UnsafeConstants populated (5/5)
[2m2026-07-14T13:52:48.755739Z[0m [33m WARN[0m cratonvm_vm::vm::vm_util: Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)

#
# A fatal error has been detected by the CratonVM Runtime Environment:
#
#  EXCEPTION_STACK_OVERFLOW (0xC00000FD) at pc=0x00007FF6CC6D5BEB
#  pid=26664 tid=31032
#  thread: "main-vm"
#  exe module base: 0x00007FF6CB870000
#  faulting RVA: 0xE65BEB
#
Native frames (most recent call first) [raw]:
#
Registers:
  rax=0x00007FF6CCEBC298  rcx=0x00000000211A5708  rdx=0x0000000044AC78D0  rbx=0x00000000211A5708
  rsp=0x0000000044AC6FF0  rbp=0x0000000044AC7070  rsi=0x00007FF6CCEBC298  rdi=0x0000000000000003
   r8=0x00000000211A5708   r9=0x00007FF6CCEBBE82  r10=0x00000000813807C0  r11=0x0000000000000004
  r12=0x0000000044AC78D0  r13=0x000000001F57F010  r14=0x00007FF6CC6D5B10  r15=0x0000000000000008
  rip=0x00007FF6CC6D5BEB
Memory around R10 (0x00000000813807C0):
    [R10+0xfffffffffffffff0] = 0x0000000000000070
    [R10+0xfffffffffffffff8] = 0x0000000000000000
    [R10+0x0] = 0x0000000081380820
    [R10+0x8] = 0x614D687361482F6C
    [R10+0x10] = 0x2470614D68736170
    [R10+0x18] = 0x0000000065646F4E
ShadowStack @ R10+0x1B8: top=0x0000000000000000 end=0x00000000813809C0 base=0x64656B6E694C2F6C
Frame slots around RBP (0x0000000044AC7070):
    [RBP+0xffffffffffffffc0] = 0x0000000000000000
    ... (all zero) ...
#
# Symbolize offline with the SAME binary:
#   CRATONVM_SYMBOLIZE=<comma-separated exe+0x RVAs> cratonvm X
Native frames [symbolized, best-effort]:

thread 'main-vm' (31032) has overflowed its stack
```

(`.err.log` for this class is empty.) `results.tsv`: `rc=-1073741571`
(`0xC00000FD` as a signed 32-bit value — matches the banner exactly),
`status=CRASH`, `tests=0 failed=0 aborted=0 skipped=0`, elapsed `88.868s`.
This is a genuine Windows SEH `EXCEPTION_STACK_OVERFLOW` (native OS thread
stack exhaustion), not a caught/reported Java `StackOverflowError` and not
a Rust panic — no Rust backtrace/panic message is present, consistent with
the native stack itself being gone by the time the handler ran (only raw
frame addresses, no symbolized frames, and the "Native frames [raw]" list
is empty — there was no room left to walk it).

The bytes near the faulting frame's `R10` register decode as ASCII
fragments of the string `java/util/HashMap$Node` (`l/HashMap`, `ashMap$`,
`Node`, little-endian). This is very likely leftover/adjacent stack
content from ordinary class-metadata bookkeeping (`HashMap` is used
pervasively by JDK bootstrap/classloading code) rather than a direct clue
about the recursion source itself — `JsonValueWriter`'s own recursive
write path (see below) goes through `ArrayList`/`ArrayDeque`, not
`HashMap`. Treated as incidental, not diagnostic.

## Analysis — leading hypothesis: the nesting-depth guard isn't bounding recursion

`JsonValueWriterTests` (`core/spring-boot/src/test/java/org/springframework/boot/json/JsonValueWriterTests.java`)
contains three tests that deliberately build **self-referential (circular)**
collections and rely on `JsonValueWriter`'s own nesting-depth guard to
convert the circularity into a clean, catchable `IllegalStateException`
rather than infinite recursion:

```java
@Test
void illegalStateExceptionShouldBeThrownWhenCollectionExceededNestingDepth() {
    JsonValueWriter writer = new JsonValueWriter(new StringBuilder(), 128);
    List<Object> list = new ArrayList<>();
    list.add(list);                                   // list contains itself
    assertThatIllegalStateException().isThrownBy(() -> writer.write(list))
        .withMessageStartingWith(
            "JSON nesting depth (129) exceeds maximum depth of 128 (current path: [0][0][0]...");
}
// + illegalStateExceptionShouldBeThrownWhenMapExceededNestingDepth (map.put("foo", Map.of("bar", map)))
// + illegalStateExceptionShouldBeThrownWhenIterableExceededNestingDepth (same self-referential list, via Iterable)
```

`JsonValueWriter`'s production code
(`core/spring-boot/src/main/java/org/springframework/boot/json/JsonValueWriter.java`)
is mutually recursive across `write` → `writeArray`/`writeObject` →
`writeElement`/`writePair` → `write` (and back), and the *only* thing
stopping it from recursing forever on a circular structure like the one
above is `start(Series)`'s explicit depth check:

```java
void start(@Nullable Series series) {
    if (series != null) {
        int nestingDepth = this.activeSeries.size();
        Assert.state(nestingDepth <= this.maxNestingDepth,
                () -> "JSON nesting depth (%s) exceeds maximum depth of %s (current path: %s)"
                    .formatted(nestingDepth, this.maxNestingDepth, this.path));
        this.activeSeries.push(new ActiveSeries(series));
        append(series.openChar);
    }
}
```

On a correct JVM, recursing 128-129 Java frames deep is trivial (HotSpot's
default thread stack handles thousands of frames without difficulty) — the
test is designed to hit the `Assert.state` failure at depth 129, not to
stress the native call stack. For this to escalate all the way to a
**native OS stack overflow** on CratonVM, one of two things must be true:

1. **The depth guard isn't actually bounding the recursion** — e.g. if
   `this.activeSeries.size()` (an `ArrayDeque<ActiveSeries>`, a private
   inner-class element type) doesn't return the real/incrementing count on
   this build for some collection-dispatch reason, `nestingDepth` would
   stay wrong/stuck below `maxNestingDepth` forever and the self-referential
   list/map would recurse without bound until the *native* stack (not a
   Java-level counter) is exhausted — which is exactly the observed
   `EXCEPTION_STACK_OVERFLOW` shape (no Java `StackOverflowError` was ever
   thrown/caught; the process died at the OS level first). This would
   most likely be a collection-dispatch regression (see below for the
   same-day timing correlation), not a `JsonValueWriter` logic bug — the
   Java-level logic is straightforward and correct.
2. Alternatively, the guard fires correctly and *does* throw at ~129
   frames, but each mutually-recursive Java call in this specific call
   chain (`write`/`writeArray`/`writeElement`/`start`/`Assert.state`'s
   lambda) consumes an unusually large amount of *native* stack per frame
   in CratonVM's interpreter/JIT — enough that ~129 levels alone exhausts
   the default thread stack before the `Assert.state` check is even
   reached at the top of that 129th frame. This is less likely to explain
   129 levels specifically (that's a very shallow depth for any reasonable
   per-frame native stack budget), but can't be ruled out without a
   symbolized native stack.

Given the two `Series`-nested boundary tests in the same file pass at
depth exactly `maxNestingDepth` (`shouldAllowStartingObjectWhenCurrentDepthIsMaxDepth`,
depth 2 — trivially shallow, uninformative either way) and the crashing
class has **zero tests reported** (unlike a targeted per-test failure),
hypothesis (1) is the leading candidate: something in the guard's
`Deque.size()`/`push()` bookkeeping — or, more broadly, some interaction
between this recursive call shape and this build's recently-changed
native-dispatch/JIT fast paths — is not stopping the circular-structure
recursion where the Java-level logic says it should.

**Same-day timing note (unconfirmed, not a specific accusation):** this
worktree's HEAD (`1021533f9`) sits directly on `77f8b37e5` ("perf(vm):
HashMap/Integer native-dispatch fast paths") and `0063b0b0e` ("perf(jit):
adjacent store-load reload elision + pure-kernel GPR local homes"), both
landed the same day as this crashfail run and both touch interpreter/JIT
call-frame and local-variable-slot handling broadly (not `ArrayDeque`
specifically). No direct code-level link from either diff to
`ArrayDeque`/`Deque.size()` was found by inspection; this is flagged as a
timing correlation worth checking during bisection, not a confirmed cause.

## What's missing (could not be done in this session)

The `apps\spring-boot` Spring Boot checkout is **not present in this
worktree** — only the suite-runner scaffolding and prior results were
carried into this worktree (see the collection-binder-tests companion doc
in this same directory for the same caveat). Could not, in this session:
- Isolate the run to just the three self-referential-collection tests to
  confirm they are the trigger (vs. some other test in the 340-line file).
- Run with `-Jit off` to check whether the overflow persists
  interpreter-only (would rule JIT frame size in/out, and by extension
  rule out `0063b0b0e`'s interpreter local-slot change).
- Add temporary instrumentation to print `this.activeSeries.size()` /
  confirm whether `start()`'s `Assert.state` check is being reached at all
  before the crash (would directly confirm or refute hypothesis 1).
- Bisect the binary against a pre-`77f8b37e5`/`0063b0b0e` build.

This doc records the log evidence, the source-level trigger candidate
(the three self-referential-collection tests), and the leading hypothesis
only — the actual defective code path has **not** been located.

## Repro

Restore the checkout (not present in this worktree) if needed:

```powershell
cd apps\spring-boot
git config core.longpaths true
git checkout HEAD -- settings.gradle gradle core module starter cli test-support `
  configuration-metadata loader config platform documentation smoke-test integration-test system-test
```

Then:

```powershell
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 -SpringBootRoot <repo>\apps\spring-boot `
  -ClassList <TSV row: core/spring-boot	org.springframework.boot.json.JsonValueWriterTests> `
  -Start 1 -Count 1 -Exe target\release\cratonvm-spring-boot-suite.exe
```

Recommended follow-ups: `-Jit off` bisection; a minimal standalone repro
using just `illegalStateExceptionShouldBeThrownWhenCollectionExceededNestingDepth`'s
body (construct a self-referential `ArrayList`, call
`JsonValueWriter(new StringBuilder(), 128).write(list)`, confirm whether it
throws `IllegalStateException` at depth 129 as expected or overflows the
native stack); `CRATONVM_SYMBOLIZE=<RVAs from a repro run>` against the
same binary to get a symbolized native stack once one is captured with the
matching `.pdb` available (per this codebase's standing rule: never
diagnose an unsymbolicated crash stack without a matching `.pdb`).

## Related

- No existing doc in `docs/known-issues/springboot/` or `docs/internal/`
  covers a nesting-depth/self-referential-collection stack overflow in
  `JsonValueWriter` or elsewhere — this is a new report, not a
  rediscovery of a fixed issue.
- `docs/internal/hashmap-native-dispatch-overhead.md` — the retired/fixed
  HashMap dispatch-overhead baseline that `77f8b37e5` (flagged above as an
  unconfirmed timing correlation) builds on top of.
- [`collectionbindertests-classcast-testdescriptor-crash.md`](collectionbindertests-classcast-testdescriptor-crash.md) —
  the other CRASH found in this same `crashfail-20260714` run; unrelated
  symptom (a `ClassCastException` vs. a native stack overflow) but same
  same-day-build timing note about `77f8b37e5`/`0063b0b0e` flagged as an
  unconfirmed candidate in both docs.
