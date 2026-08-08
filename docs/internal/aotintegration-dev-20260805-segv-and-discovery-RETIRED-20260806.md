# `dev` 2026-08-02→05 breaks `AotIntegrationTests`: what it was, and what it was not

**Status: RETIRED 2026-08-06.** Was
`docs/known-issues/spring/dev-20260805-aotintegration-segv-and-discovery-issues.md`.
Of the three failure modes it reported, none survives; the class's actual
blocker was a fourth thing the doc never saw, and it is fixed. The one live
remainder is a *different* defect with its own page —
[`aotintegration-hangs-after-the-unmodifiable-get-fix-20260806.md`](../known-issues/spring/aotintegration-hangs-after-the-unmodifiable-get-fix.md).

The original doc's proposed next step was to bisect 882 commits starting from
the JIT code-cache retirement path. That would have found nothing: see §2.

## 1. What actually blocked the class

At the 2026-08-06 tip the class failed **both** its tests, deterministically,
3 runs out of 3:

```
RESULT ...AotIntegrationTests found=4 succ=0 fail=2 skip=2
FAILCAUSE ... :: endToEndTests() :: java.lang.ArrayIndexOutOfBoundsException: null
	at ...CompileWithForkedClassLoaderExtension.runTest(CompileWithForkedClassLoaderExtension.java:140)
```

Line 140 is `throw summary.getFailures().get(0).getException();` — the class
rethrowing the **inner** launcher's failure, because
`@CompileWithForkedClassLoader` re-runs each test method inside a nested JUnit
Platform launcher. The real exception never reaches the outer summary intact.

Overlaying a patched `CompileWithForkedClassLoaderExtension` ahead of the jar
on the runner's classpath (`apps/spring-suite-runner` is first on `-cp`, so a
`.class` dropped there wins — see `classpath-overlay-instrumentation-technique`)
and printing the inner summary before the rethrow named it in one run:

```
=> java.lang.IllegalStateException: Unable to parse source file content: ...
       org.springframework.core.test.tools.SourceFile.getClassName(SourceFile.java:188)
     Caused by: java.lang.ArrayIndexOutOfBoundsException
       org.springframework.core.test.tools.SourceFile.getClassName(SourceFile.java:183)
```

`SourceFile.java:182-183`:

```java
Assert.state(javaSource.getClasses().size() == 1, "Source must define a single class");
JavaClass javaClass = javaSource.getClasses().get(0);
```

A list that reports `size() == 1` and then throws from `get(0)` is a VM bug.
QDox's `JavaSource.getClasses()` is a `Collections.unmodifiableList` view over
a **non-ArrayList** backing, and `native_unmod_get` bounds-checked the index
against `al_state`'s size — which is `0` for every backing whose
`elementData`/`size` slots it must not read. Every index was rejected.

Reproduced with no Spring at all in five seconds:

```java
Collections.unmodifiableList(new LinkedList<>(List.of("a"))).get(0)
// CratonVM: ArrayIndexOutOfBoundsException.  HotSpot: "a".
// size()=1, isEmpty()=false and the iterator yields "a" on both.
```

**Fixed on `dev` by `de3c4d35b` / `5d265dbeb`** (a concurrent session, using the
`elementData` DATA slot rather than the size as the readability signal). This
session reached the same defect independently and its work is folded in as the
residual fix and the regression pin below — see
`same-doc-worked-twice-check-dev-before-building`, which this session then
demonstrated the hard way by building a duplicate before re-checking `dev`.

## 2. The three modes the doc reported

All were measured on `cvm-spr4-merged.bin`, built 2026-08-05 16:13. Note that
the `get` defect above landed at 2026-08-05 22:15 (`026ba86c9`) — **six hours
after that binary was built** — so it is not the explanation for any of them.

### 2.1 The SIGSEGV was misread

The doc concluded "a freed buffer's address range was recycled under an
executing frame". Its own evidence says otherwise:

```
#  SIGSEGV at pc=0x70636ff87f41, addr=0x5, pid=2930808
#  fault pc is inside a RECENTLY FREED code buffer: base=0x70636ff86000 len=0x2000
#      active_jit_executions_at_free=0x0
#  fault pc is inside a LIVE registered code buffer: base=0x70636ff87000 cap=0xb740
#  maps: fault pc IS MAPPED - perms are on the `here` line
```

`addr=0x5`, **not** `addr == pc`. That is a *data* fault at address 5 — a
near-null dereference — taken by code executing at a pc the handler itself
reports as inside a **live** registered buffer, mapped `r-xp`. It is not an
instruction-fetch fault, so `jit-code-unmapped-while-executing` does not apply,
and the "RECENTLY FREED" line is an artifact of a retired 0x2000-byte buffer at
`…86000` having overlapped the low page of the larger live one at `…87000`.
`active_jit_executions_at_free=0` is what an ordinary retirement records.

Whatever it was, it was a near-null dereference in **live** compiled code, not
a use-after-free of executable memory — and the code-cache retirement path the
doc named as "the place to start" had already been audited and fixed on
2026-08-03 (`jit-code-buffer-released-outside-retirement-queue-fixed-20260803.md`).
Not reproduced in any run since.

### 2.2 `294 critical discovery issues` / `TestContextAotException`

Not reproduced. Both are downstream of the same two test methods. The
`spring-test` classpath was re-checked at the start of this work (**0 missing of
254**) — see `testcp-pins-jar-paths-that-another-session-evicts`, whose
documented symptom is exactly "a completely plausible failure, not a classpath
error".

What that binary *does* fail with today is a third thing again, fixed since:

```
endToEndTestsForBeanOverrides ->
  java.lang.NoSuchMethodError: java.util.stream.Stream.accept(Ljava/lang/Object;)V
    at org.springframework.aot.hint.ReflectionHints.registerType(ReflectionHints.java:100)
```

### 2.3 The "hang / extreme slowdown" — the one live remainder

Real, still present, and now much cheaper to work on. It has its own page:
[`aotintegration-hangs-after-the-unmodifiable-get-fix-20260806.md`](../known-issues/spring/aotintegration-hangs-after-the-unmodifiable-get-fix.md),
to which this session contributed a per-method oracle and two ruled-out causes.

The doc's own bisect setup is **moot** and should not be resumed: it used the
whole class (769-971 s per good step) as the oracle, and its "good" base
`86a01abf90` does not reproduce today's baseline anyway (§3).

## 3. The doc's baseline no longer holds

`cvm-spr4-gcscan.bin` — the doc's GOOD binary, `dev` @ `86a01abf90` — measured
today on the same host, same fixture, per method:

| method | 08-02 GOOD binary | 08-06 `dev` tip |
|---|---|---|
| `endToEndTests` | **fails** in 76 s | **passes** in 73 s |
| `endToEndTestsForBeanOverrides` | passes in 418 s | hangs |

The 08-02 failure is a resource-loading gap `dev` has since closed:

```
IllegalStateException: Could not detect default properties file for test class
  [...BasicSpringJupiterTests]: class path resource
  [org/springframework/test/context/aot/samples/basic/BasicSpringJupiterTests.properties]
  does not exist.
```

So "`found=4 succ=2` on 08-02" is not a baseline anything can be bisected
against today. Both binaries are still in `/data/data/wt-spr4-20260802/localbin/`.

## 4. Residual fixed here

Letting a valid index through uncovered what the always-throwing pre-check had
been masking: the accessors the view *delegates to* did not bounds-check
either. Measured against HotSpot 25 (`repro/ListItrEndRepro.java`), before:

| call | HotSpot | CratonVM |
|---|---|---|
| `new ArrayList<>(List.of("a","b")).listIterator(2).next()` | `NoSuchElementException` | **`null`** |
| `...listIterator(0).previous()` | `NoSuchElementException` | **`null`** |
| `foreignList.get(size())` (any `AbstractSequentialList`) | `IndexOutOfBoundsException` | **`null`** |
| `cow.get(2)` / `get(9)` / `get(-1)` | `ArrayIndexOutOfBoundsException` | **`null`** |
| `cow.set(9, "z")` | `ArrayIndexOutOfBoundsException` | **`null`** |
| `cow.remove(9)` | `ArrayIndexOutOfBoundsException` | **`null`** |
| `cow.add(9, "z")` on a 2-element list | `IndexOutOfBoundsException` | **returns, list becomes `[a, b, z]`** |

The `AbstractSequentialList` row is the load-bearing one: that class's `get` is
`listIterator(index).next()` inside `catch (NoSuchElementException) ->
IndexOutOfBoundsException`, so swallowing the exception broke `get(size())` for
**every** user list that inherits `get`, directly and through any unmodifiable
view. `cow.add(9, …)` silently appending is the ByteBuffer shape again — worse
than a missing exception, because a computation consumes the result.

`ListItrEndRepro` is now byte-identical to HotSpot. Details:
[`list-out-of-range-accessors-returned-null-FIXED-20260806.md`](list-out-of-range-accessors-returned-null-FIXED-20260806.md).

## 5. Acceptance

Host Azure `20.83.144.174`, real JDK 25, repaired classpath (0 of 254 missing).

| | |
|---|---|
| `AotIntegrationTests#endToEndTests` | **`found=1 succ=1 fail=0`** (HotSpot agrees), 73-76 s, 3/3 runs |
| `AotIntegrationTests#endToEndTestsForBeanOverrides` | hangs — the remaining defect, see its own page |
| `repro/ListItrEndRepro.java` | byte-identical to HotSpot |
| `repro/DirectGetRepro.java` | identical except two pre-existing rows (the `Vector` OOB exception *class*, and a `getClass()` display name) |
| `vm/tests/unmodifiable_list_get_nonarraylist_backing.rs` | passes; FAILS on the pre-fix binary (5 of 7 non-empty backings) and on `dev` before §4 (3 rows), so it is not vacuous |
| `cargo test -p cratonvm-native-collections --lib` | 105/105 |
| `cargo test -p cratonvm-native-builtins --lib` | 3303/3303 |
| `cargo test -p cratonvm-vm --lib` | 2443 pass, 4 fail — **the same 4 fail on pristine `origin/dev`** (`runtime_error_array_index_carries_index`, `hot_files_have_no_production_panics`, `no_unallowlisted_metadata_table_bypass_exists`, `the_allowlist_has_no_dead_rows`) |

## 6. Lessons

* **`addr` is half the crash report.** `pc == addr` is an instruction fetch;
  `addr=0x5` with a `pc` in mapped `r-x` is a near-null data read. The original
  doc quoted both numbers and reasoned only from the `pc`, which pointed the
  next step at the one subsystem that had just been audited and fixed.
* **A sentinel that shares a value with a real answer will be read as the real
  answer.** `al_state` returning `(None, 0)` for "empty" and for "not my
  layout" is the whole of §1; the `None` was there to be checked and the call
  site checked only the `0`.
* **A nested launcher eats the exception you need.** One overlaid class turned
  an unreadable `AIOOBE: null` into a named root cause in a single run.
* **Fixing a masked bug uncovers what it masked.** §4 existed all along; the
  always-throwing pre-check hid it, and the negative half of the new probe is
  what caught it. A regression test that only asserts the reads that must
  succeed would have passed over all seven rows.
* **Re-check `dev` before building.** The `get` fix had already landed there
  while this session was building its own; only the docs and the residual
  survived. `same-doc-worked-twice-check-dev-before-building`.
* **A shared box forges every verdict.** Four consecutive acceptance runs died
  to host conditions — two OOM-kills (`rc=137`) and two timeouts — none of them
  the VM's doing. Per-method runs (73 s / 418 s) beat the 900 s whole-class
  oracle for exactly this reason; `apps/spring-suite-runner/onem.sh` is that
  runner.
