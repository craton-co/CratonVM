# FIXED — the Spring Framework FAIL clusters left after the `ConcurrentHashMap` bug

## Status
**FIXED 2026-08-20** on `fix/spring-chm-clusters-20260820`. Filed 2026-08-19 as
a triage/pointer doc over the classes still failing under all three GC variants
once the `ConcurrentHashMap.entrySet().removeIf` cluster was accounted for. Six
items were listed. Every one is now resolved: four were CratonVM defects and are
fixed, one was already fixed on `dev`, and one turns out **not to be a CratonVM
defect at all** — it fails identically on HotSpot.

Measured on the same 90 classes (the intersection of the non-OK sets of the
three 2026-08-19 GC-variant runs), same driver, same host, binary the only
variable:

```text
                                          classes OK / 90
  dev tip 26e4b5db4                            15
  + ConcurrentHashMap removeIf fix             83
  + everything in this doc                     88
```

The two that remain are named at the bottom; neither is one of this doc's items.

## The six items, and what each turned out to be

### 1. `AnnotationTransactionAttributeSourceTests.serializable()` — `InvalidObjectException: invalid object` — FIXED

The one-line failcause hid a much larger defect: **serializing any
`List.of`/`Set.of`/`Map.of`/`copyOf` collection was broken**, 20 of the 52 rows
of `probes/CollectionSerProbe.java`.

`ObjectOutputStream` looks a value's class up by the name `getClass()` reports.
For a CratonVM-minted immutable collection that is the aliased real JDK class
(`ImmutableCollections$List12`/`ListN`/`Set12`/`SetN`/`Map1`/`MapN`), each of
which declares its own `writeReplace()`. So the JDK body ran — over a receiver
whose declared `e0`/`e1`/`elements`/`table` fields this VM never fills — and the
`java.util.CollSer` it handed the stream carried this VM's own
`(backing, immutable-marker)` slots instead of the elements:

```text
                      HotSpot     CratonVM (before)
  List.of("a")        [a]         [[a], 1]
  Set.of("a","b")     [a, b]      InvalidObjectException: invalid object
  Map.of("a","1")     {a=1}       NPE: ... "this.table" is null
```

`Collections$UnmodifiableRandomAccessList` failed for the same reason and is the
only `Collections$Unmodifiable*` wrapper that declares a `writeReplace`; its
non-RandomAccess twin `unmodifiableList(LinkedList)` passed throughout, which is
the control.

Fixed in `native-collections/src/lib.rs` by
`register_immutable_serialization_natives`: `writeReplace` natives that build
the `CollSer` from the receiver's OWN public surface (`toArray()`,
`entrySet()`), so they are correct for a CratonVM carrier and for a genuine JDK
instance alike, and a `CollSer.readResolve` native that rebuilds through
`of_list`/`of_set`/`of_map` — needed because the real `readResolve`'s `IMM_MAP`
arm builds `new MapN<>(array)`, a real constructor whose `table` no map native
in this crate reads. Both need an entry in both force-native gates, because the
reflective `Method.invoke` route `ObjectStreamClass` uses has no bytecode PC to
key an invoke-cache entry on.

`AnnotationTransactionAttributeSourceTests` now 22/22, equal to HotSpot.

### 2. `JRubyScriptTemplateTests` (both classes) — FIXED

Filed under the (already-fixed) `MethodHandle.asSpreader` note as "recheck
against `dev` tip". Rechecked: still failing, and for an unrelated reason.

```text
IllegalStateException: Failed to evaluate script [.../render.rb]
  <- ScriptException: WrongMethodTypeException: cannot explicitly cast
     MethodHandle(ThreadContext,IRubyObject,IRubyObject,IRubyObject[])IRubyObject
     to (ThreadContext,IRubyObject,IRubyObject,IRubyObject,IRubyObject)IRubyObject
       at com.headius.invokebinder.Binder.invoke(Binder.java:1373)
       at org.jruby.ir.targets.indy.InvokeSite.<init>(InvokeSite.java:757)
```

Root cause: `MethodHandles.collectArguments(target, pos, filter)` gave its
adapter the TARGET's descriptor instead of the adapter's, so the handle
dispatched correctly and **lied about its type**. Nothing in CratonVM read that
type back, which is why it survived. invokebinder does: `Binder.invoke(target)`
walks its transforms calling each one's `up()` and then hands the result to
`MethodHandles.explicitCastArguments(handle, startType)`, which refuses on an
ARITY mismatch alone. JRuby's `InvokeSite.prepareBinder` folds the flat Ruby
arguments into the `IRubyObject[] args` parameter with `SmartBinder.collect`,
which lowers to exactly this combinator, so every JRuby call site failed to
link.

Fixed by `collect_args_adapter_descriptor` in
`native-builtins/src/lang_invoke.rs` (the javadoc'd rule: a value-returning
filter REPLACES the parameter at `pos` with its own parameter list, a `void`
filter INSERTS them). Both classes now 1/1.

Two neighbouring `MethodHandle` divergences were found by the probes written for
this and fixed alongside — `Lookup.find*`/`unreflect*` did not mark a
variable-arity target's handle as a varargs collector, and `MethodType.toString`
rendered an array parameter as `String;` rather than `String[]` (it is the
message text of every `WrongMethodTypeException`, so the difference lands
squarely in a diagnostic whose whole job is to be compared). See
`probes/VarargsCollectorProbe.java`.

### 3. `InvocableHandlerMethodKotlinTests.genericParameter()` — Kotlin reflection NPE — FIXED

```text
NullPointerException: Parameter specified as non-null is null:
  method kotlin.reflect.jvm.internal.impl.types.SimpleTypeImpl.<init>,
  parameter arguments
```

Not a reflection gap: **a JIT miscompile**. `--nojit` passed 30/30, and the
bisect narrowed it to one mechanism and then to one line.

```text
  --nojit                                          OK
  CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS=0    OK
  CRATONVM_JIT_LOCAL_REGS=0..3                     OK
  CRATONVM_JIT_LOCAL_REGS=4,5                      FAIL
  CRATONVM_JIT_DENY=KotlinTypeFactory.simpleTypeWithNonTrivialMemberScope  OK
  12 GiB heap / NO_CALLEE_OOP_FLUSH / SAFEPOINT_REG_SPILL=all /
  DISABLE_INLINE_NEW / DISABLE_SCALAR_REPLACEMENT                          FAIL
```

— i.e. deterministic, needs at least four colourable locals, and is not a GC
root-visibility problem.

`invalidate_callee_saved` reserves ONE fresh slot at the top of the operand
spill reserve and repoints EVERY entry that reads the register at it, including
entries buried under the top of the stack and including two entries at one
shared slot. `pop_stack` then reclaimed on `off == next_spill_offset - 8` alone,
which assumes the top entry owns the topmost slot. Popping the shallower of two
aliases satisfied that test while the deeper alias still read the slot, so the
next `push_stack` was handed it and the pushed value overwrote the buried
operand.

`KotlinTypeFactory.simpleTypeWithNonTrivialMemberScope` is a kotlinc body that
pops five operands into locals 7..11 — an invalidation each — while four earlier
operands still sit on the stack, then reloads all five to build a lambda and
finally calls a constructor with the four buried ones. The buried `arguments`
operand, pushed by its caller as `Collections.emptyList()`, arrived null.

Fixed in `jit/src/x64/operand_stack.rs`: reclaim only when no entry still on the
stack lives at or above the popped offset. The change can only ever DELAY a
reclaim, never free a slot earlier. Regression test
`pop_does_not_reclaim_a_slot_a_buried_entry_still_owns` in `jit/src/x64/tests.rs`
(verified to fail without the fix).

### 4. `WebClientIntegrationTests` — `VerifySubscriber timed out` — FIXED by item 3

The 2026-08-19 doc flagged this as possibly load-induced and asked for an
isolated rerun. Reran five times alone: NOT a flake — 1–2 failures every run,
always on the `[2] JDK` client variant, always the same
`VerifySubscriber timed out on ...MonoFlatMap$FlatMapMain` shape.

It then went away with the JIT fix above, with no change of its own: 170 found,
**169 succeeded, 0 failed, 1 skipped** — identical to HotSpot's own numbers on
this class. (Both VMs exit on the harness timeout afterwards; this class leaks
non-daemon threads on HotSpot too, so `rc=124` after a complete `RESULT` line is
the harness, not the VM.)

### 5. Groovy markup-template **compile**-time failures (2 classes) — FIXED by item 1 or the CHM fix

`GroovyMarkupViewTests` and `ViewResolutionIntegrationTests.groovyMarkup()` both
pass. The doc could not tell whether these were a CratonVM defect or a
Groovy/classpath version mismatch, because the pooled failcause truncated the
compiler error to `startup failed:`. They are green on the fixed binary and were
green already at the 83/90 point, so they belonged to one of the collection
fixes rather than to the Groovy compiler.

### 6. The "recheck against `dev` tip" Groovy/JRuby list — DONE

`GroovyAspectTests`, `GroovyAspectIntegrationTests`, `GroovyScriptFactoryTests`:
pass. Both `JRubyScriptTemplateTests`: were still failing, root-caused and fixed
as item 2.

### And one that is NOT a CratonVM defect: `FileNativeConfigurationWriterTests`

Five methods failing with a bare `AssertionError`. Pulling the full output gives
`Unexpected: comment` — a JSONAssert `NON_EXTENSIBLE` complaint that the written
`reachability-metadata.json` carries a `comment` key the expected JSON does not.
`RuntimeHintsWriter` writes that key iff `SpringVersion.getVersion()` is
non-null, i.e. iff `Package.getImplementationVersion()` answers for
`org.springframework.core`.

**HotSpot answers `7.1.0-SNAPSHOT` too**, on the same classpath, and fails this
class identically: `found=9 succ=3 fail=6` on both VMs. `probes/` has the
one-file check (`PkgVersionProbe` shape: the two VMs agree row for row on
`getImplementationTitle`/`Version`/`Vendor` for a directory entry, a jar entry
and a java.base class). This is a harness or upstream-test property of running
these classes outside Gradle, not a VM divergence, and it should not be counted
against CratonVM in any sweep summary.

## What is still non-OK in those 90, and why it is not in this doc

* `BeanRegistrationsAotContributionTests` — exceeds a 900 s cap where HotSpot
  takes 28 s. This is the AOT/Mockito dispatch-throughput wall, which predates
  this doc and has its own history (the reflective `Method.invoke` path through
  Mockito's constructor-mock listener, whose profile is recorded on
  `force_native_over_real_jdk_bytecode_memoized`). It was never one of this
  doc's clusters.
* `FileNativeConfigurationWriterTests` — see above; HotSpot fails it identically.

Separately, `probes/MhCombinatorProbe.java`'s purity section still fails four
rows: `asType` and `explicitCastArguments` adapt the RECEIVER in place instead
of returning a new handle. That is the pre-existing G31-1 nomination recorded on
the `asType` registration, it explains no suite failure that survives the fixes
above, and collapsing it changes `MethodHandle` identity VM-wide — it wants its
own task and its own verification rather than a rider on this one. The probe now
records it, which it did not before.

## Repro

```bash
cd apps/spring-suite-runner
SPRING=/data/cratonvm/apps/spring-framework JDK25="$JAVA_HOME" \
CRATONVM_BIN=<cratonvm-bin> ./run-suite.sh run --category all \
  --only 'AnnotationTransactionAttributeSourceTests|InvocableHandlerMethodKotlinTests|JRubyScriptTemplateTests|WebClientIntegrationTests|GroovyMarkupViewTests|ViewResolutionIntegrationTests' \
  --tag chm-clusters-recheck
```

Probes (each self-checks and prints `TOTALFAILS`; the `.expected.txt` next to
each is the HotSpot capture):

```bash
cd probes && javac -d /tmp/p ViewRemoveIfProbe.java CollectionSerProbe.java \
  VarargsCollectorProbe.java MhCombinatorProbe.java
```
