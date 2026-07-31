# Spring AOT cluster — CLOSED

| | |
|---|---|
| **Status** | **CLOSED 2026-07-31.** All 20 chunks match HotSpot; chunk 6 beats it. The last residual, chunk 4, was a `StringBuilder` correctness bug, not the throughput bug it had been attributed to. |
| **Scope** | `org.springframework.test.context.aot.AotIntegrationTests#endToEndTestsForBeanOverrides` — 150 test classes, grouped by top-level class into 20 chunks. |
| **Measured** | 2026-07-31, Azure host `20.83.144.174`, real JDK 25, `fix/spring-aot-cluster-residual-20260731` (dev `a31a8a93fe` + the fix below), binary `cratonvm-aotresid-r2.bin`. Both VMs re-swept end to end on the same day, same harness, same classpath. |
| **Predecessor** | Replaced `CRATONVM-SPRING-GENUINE-BUGLIST.md`, retired 2026-07-30. Its history is alongside this file. |

## Final state — all 20 chunks, both VMs, 2026-07-31

| chunk | CratonVM | HotSpot | |
|--:|---|---|---|
| 0 | 38/38 | 38/38 | |
| 1 | 37 found, 36 succ | same | |
| 2 | 6 found, 1 succ | same | |
| 3 | 42/42 | 42/42 | |
| **4** | **25 found, 24 succ, 1 fail** | **same** | was: does not finish |
| 5 | 20/20 | 20/20 | |
| 6 | **33/33** | 33 found, 31 succ | CratonVM ahead |
| 7–11 | identical | | |
| 12 | 10 found, 9 succ, 1 fail | same, **same failure** | |
| 13–18 | identical | | |
| 19 | 34 found, 32 succ, 2 fail | same | see below |

`succ < found` rows are the harness's own artefacts, equal on both VMs — which is
the point of comparing per chunk rather than counting failures. Raw logs:
`/data/tmp/allchunks-hs-0731/` and `/data/tmp/allchunks-cv-r2/`.

## Chunk 4: what it actually was

AOT processing completed (`PROBE aot-processing OK`) and the replay never
returned — killed at the 2400 s ceiling on every run, at 99% CPU.

It had been attributed to
[`../../mockito-redefine-makes-every-call-40us-20260726.md`](../../mockito-redefine-makes-every-call-40us-20260726.md),
the ~46x per-call cost a redefined class used to carry. **That fix is real and
landed on 2026-07-31 — and it was not what wedged chunk 4.** With it in place
and confirmed on this host (`RedefineCostProbe`: multiplier 1x; `SbCostProbe`:
32,876 → 444 ns/call after the mock), chunk 4 still did not finish.

A watchdog dump (`CRATONVM_DEFAULT_WATCHDOG_SEC=420`, 56,324 samples of the main
thread) is what turned it. The old reading of that stack — "javac's tokenizer,
therefore the per-call cost" — had skipped the frames that mattered:

```
JavacParser.parseCompilationUnit
  -> VirtualParser.<init>                        30% of samples
  -> topLevelMethodOrFieldDeclaration
  -> updateUnexpectedTopLevelDefinitionStartError
```

`updateUnexpectedTopLevelDefinitionStartError` is javac's *error* path. javac was
not slow, it was **failing to parse** — sitting in `parseCompilationUnit`'s
`OUTER:` error-recovery loop, speculatively re-parsing every token as an
implicitly-declared class. The generated sources were fine: dumping all 54 of
them from both VMs (`AotSrcDumpProbe`) gives **byte-identical** files.

### Root cause

CratonVM's builders are a synthetic two-field object — `char[] value`,
`int count` — not the JDK's compact `byte[] value` / `byte coder` / `int count`.
Every builder operation is therefore meant to resolve to a native shim; the list
that keeps it that way across a redefinition is
`is_string_builder_layout_native_override` in
`vm/src/runtime/interpreter/invoke.rs`.

Six operations were missing from it. One `Mockito.mock(StringBuilder.class)`
anywhere in the process evicts the native shadow, and those six then fell back
to real JDK bodies that index a layout the object does not have — silently
corrupting **every real StringBuilder in the process**:

| operation | before the redefinition | after |
|---|---|---|
| `setLength(4)` | `len=4 str=abcd` | `len=4 str=a` |
| `setLength(0)` | `len=0 str=` | `len=0 str=a` |
| `deleteCharAt(1)` | `len=9 str=acdefghij` | `len=9 str=a` |
| `replace(1,3,"Q")` | `len=9 str=aQdefghij` | `ArrayStoreException: src=Byte, dest=Char` |
| `ensureCapacity` | `abcdefghij!` | 11 spaces |
| `trimToSize` | `abcdefghij!` | 11 spaces |
| `repeat('x',3)` | `abcdefghijxxx` | `abcdefghij` |

`length()` stayed truthful while `toString()` did not, which is why this
presented as garbage rather than as an exception. `ArrayStoreException:
src=Byte, dest=Char` is the mechanism in one line: real JDK bytecode copying out
of a compact `byte[]` into the synthetic `char[]`.

javac's `JavaTokenizer` keeps one long-lived `StringBuilder` and calls
`sb.setLength(0)` at the top of every `readToken()`, plus
`sb.setLength(sb.length() - 1)` inside `scanOperator()`. Every identifier it
lexed came out wrong. Hence the parser loop, hence the "hang".

Fixed in `fix(natives): a redefinition must not hand REAL StringBuilders back to
JDK bodies`. `length()` and `substring(int)` deliberately stay OUT of the list —
they are the two operations `MockitoBeanByTypeLookupIntegrationTests` genuinely
stubs and verifies on a mocked builder, so their native shadow must stay
evictable for Mockito's woven advice to run. Three unit tests pin all three
sets.

Witness: [`../../../known-issues/repros/redefine-builder-layout/`](../../../known-issues/repros/redefine-builder-layout/)
— no Mockito, no Spring, no JUnit, no AOT. It redefines the builder classes with
their **own bytes** and diffs 28 operations across the redefinition. 8 DIFFs
before the fix, `PROBE PASS` after.

### The one remaining failure in chunk 4 is the probe, on both VMs

`contextLoadFailureCausesExpectedTestFailures()` fails on CratonVM and on
HotSpot, for different reasons, and neither is a VM defect:

* HotSpot dies first on `ServiceConfigurationError: TestNGTestEngine could not
  be instantiated` — the forked-TCCL artefact this document has always
  described.
* CratonVM instantiates that engine fine, gets further, and then fails the
  assertion: the nested `ContextLoadFailureTestCase` is reached only through
  `EngineTestKit`, so the cut-down probe never AOT-processes it, and under
  `spring.aot.enabled=true` it throws *"Failed to load AOT
  ApplicationContextInitializer class"* instead of the *"Failed to load
  ApplicationContext"* the assertion wants.

Settled two ways. The generated `AotTestContextInitializers__Generated.java` is
byte-identical between the VMs and contains no entry for
`ContextLoadFailureTestCase`, so any VM would throw the same thing. And adding
the nested class to the probe's own class list makes the test **pass** on
CratonVM:

```bash
./oneaot.sh tag org…MockitoResetTestExecutionListenerWithContextLoadFailureTests \
              'org…MockitoResetTestExecutionListenerWithContextLoadFailureTests$ContextLoadFailureTestCase'
# PROBE RESULT found=3 succ=1 fail=2  — the outer test passes; the 2 failures are
# ContextLoadFailureTestCase's own test1/test2, which are supposed to fail and are
# only selected here because the probe selects everything it processes.
```

Chunk 19's two failures are the same shape and were settled outright: strip the
TestNG engine jars from the classpath so HotSpot stops dying early, and HotSpot
produces CratonVM's failure exactly — `started ==> expected: <1> but was: <0>`,
twice.

## Reproducing

```bash
cd /data/data/aot20260726
# one chunk (~2-10 min)
CRATONVM_BIN=<binary> TMO=1500 XMX=2g ./oneaot.sh tag $(tr '\n' ' ' < bochunks/chunk.009)
# all 20 chunks, either VM (CRATONVM_BIN unset => HotSpot)
CRATONVM_BIN=<binary> ./allchunks.sh <tag>
# one class, with stacks
PROBE_STACK=1 CRATONVM_BIN=<binary> ./oneaot.sh tag <fqcn> [<fqcn>$NestedTests]
# dump the generated sources instead of compiling them, for a cross-VM diff
CRATONVM_BIN=<binary> ... ForkedProbeMain AotSrcDumpProbe <outdir> <fqcn...>
```

`AotE2EProbe`/`AotE2EProbe2` are `AotIntegrationTests.runEndToEndTests` cut down
to the named classes. Three harness rules that cost an hour each if broken:

* the `ForkedProbeMain` wrapper is **mandatory** (the generated
  `__TestContext001_BeanDefinitions` classes touch package-private members of
  the test class);
* a chunk **must** keep a class and its `@Nested` children together — splitting
  them fails on HotSpot too;
* a nested test case reached only through `EngineTestKit` is **not**
  AOT-processed by these probes, so any assertion about how it fails under
  `spring.aot.enabled=true` is about the probe, not the VM. Name it explicitly
  before drawing a conclusion.

`AotIntegrationTests#endToEndTestsForBeanOverrides` itself is not a debugging
loop: it did not complete in 3 h at `--Xmx 8g`.

## Closed 2026-07-30/31

Five defects, each with a standalone witness. Full rationale is in the commit
messages. The first four are the same shape — **a loader-blind lookup in a
process where two loaders define every name** — and the fifth is the one that
had been hiding behind a throughput story.

1. **Every AOT probe died in 1.3 s** — `NullPointerException … "this.logger" is
   null` at `AbstractEnvironment.setActiveProfiles`. log4j's `StackLocator`
   walked the stack, a frame's `getDeclaringClass()` answered **null**, and
   `construct_real_standard_environment` **swallowed the resulting exception**,
   handing Spring a synthetic `StandardEnvironment` built with no constructor.
   `declaring_class_native` had been re-resolving a frame's class through
   `class_id_by_name`, which is ambiguity-strict; the frame's own ClassId was
   already resolved to a mirror and simply never consulted. The swallow now
   reports. *(`fix(stackwalker): a frame's declaring class comes from the frame`)*

2. **A CGLIB AOP proxy inherited the wrong copy of its superclass.** The Rust
   side modelled **no parent chain at all** for user-defined loaders, so
   resolution against already-defined classes saw only the loader's own
   namespace and the built-in chain, then fell through to a loader-blind global
   path that DEFINES a second copy in the application namespace. Witness:
   `/data/data/pcprobe` `ParentChainProbe` — `Bar.getSuperclass()` resolved to
   the application loader's `Foo` instead of its parent loader's, in one second,
   with no Spring involved. `CRATONVM_LOADER_PARENT_CHAIN=0` restores the old
   behaviour for A/B on a single binary.
   *(`fix(classloading): model user-loader parent chains for class RESOLUTION`)*

3. **AssertJ soft assertions could not build their ByteBuddy proxy** —
   `IllegalArgumentException: Could not create type` … `Cannot resolve T`.
   CratonVM's own ByteBuddy shims built their results with
   `new_object_initialized(<literal name>, …)`, which resolves globally and so
   returned the **application** loader's copy whoever called. Two copies of
   `TypeDefinition$Sort` then compare unequal, `getTypeVariables()` filters
   every type variable away, and a generic method looks non-generic.
   *(`fix(natives): ByteBuddy shims must build their result in the CALLER's loader`)*

4. **`@MockitoSpyBean` stubs vanished behind a Spring AOP proxy.** The spy was
   fine — a direct call returned the stub — but the call through the proxy ran
   the real method with **no interceptor frames at all**. The proxy's override
   is package-private and had been defined into `DynamicClassLoader` while its
   superclass lives in the fork loader: same package name, different runtime
   packages, so per JVMS §5.4.5 it does not override. HotSpot avoids this
   because `java.base` does not `opens java.lang`, so Spring CGLIB cannot use
   the reflective `ClassLoader.defineClass` and falls back to
   `Lookup.defineClass` on the neighbour class — which lands the proxy in its
   superclass's loader. CratonVM allowed the reflective call.
   `probes/DefineClassAccessProbe.java` is the witness.
   *(`fix(reflect): keep ClassLoader.defineClass encapsulated, as HotSpot does`)*

5. **A redefinition corrupted every real `StringBuilder`** — chunk 4, above.
   *(`fix(natives): a redefinition must not hand REAL StringBuilders back to JDK bodies`)*

### Three investigation notes worth reusing

* **`setAccessible` has three competing natives** (`lang_class::set_accessible_impl`,
  `lib.rs`'s `native_set_accessible_write_override`, `lang_reflect`'s
  `AccessibleObject` variants). A guard added to one of them is silently
  bypassed — the first attempt at fix 4 looked like a no-op for exactly that
  reason.
* **Instrument the real test before writing a fourth synthetic probe.** Four
  successively closer standalone probes for fix 4 (plain spy → spy behind an AOP
  proxy → proxy in a child loader → package-private method) **all passed**; the
  bug needed the real AOT context. Compiling an instrumented copy of the failing
  test into a directory placed ahead of the suite on `-cp` costs two minutes and
  prints whatever you want from inside the real run. That is what cracked it.
* **A "spinning, not blocked" stack is not automatically a throughput bug.**
  Chunk 4 cost an extra session because the profile agreed with the throughput
  story and nobody read far enough down the frames to notice javac was in its
  *error* path. Two cheap checks would have caught it much earlier: diff the
  generated artefacts across the VMs (they were identical — so the input was
  fine and the consumer was broken), and check whether the loop is making
  progress at all before asking how fast it is.
