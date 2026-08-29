# The definition of done, run on the three real workloads — two defects stood between it and green, both mode-independent

**Status: MEASURED and FIXED 2026-08-28** on `azure-host-2`
(`azureuser@20.80.105.49`, hostname `vm1`), worktree `/data/cvm-l7dod-20260828`
branched from `origin/dev` at `fee780fc1`. Oracle: HotSpot
`/data/jdkimages/jdk25-linux/jdk-25.0.4+7`.

This is Phase 4 / the L7 lane of `HANDOFF-20260828-SCOPE.md`: not a row count,
the goal itself.

> `docs/feature-designs/jdk-only-completion-roadmap.md` §6 — **Definition of
> done.** Not a suite number. A Spring Boot application, a servlet container
> serving HTTPS, and a JDBC workload each run to completion under `--jdk-only`
> with **no fabricated class instantiated, whatever its package** — screened
> against the refused-class set the VM reports, not against a prefix.

---

## 1. The blocker was a host, not an absence

The lane doc opens by saying none of the three workloads is checked out, and
names `azure-host-2` as the place to check first. It was right to say so:
**all three are there, and built.**

```text
apps/h2database/h2          target/classes 1114 .class   target/test-classes 688
apps/tomcat                 output/testclasses 2801 .class, output/build CATALINA_BASE
apps/spring-boot/smoke-test 108 modules, each with build/cratonvm-test-cp.txt
```

So the honest form of that blocker was **"not on the Windows box"**, and the
timing-comparability warning it carries is the reason it read as absence: the
five-probe screen of
`the-definition-of-done-screen-run-for-the-first-time-20260828.md` was run
where the workloads are not.

`apps/spring-boot/smoke-test/spring-boot-smoke-test-tomcat-ssl` is worth naming
on its own: it is a Spring Boot application whose embedded Tomcat serves HTTPS
from a JKS bundle. It answers two of the three DoD workloads at once, which is
why it exists as its own arm below rather than being folded into either.

## 2. The five arms

One `cratonvm` process each, each writing its own `--jdk-only-report`. Runner:
`probes/dod-arms.sh` over `probes/dodscreen-linux.sh`.

| arm | what it is | scale |
| --- | --- | --- |
| `sbsimple` | a Spring Boot application: full context refresh, auto-configuration, classpath scanning, CGLIB `@Configuration` proxies, logback | 55 beans |
| `tcssl` | a servlet container serving HTTPS: Spring Boot on embedded Tomcat with a JKS bundle, three real HTTPS requests, clean context close | 297 beans, 3 requests |
| `tcnetssl` | the same requirement without Spring in the way: Apache Tomcat's own `org.apache.tomcat.util.net.TestSsl` | 21 tests |
| `jdbc` | a JDBC workload written to the `java.sql` contract, in-memory and file-backed | 92 checks |
| `h2jdbc` | the engine's own JDBC conformance classes | 12 classes |

Every driver **returns normally from `main`**. That is not tidiness: the report
is not written on the `System.exit` path, so `JUnitCore.main` and
`SpringApplication.exit` each produce a run with no file at all — which reads
exactly like a clean one. `probes/dodscreen-linux.sh` prints
`NO-REPORT-WRITTEN` rather than letting that pass.

### The three arms are three, and the fourth trap is new

`HANDOFF-20260828-L7` §3 lists three instrument traps and all three are still
live. A fourth cost a run here: **`--Xmx` takes its size as a SEPARATE
argument** on this VM. `--Xmx2g` is rejected during argument parsing with
`error: unexpected argument '--Xmx2g'`, exit 2, no report — indistinguishable
from the `System.exit` case if you only look for the file.

## 3. What the first run said

Both `cratonvm` modes against HotSpot, same binary, same host:

```text
arm        hotspot        compat         strict (--jdk-only)
sbsimple   OK             OK             FAIL   IllegalStateException in LogbackLoggingSystem
tcssl      OK             OK             FAIL   same
tcnetssl   21/21          20/21          20/21  the renegotiation row, both modes
jdbc       92 checks OK   92 checks OK   FAIL   ArrayStoreException in H2 SortOrder.sort
h2jdbc     12/12          12/12          12/12
```

Two `--jdk-only`-only failures, and **both turned out to be mode-independent
VM defects that compatible mode was hiding behind a synthetic stub.** That is
the campaign's recurring shape read from the other side: not "strict is more
correct than the default" this time, but "strict is what makes the default's
defect reachable".

`tcnetssl`'s single failure is `testClientInitiatedRenegotiation[JSSE]` in
**both** modes. It is a recorded, accepted design limit — rustls implements no
TLS 1.2 renegotiation, and firing `HandshakeCompletedEvent` for a
`startHandshake()` on an established connection would tell the application key
material had been derived when none had. See
`known-issues/tomcat/ssl-renegotiation-emulation-limits.md`. **It is not a
`--jdk-only` defect and it is not this lane's to fix.** Twenty of the
twenty-one tests — the ones that actually start the container, serve servlets
over TLS and drive client certificates — pass under `--jdk-only`.

## 4. Defect 1 — a `-cp` jar's `module-info` named its classes' module, and `ServiceLoader` skipped every one of them SILENTLY

### The symptom

```text
java.lang.IllegalStateException: LoggerFactory is not a Logback LoggerContext but
Logback is on the classpath ... (class org.slf4j.helpers.NOPLoggerFactory ...)
    at org.springframework.boot.logging.logback.LogbackLoggingSystem.beforeInitialize
    at org.springframework.boot.SpringApplication.run
```

Every Spring Boot application dies before its context exists.

### The rung, found by walking them

`probes/DodServiceLoaderSweep` walks the chain SLF4J walks. Under `--jdk-only`
every `java/util/ServiceLoader` native is a refused `SyntheticStub`
(`service_loader.rs:3708`–`3751`, `streams.rs:74`), so the **real JDK
`ServiceLoader` bytecode runs**. Measured, strict vs HotSpot:

```text
R1 provider class loads                      identical
R2 getResources(META-INF/services/...)  = 1  identical  (app, system, tccl, bootstrap)
R2 openStream / openConnection first line    identical
R3 ServiceLoader.load(...).iterator()   HotSpot 1     strict 0     <- the rung
R4 LoggerFactory.getILoggerFactory()    HotSpot LoggerContext   strict NOPLoggerFactory
```

Resources enumerate. The provider class loads, is assignable, and instantiates.
`ServiceLoader` still reports nothing, and reports it without an error.

`ServiceLoader.LazyClassPathLookupIterator.hasNextService()` has exactly one
rung that drops a provider with no exception and no report row:

```java
if (clazz.getModule().isNamed()) {
    // ignore class if in named module
    continue;
}
```

### The cause

```text
ch.qos.logback.classic.spi.LogbackServiceProvider.getModule()
  HotSpot   isNamed=false  name=null
  CratonVM  isNamed=true   name=ch.qos.logback.classic      (BOTH modes)
org.slf4j.spi.SLF4JServiceProvider.getModule()
  HotSpot   isNamed=false  name=null
  CratonVM  isNamed=true   name=org.slf4j                   (BOTH modes)
```

A real JVM ignores a class-path jar's `module-info` outright: every class it
defines belongs to the **unnamed module of its loader**. This VM's module
registry scans the application class path for `module-info.class` and registers
what it finds — deliberately, so readability checks pass for jars like
`org.jboss.logging` (`E4-R11-CLASS-PATH-MODULE-BOOT-LAYER-FIX-20260813.md`) —
and `Class.getModule()` was reporting the scanned name.

**The predicate for this already existed and two other doors already applied
it.** `NativeContext::module_is_class_path_only` is exactly "this module
reached the registry only by scanning the application class path";
`populate_boot_layer_modules` uses it, and `ModuleRegistry::providers_for_service`
uses it — whose own doc comment says *"the two doors onto the module source
were fixed a day apart and only the boot-layer one got the module-shaped
predicate"*, and then names `service_loader.rs`'s arm as a third.
**`Class.getModule()` is the fourth**, and it is the one the JDK's own
`ServiceLoader` reads.

### The fix

`native-builtins/src/lib.rs` `Class.getModule()` — filter the resolved module
name through `module_is_class_path_only`, which routes the class into the
existing per-loader unnamed-module branch. The same edit in
`native-builtins/src/phases_late/reflect_invoke.rs`, the `--synthetic-jdk` twin:
the two are a recorded pair in `phases_late.rs`'s `P59_AND_ESSENTIAL` ratchet
table, and a duplicate sitting half-fixed is the shape that survives until
registration order changes.

A `--module-path` module is re-registered `automatic = false` by `vm_init`
immediately after `ClassManager::new`, so it is not class-path-only and is
untouched — which is what `regression-suite/src/RJdkModule.java` pins, and it
stays green.

## 5. Defect 2 — `Arrays.copyOf(U[], int, Class)` threw a FALSE `ArrayStoreException` whenever the destination component was itself an array

### The symptom

```text
org.h2.jdbc.JdbcSQLNonTransientException: General error:
  "java.lang.ArrayStoreException: org.h2.value.Value"
SELECT ... GROUP BY c.ID, c.NAME ORDER BY T DESC, c.NAME LIMIT 3
Caused by: java.lang.ArrayStoreException: org.h2.value.Value
    at java.util.ArrayList.toArray(ArrayList.java:401)
    at org.h2.result.SortOrder.sort(SortOrder.java:220)
```

Every `ORDER BY` in H2 under `--jdk-only`. `SortOrder.sort` does
`rows.toArray(new Value[0][])` on a list of `Value[]`.

### The cause, shrunk

`probes/DodArrayStoreSweep`, strict vs HotSpot:

```text
Arrays.copyOf(Object[]{String[]}, 2, String[][].class)   HotSpot ok   CratonVM ASE: java.lang.String
Arrays.copyOf(String[][],        2, Object[][].class)    HotSpot ok   CratonVM ASE: java.lang.String
ArrayList<String[]>.toArray(new String[0][])             HotSpot ok   CratonVM ASE: java.lang.String
ArrayList<String[]>.toArray(new String[2][])             identical    (arraycopy route, not copyOf)
aastore String[] into String[][]                         identical
System.arraycopy(Object[]{String[]} -> String[][])       identical
Class.getComponentType / Array.newInstance, 16 rows      identical
```

So neither rung the real `Arrays.copyOf` bytecode stands on is wrong, and the
`aastore` opcode is right. The failing site is the **native** at
`native-builtins/src/lib.rs`, and it fails in both modes:

```rust
let actual = ctx.class_id_of_object(v);
if actual != cid && !ctx.is_subclass(actual, cid) { /* ArrayStoreException */ }
```

On a reference array the heap header's class id holds the **component** class.
So a `String[]` element answers `java/lang/String`, while the destination
component of a `String[][]` is `[Ljava/lang/String;`. They can never be equal,
and `is_subclass` cannot bridge them.

**Why compatible mode never saw it.** `ArrayList.toArray([Ljava/lang/Object;)`
is a `SyntheticStub` (`native-collections/src/lib.rs:5110`) that copies without
consulting `Arrays.copyOf` at all. Strict refuses the stub, the real
`ArrayList` bytecode runs, and the broken native is reached for the first time.
A `--jdk-only` arm exposed a defect that had been sitting in the default mode.

### The fix

Both hand-rolled ladders now ask `NativeContext::aastore_element_assignable` —
the VM's own predicate, already shared by the `aastore` opcode, `jit_aastore`
and `java.lang.reflect.Array.set`. It is deliberately *additive* (fails open for
interface components, `$Proxy` values, synthetic class ids, cross-loader
same-named components), so adopting it cannot manufacture a false throw. `None`
— a mock with no class hierarchy — leaves each caller on its old ladder.

The message is HotSpot's, measured rather than invented: the real
`Arrays.copyOf` reaches its exception through the `System.arraycopy` inside its
own bytecode, so both sites print

```text
arraycopy: element type mismatch: can not cast one of the elements of
java.lang.Object[] to the type of the destination array, java.lang.String
```

and the renderer had to learn HotSpot's two dialects in one sentence: the
source is an external name (`java.lang.Object[]`), the destination component is
the component class's dotted NAME, which for an array class is a descriptor
(`[Ljava.lang.Integer;`, `[I`).

## 6. Two more, found by asking the other polarity

A sweep that only asks "does the legal store succeed?" would have reported the
family clean after §5. Both of these are missing `ArrayStoreException`s — the
direction a green probe cannot see — and both are mode-independent.

**`System.arraycopy` accepted any array into any array-of-array.** The ladder's
last hedge was a blanket: `if the element is an array and the destination
component name starts with '[' → allow`.

```text
System.arraycopy(new Object[]{new String[]{"s"}}, 0, new Integer[1][], 0, 1)
  HotSpot   ArrayStoreException      CratonVM  copied
```

Fixed by routing array-typed elements through the same `aastore` predicate. The
case the blanket existed for — a primitive `int[]` element, which carries the
synthetic `ClassId(0)` — still copies, and `long[]` into `int[][]` now throws.

**The `aastore` predicate itself failed open on a plain object into an array
component.**

```text
Arrays.copyOf(new Object[]{Integer.valueOf(1)}, 1, String[][].class)
  HotSpot   ArrayStoreException      CratonVM  a String[][] holding an Integer
```

That fail-open was a hedge against "imprecise component info", but this arm
needs no component information to be sound: the only instances of an array type
are arrays (JLS §10.7), and the branch above it has already established that
the heap does not call this value an array. It is the one narrowing in that
function that cannot produce a false positive, and it reaches all four
callers at once.

## 7. The result

**All five arms run to completion under `--jdk-only`, and the DoD predicate
holds by row on every one of them.** `probes/dod-summary.py` over the five
reports; nothing here filters on a class-name prefix.

```text
arm        mode      compatibility_classes  synthetic_stub_invocations  fabrication requests  result
sbsimple   hotspot            -                        -                        -             OK
sbsimple   compat            19                    45 780                      19             OK
sbsimple   strict             0                         0                       5             OK
tcssl      hotspot            -                        -                        -             OK   3/3 HTTPS
tcssl      compat            23                   160 419                      23             OK   3/3 HTTPS
tcssl      strict             0                         0                       8             OK   3/3 HTTPS
tcnetssl   hotspot            -                        -                        -             21/21
tcnetssl   compat            20                   741 437                      20             20/21
tcnetssl   strict             0                         0                       7             20/21
jdbc       hotspot            -                        -                        -             92/92 checks
jdbc       compat            16                    64 392                      16             92/92 checks
jdbc       strict             0                         0                       3             92/92 checks
h2jdbc     hotspot            -                        -                        -             12/12 classes
h2jdbc     compat            20                 1 880 581                      20             12/12 classes
h2jdbc     strict             0                         0                       5             12/12 classes
```

**`compatibility_classes: 0` and `synthetic_stub_invocations: 0` on all five
strict arms.** That is the roadmap's predicate, on the three named workloads,
and it is met.

The compatible-mode column is not filler. It is the same programs on the same
binary **instantiating 16 to 23 fabricated classes each and taking up to 1.9
million synthetic-stub calls** — so the zeros are `--jdk-only` doing its job,
not a property of the workloads.

Each report is complete rather than a floor, and that is checked rather than
assumed: `partial: false`, and `observation_sink` reports
`truncated: false, saturated: false, dropped: 0` on every one, with 540–1190
rows recorded against a 4096 cap. A truncated `violations[]` is byte-identical
in shape to a complete one, which is why `probes/dod-report.py` prints the sink
block before anything else.

For Phase 2's benefit the same reports carry the shadow rows split by who
actually won the dispatch:

```text
arm        native-won   bytecode-won
sbsimple      560           252
tcssl         844           354
tcnetssl      731           321
jdbc          352           193
h2jdbc        515           277
```

The class counts (`compatibility_classes`, the fabrication-request set) are
identical across repeats; the INVOCATION counts are not, and the shadow rows
move by a handful. Two runs of the same binary on this host differ by ~0.3% on
`synthetic_stub_invocations` and by 1–3 shadow rows, because both depend on how
far the JIT got. Quote the class counts; treat the rest as a scale.


### The fabrication requests, named

Nine distinct classes across the five arms, every one with the `file:line`
that asked for it. **Six of the nine do not match `cratonvm/internal/`** — the
roadmap's prefix clause, demonstrated on the real workloads rather than argued
from the Phase 1 list:

| class | requester | arms (strict) |
| --- | --- | --- |
| `cratonvm/internal/BufferPool` | `native-builtins/src/shared_secrets_bridge.rs:2818` | tcssl |
| `cratonvm/internal/SystemLogger` | `native-builtins/src/lib.rs:27939` | tcssl, tcnetssl, h2jdbc |
| `cratonvm/internal/foreign/MemorySegmentImpl` | `native-builtins/src/panama.rs:191` | tcnetssl |
| `cratonvm/stream/LazyOp` | `native-collections/src/lib.rs:25760` | all five |
| `java/util/ArrayDeque$Itr` | `native-collections/src/lib.rs:44681` | sbsimple, tcssl |
| `java/util/Enumeration$Impl` | `native-builtins/src/classloader.rs:5402` and `:5430` | all five, two distinct call sites |
| `java/util/HashMap$KeyItr` | `native-collections/src/lib.rs:58700` | all five |
| `java/util/IteratorEnumeration` | `native-builtins/src/keystore.rs:3106` | tcssl, tcnetssl |
| `java/util/TreeSet$Itr` | `native-collections/src/lib.rs:53586` | sbsimple, tcssl, tcnetssl, h2jdbc |

**A request is not a failure, and the recovery is shown by the workload's own
result rather than asserted.** Every arm carrying these requests completes:
`sbsimple` brings up a 55-bean context with five of them, `tcssl` serves three
HTTPS requests and closes cleanly with eight, `jdbc` passes 92 of 92 checks with
three, `h2jdbc` passes all twelve conformance classes with five. The native
asks, is refused, and the caller lands on real JDK bytecode.

`tcnetssl` is the one arm with a failing row, and it is not one of these seven:
it is `testClientInitiatedRenegotiation[JSSE]`, red in BOTH modes — including
compatible mode, where every one of those classes IS fabricated. See §8.

**One request recovers into a wrong ANSWER rather than a wrong outcome, and that
is a weaker claim than the rest of this table.**
`cratonvm/internal/foreign/MemorySegmentImpl` at `panama.rs:191` falls back to
allocating an object whose class is `java/lang/foreign/MemorySegment` — the
INTERFACE. `tcnetssl` completes with the request in its report, so it does not
block the workload; but an instance whose class is an interface is a thing the
Java object model does not contain. That is
`the-definition-of-done-screen-run-for-the-first-time-20260828.md` §4's finding,
unchanged, and it is `panama.rs`'s own piece of work.


## 8. Residuals — measured, and why each was not fixed here

> **FOLLOWED UP 2026-08-29: three of these four reasons did not survive contact,
> and two of the residuals are now closed.** R2 turned out to be a family of
> THIRTY classes with a second, worse defect underneath it, and R3 was closed by
> retiring the shadow rather than improving it. R1 is confirmed WILL-NOT-FIX
> with the reason verified rather than repeated, and R4 is measured properly —
> seven sites, not one — and left as a lane with a standing assertion.
> `the-four-residuals-two-closed-one-was-a-family-of-thirty-and-one-is-a-lane-20260829.md`.
> Read that page for the current status of each; what follows is what was known
> on the 28th.

Four, each measured, three of them mode-independent and none of them a
`--jdk-only` blocker.

**R1 — `TestSsl.testClientInitiatedRenegotiation[JSSE]`, red in BOTH modes.**
The listener added by `addHandshakeCompletedListener` never fires after a
client-initiated `startHandshake()` on an established connection. **Not fixed,
and it should not be:** rustls implements no TLS 1.2 renegotiation, and firing
`HandshakeCompletedEvent` there would tell the application fresh key material
had been derived when none had. Recorded, with its history and the sibling
`testClientCertPostZero` row, in
`known-issues/tomcat/ssl-renegotiation-emulation-limits.md`. The other twenty
tests — the ones that start the container, serve servlets over TLS and drive
client certificates — pass under `--jdk-only`.

**R2 — compatible mode's `ArrayList.toArray(T[])` performs no store check.**

```text
a List<Object> holding an Integer, toArray(new String[0])
  HotSpot   ArrayStoreException
  strict    ArrayStoreException, in HotSpot's own text   <- correct after §5
  compat    a String[] holding an Integer
```

Site: `native_al_to_array_typed`, `native-collections/src/lib.rs`. **Not fixed
here, and the reason is not scope.** The text this must print is HotSpot's, and
its renderer (`element_type_mismatch_message` / `external_class_name`) lives in
`native-builtins`, which depends on `native-collections` and not the reverse —
so the honest fix moves the renderer down to `native-api` first. Copying it
instead leaves a correct twin beside the original, which is the shape where one
of the two later gets fixed alone. It is a compatible-mode-only wrong answer
with strict already right, so it is worth doing properly rather than quickly.

**R3 — compatible mode's `ServiceLoader.iterator()` returns the wrong iterator
type.**

```text
ServiceLoader.load(SLF4JServiceProvider.class, cl).iterator().getClass()
  HotSpot   java.util.ServiceLoader$2
  strict    java.util.ServiceLoader$2        <- the real bytecode, after §4
  compat    java.util.ArrayList$Itr
```

The provider SET is identical in all three; only the iterator's identity
differs, because compatible mode's `ServiceLoader.iterator` native materialises
the providers into a list and hands back its iterator. It is visible to anything
that reasons about the iterator's class, and it means compatible mode does not
have the lazy iterator's semantics either — a `ServiceConfigurationError` is
raised at `load` rather than at the offending provider. Strict mode is right
because the stub is refused there. Same lane as R2.

**R4 — `cratonvm/internal/foreign/MemorySegmentImpl`.** §7's table entry: the
refusal fallback hands back an instance whose class is an interface. Unchanged
from the earlier screen, closest to L1, and it blocks no workload.


## 9. Gates and the three arms

Run on the MERGED tree — `origin/dev` had moved eight commits while this lane
was measuring, and the whole set was re-run after the merge rather than the
quick subset. Release binary, same host. **No red is new.**

```text
cargo test -p cratonvm-types                       589 passed, 0 failed  (see the flake note)
cargo test -p cratonvm-native-builtins  (the seven ratchets)   PASS
  ... --features management (three of them)                    PASS
cargo test -p cratonvm-native-builtins --lib      4177 passed, 0 failed
cargo test -p cratonvm-vm --lib                   2639 passed, 0 failed   <- see below
SUITE=core                bash regression-suite/run.sh          72 / 72
SUITE=all                 bash regression-suite/run.sh         111 / 112
CRATONVM_ARGS=--jdk-only  bash regression-suite/run.sh         111 / 112
```

All fifteen definition-of-done arm runs were re-taken on the merged tree too,
with the same verdicts and the same two compatible-mode probe differences (R2
and R3 below); a merge of two independently-verified halves is not a verified
whole. That is not a formality here — the second merge arrived with a red of its
own, and §10 is it.

`cratonvm-types` failed once, on
`arraylist_view::tests::a_fallback_mint_revokes_the_yield_permanently`, during a
window with seven other sessions' regression suites running on the same host.
It **passes alone and passes in a full clean re-run** — the dial it asserts on
is a process-global latch, so the failure is test interleaving under load, not a
verdict. Checked rather than assumed, and recorded rather than quietly re-run.

**`cratonvm-vm --lib` was red for most of this lane, and the red was never
this branch's. It went green on the last merge, fixed at its source by the lane
that owns the site — which is the outcome the section below argued for.** Kept
because the reasoning is the transferable part.

The two failures were
`runtime::resolve::guard::no_unallowlisted_metadata_table_bypass_exists` and
`the_allowlist_has_no_dead_rows`, red on pristine `origin/dev` and in a file
this branch does not touch.
Both reported

```text
vm/src/runtime/interpreter/invoke.rs
    `find_method_recursive(` appears 4 time(s); the allowlist says 3
```

Verified pre-existing two ways: `git diff --name-only` on this branch lists
four files and `invoke.rs` is not one of them, and `git show
origin/dev:vm/src/runtime/interpreter/invoke.rs | grep -c` is **4** against an
allowlist row of **3** on the same commit. Both tests are pure source scans, so
the count settles it without a build.

**Not fixed here, deliberately — and that turned out to be right.** The fourth
site was `loader_interface_override`
in `invoke.rs` — a documented, loader-accurate override check added for
`SpringBootContextLoaderAotTests`. Raising the row 3 → 4 alone then trips
`the_split_did_not_change_the_interpreter_budget`, whose interpreter total is 29
against an actual 30, and whose own docstring names this exact edit — *"add a
bypass to any interpreter file and give it a row, without taking the count off
another row"* — as the thing it exists to catch. Relaxing that budget for a
bypass this lane neither added nor adjudicated turns a ratchet into a rubber
stamp. The two honest resolutions both belong to whoever owns that dispatch
path: route the site through `MemberResolver`, or move the count off another row
in the same commit.

**They took the first one.** `dev`'s `fix/ir-inline-unresumable-deopt-20260828`
work landed while this lane was merging and `invoke.rs` is back to three sites,
so both tests are green on the tree being pushed. Raising the row to 4 would
have left a rubber-stamped allowlist behind a green gate; declining to left the
ratchet doing its job until its owner moved the site.

`RJdkEnumerations` is dev's recorded red, bisected to `a0168ed03`
(`rjdkenumerations-is-red-on-dev-from-the-chm-values-cursor-20260827.md`), and
it reproduces here with the identical signature — including in the DEFAULT mode,
as that page's own correction says:

```text
java.lang.AssertionError: ConcurrentHashMap.elements(): hasMoreElements() never terminated
  at RJdkEnumerations.concurrentHashMap(RJdkEnumerations.java:208)
  at RJdkEnumerations.drain(RJdkEnumerations.java:113)
```

`RBlockingQueue` failed once in the pre-merge `--jdk-only` arm, **passes
standalone on the same binary**, and did not fail again in the merged tree's
three arms — the documented load flake of `HANDOFF-20260812.md`, checked rather
than assumed.


## 10. Two reds this lane did not cause, cleared because they blocked everyone

`origin/dev` moved twice more between the first merge and the push — nine and
eleven commits — and each time brought something of its own.

### 10.1 `Class.forName` on an array descriptor named the descriptor

The eleven-commit merge took `RExceptions` (core) and `RJdkFailure` (strict and
all) red. Both report the same thing:

```text
java.lang.AssertionError: an array CNFE must name the element, not the
descriptor: [Lcom.cratonvm.absent.NoSuchClass20260731;
```

The cause is a new early arm in `native_class_for_name`
(`native-builtins/src/lang_class.rs`) that resolves an array DESCRIPTOR without
consulting a loader. The arm is right to exist — `Class.forName("[I")` must
work while `ClassLoader.loadClass("[I")` must throw, and handling the descriptor
before the delegation is what lets the two doors keep different contracts. Its
miss path threw `ClassNotFoundException(dotted_name)`, the whole descriptor,
where HotSpot names the element:

```text
Class.forName("[Lp.X;")    HotSpot  CNFE msg="p.X"     cause=null
                           here     CNFE msg="[Lp.X;"
```

**Both other CNFE arms of that same function already call
`for_name_cnfe_name`**, which is exactly this rule and a no-op for every
non-array name. The new arm returns before either can run, so an invariant the
file documents in three places stopped holding at the door that now answers
first. Fixed by calling it there too — one argument.

Cleared here rather than left, because this is the opposite case from the
`invoke.rs` allowlist above: nothing is being relaxed, the correct value is
already computed by a helper used twice in the same function, and the assertion
that failed states the contract in its own message. Verified as dev's before
touching, by the rule the scope brief gives: the arm arrives in
`git diff <first-merge>..HEAD -- native-builtins/src/lang_class.rs`, and this
branch's own diff does not list that file.

### 10.2 `dev`'s tip did not COMPILE

The next merge would not build at all:

```text
error: could not compile `cratonvm-native-builtins` (lib) due to 16 previous errors
```

All sixteen inside two `native-builtins/src/craton_gpu.rs` handlers,
`builtin_future_status` and `builtin_array_to_host`, which had lost their
`#[cfg(feature = "gpu-offload")]`. Every other handler in that module has it, so
on a default build those two were compiled while the imports they need
(`Value`, `state`, `arg_long`, `rebuild_java_array`, `array_replace_bytes`) were
gated out from under them.

**Proven to be `dev`'s, not the merge's**, on a pristine detached worktree at
`origin/dev` (`8f9ae7a9c`) with its own target directory and nothing of this
branch in it:

```bash
git worktree add /data/cvm-devcheck-l7 --detach origin/dev
cargo check -p cratonvm-native-builtins --lib     # the identical 16 errors
```

Fixed by restoring the attribute on both — which is the contract the module's
own header states (*"On a default build it compiles down to an empty
`register()` that does nothing"*), and no `not(feature)` shim is needed because
both are named only from `register()` and from
`#[cfg(all(test, feature = "gpu-offload"))] mod tests`, both gated.

Worth stating plainly because a broken tip is worse than a red gate and much
easier to misattribute: **any lane that merged `dev` in this window and saw
`native-builtins` fail to compile was looking at this, not at its own work.**


## Reproduce

```bash
ssh -i ~/.ssh/azure.pem azureuser@20.80.105.49
source /data/toolchain/env.sh
cd <worktree> && cargo build --release -p cratonvm-cli -j6
cd probes && mkdir -p out
SB=/data/cratonvm/apps/spring-boot/smoke-test
H2=/data/cratonvm/apps/h2database/h2
TC=/data/cratonvm/apps/tomcat
javac -d out -cp "$(cat $SB/spring-boot-smoke-test-tomcat-ssl/build/cratonvm-test-cp.txt)" \
      DodSpringApp.java DodServiceLoaderSweep.java
javac -d out -cp "$(cat $H2/craton-testcp.txt):$H2/target/classes:$H2/target/test-classes" \
      DodJdbcWorkload.java DodH2JdbcSuite.java
javac -d out -cp "$(cat $TC/.suite/cp-linux-fixed.txt)" DodJUnitRunner.java
javac -d out DodArrayStoreSweep.java
cd ..

export DOD_CVM=/data/l7dod-target/release/cratonvm
export DOD_CLASSES=$PWD/probes/out DOD_OUT=/data/dod-out
for m in hotspot compat strict; do bash probes/dod-arms.sh $m; done
python3 probes/dod-summary.py /data/dod-out
python3 probes/dod-report.py /data/dod-out/rep-tcssl-strict.json
```
