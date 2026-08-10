# `AotIntegrationTests` now HANGS where it used to fail in 20 minutes

| | |
|---|---|
| **Status** | **CLOSED 2026-08-10**, retired here out of `known-issues/spring/`. Both live methods pass under CratonVM with the JIT on. The hang was a JIT defect: a compiled `invokestatic` bound its owner class by NAME, so under `@CompileWithForkedClassLoader` it called into the *other* loader's copy. Read the last section first — everything above it is the investigation as it stood, including two readings this closed out as wrong. |
| **Scope** | `org.springframework.test.context.aot.AotIntegrationTests` (spring-framework, `spring-test`). |
| **Oracle** | HotSpot `found=4 succ=2 fail=0 skip=2`, 63 s. |
| **Cap your timeout** | Applied to every binary before `3f8c53583`. It no longer does. |

## What changed

`de3c4d35b` / `5d265dbeb` fixed `Collections.unmodifiableList(x).get(i)`
throwing `ArrayIndexOutOfBoundsException` on a **valid** index. That fix is
correct and independently verified (see
`docs/known-issues/repros/list-get-oob/UM.java`), and it takes
`BeanRegistrationsAotContributionTests` from 8/6 to **14/14 = HotSpot**.

It also changed this class's failure mode:

| binary | collections fix | nested tests started | outcome |
|---|---|---|---|
| pre-fix | no | 4 | `found=4 succ=0 fail=2 skip=2` in **1209 s** |
| post-fix | yes | 2 | **hang**, killed at 10754 s |
| post-fix + data-slot redesign | yes | 2 | **hang**, identical |

## Why this is most likely UNMASKING, not a new defect

Before the fix, `CompileWithForkedClassLoaderExtension.runTest:140` —

```java
if (summary.getTotalFailureCount() > 0)
    throw summary.getFailures().get(0).getException();
```

— threw `AIOOBE` out of `.get(0)` itself and aborted the test method early. With
`get` working, execution proceeds into a phase it had never reached, and that
phase spins. This is the same shape as the CharBuffer `address` fix uncovering
the javac CCE beneath it: a correct fix exposing the next defect down.

**Not proven.** The alternative — that the fix itself introduced the loop — has
not been ruled out, and the obvious mechanism for it was ruled out (below). Do
not repeat the ruling-out; do the `--nojit` run.

## What has been ruled out

* **Not the per-`get` cost.** The first fix used `al_state`'s SIZE as the
  readability signal, which is 0 for an unreadable backing, so the fallback
  fired on every `get` — a VM dispatch per element. `de3c4d35b` switched to the
  DATA slot and removed the extra dispatch entirely. **The hang is identical
  either way**, so that was not it.
* **Not host contention.** The 10754 s run overlapped a 2.9 h Bean run; the
  re-run had the box to itself and hung the same way at the same line.
* **Not a deadlock.** One core is saturated throughout (CPU advanced 31 s per
  30 s of wall clock). It is a spin, not a wait.

## The one hard clue: the loop is in COMPILED code

`--stack-sample-ms 15000` produced **zero** `T19.H1 stack dump` records over the
whole run. That flag samples the interpreter dispatch loop, and its own help
text says JIT-compiled frames never reach it. Zero samples with a core pinned
means the loop is in JIT-compiled code.

Corroborating: RSS collapses to **10 MB** (from 157 MB) and stays flat — a tight
loop touching almost nothing.

## Where it stops

Identical in every post-fix run, at the second nested TestNG suite:

```
FINE [...LoggingListener] alter: [[Suite: "Command line suite" ...]]
FINE [...LoggingListener] onStart:  org.testng.TestRunner@…
FINE [...LoggingListener] onFinish: org.testng.TestRunner@…     <- stops here
```

with **no** `onTestStart` between them, where the pre-fix run ran a full
`BasicTestNGTests` / `BasicSpringTestNGTests` sequence at the same point
(`onTestStart` 4 pre-fix vs 2 post-fix). So the nested suite is now discovering
nothing and then spinning, rather than spinning mid-test.

## Next step

Run it with `--nojit`. That is the one arm that turns the sampler back on for
this loop:

```bash
KRUN_STACK=1 <beanall.sh> cvm aotnojit \
  org.springframework.test.context.aot.AotIntegrationTests --nojit --stack-sample-ms 15000
```

Expect it to be much slower — budget accordingly, and cap the timeout. If the
loop disappears under `--nojit`, it is a JIT defect and
`docs/known-issues/…/jit-*` is the right neighbourhood; if it persists, the
sampler will finally name the Java frame.

---

## 2026-08-06, later: the hang is gone, and what is under it is now named

Re-measured on `dev` @ `0bcbe2032` + `list-out-of-range-accessors-returned-null-FIXED-20260806.md`,
Azure `20.83.144.174`, real JDK 25, repaired classpath (0 of 254 missing).
**Do not budget 90 minutes any more — this is a 49-second failure.**

### Run the two methods separately

The class's two live methods differ by an order of magnitude in cost, and only
one of them was ever the problem. `apps/spring-suite-runner/onem.sh` runs a
single method with `one.sh`'s classpath and JVM args:

```bash
CRATONVM_BIN=<bin> ./onem.sh org.springframework.test.context.aot.AotIntegrationTests endToEndTests
```

| method | this binary | HotSpot |
|---|---|---|
| `endToEndTests` | **`found=1 succ=1 fail=0`**, 73-76 s | agrees |
| `endToEndTestsForBeanOverrides` | `found=1 succ=0 fail=1`, **49 s** | passes |

That is the whole reason to stop using the class as the oracle: a 900 s+
whole-class run that spends nearly all of its time in one method, on a shared
box that OOM-killed four consecutive acceptance attempts here.

### The remaining failure, and where it is

```
FAIL endToEndTestsForBeanOverrides() :: java.lang.IllegalArgumentException: array element type mismatch
	at org.springframework.core.annotation.TypeMappedAnnotation.adapt(TypeMappedAnnotation.java:487)
	at ...AbstractMergedAnnotation.getValue(AbstractMergedAnnotation.java:177)
	at ...SynthesizedMergedAnnotationInvocationHandler.invoke(...:76)
	at ...ContextLoaderUtils.resolveContextHierarchyAttributes(ContextLoaderUtils.java:138)
	at ...TestContextAotGenerator.processAheadOfTime(TestContextAotGenerator.java:181)
```

`TypeMappedAnnotation.java:487` is `Array.set(array, i, annotations[i].synthesize())`,
filling a `ContextConfiguration[]` with synthesized annotation proxies.

`Array.set`'s refusal now names both sides under `CRATONVM_DBG=coerce`
(added with this note; the message itself stays HotSpot's bare wording because
source witnesses pin it):

```
[DBG_COERCE] Array.set: rejecting -- array=org/springframework/test/context/ContextConfiguration
    component=org/springframework/test/context/ContextConfiguration (cid=ClassId(2547))
    value_class=jdk/proxy3/$Proxy27 (cid=ClassId(2552))
```

Exactly one rejection per run. So `is_subclass($Proxy27, ContextConfiguration)`
is false for a proxy that was created *from* that annotation type.

**This is almost certainly not a real type error.** It is the shape recorded in
`field-set-argument-type-mismatch-is-a-loader-split-use-dbg-coerce`: one class
NAME with two `ClassId`s under two loaders. `jdk/proxy3/` — not `jdk/proxy1/` —
says this proxy was defined for a third loader, and this class runs everything
under `@CompileWithForkedClassLoader`. Corroborating: a standalone
`Array.set(Ann[], Proxy.newProxyInstance(cl, {Ann.class}, h))`
(`repro/ArraySetProxyRepro.java`) is byte-identical to HotSpot, proxies
included — so `Array.set` and `is_subclass` handle proxies correctly when there
is only one loader in play.

### Next step

Print the proxy's recorded interfaces and the loader id of both `ClassId`s at
the rejection, then compare with `--dump-*` for the two `ContextConfiguration`
entries. If they are two ids for one name, the defect is in how the forked
loader's parent delegation is modelled, not in `Array.set` —
`user-loader-parent-chain-was-unmodelled-rust-side` and
`forked-loader-mixed-copies-triage-recipe` are the right neighbourhood, and
`Array.set` is only where it happens to surface.

---

## 2026-08-07: why it surfaced in `Array.set` and nowhere else

Two facts, both established by reading the tree rather than by running
anything. Together they answer the "why here?" the section above left open.

**1. `Array.set` was the strictest assignability check in the VM.** The
loader-identity-blind name walk this needs already exists —
`Class::is_assignable_to_name`, over supers *and* the interface DAG — and its
own doc comment names `@CompileWithForkedClassLoader` child loaders re-defining
app classes as the shape it was built for. It was already wired into five
paths: exception `catch_type` matching (×3), JIT `checkcast`/`instanceof`, the
recovered-mirror receiver check, `VarHandle` return coercion, and the
`Serializable` probe. The sibling reflective coercion in `lang_class.rs` goes
further and skips the check entirely when the expected type is an interface.
`Array.set` had none of it and compared `ClassId`s.

**2. The `aastore` bytecode makes a completely different check, and it would
have allowed this store.** `typecheck::aastore_element_assignable` — shared by
the interpreter opcode and the JIT's `jit_aastore`, per its own doc, "so both
enforce the same rule" — is deliberately *additive*: it must never produce a
FALSE `ArrayStoreException`. It fails open for an **interface component**, for a
value whose class name `contains("$Proxy")` or is `AnnotationProxy`, for a
synthetic class id, and for a same-named component that resolved to a different
`ClassId` under another loader. Each hedge was paid for by a regression; one
comment names storing an `AnnotationProxy` into an annotation-type array, which
is this case exactly.

So the two paths would have given opposite answers on the same store, and
`Array.set` was the one that had been taught nothing.

That is the answer to "why here?". `TypeMappedAnnotation.adapt` is generic over
the component type, so it *must* go through `Array.set` where ordinary code
emits `aastore` and sails past. A loader split presumably present in plenty of
places met the one check strict enough to notice it.

> **Correction, same day.** The first version of this section said the
> interpreter throws `ArrayStoreException` *nowhere* and that there is no
> `aastore` check at all. That is wrong. It came from a `grep` for the literal
> string that hit its result limit before reaching `vm/src/runtime/`, and a
> truncated result was read as an exhaustive one — the check is spelled
> `RuntimeError::ArrayStoreException`, in `opcodes.rs` and `helpers.rs`, behind
> the predicate above. The conclusion survives, but for a better reason than
> the one first given: the bytecode path is not *unchecked*, it is checked
> **leniently and on purpose**, and that is a far stronger argument for
> `Array.set` adopting it than "nothing else checks".

### What landed

`reflect_array_element_assignable` keeps its exact `is_subclass` test and then
calls the shared predicate, through a new
`NativeClassAccess::aastore_element_assignable` that the VM implements by
delegating to `typecheck::aastore_element_assignable`. Reflective `Array.set` is
now its third caller alongside the interpreter and the JIT, which is what
HotSpot does — there, the two are one rule. `None` from the trait method (the
default) means "this context models no hierarchy", so mocks and the
fabricated-class harnesses keep exact-only behaviour.

This deliberately replaced a first attempt that bolted a name-based walk onto
`Array.set` directly. That would have been a fourth reimplementation of one of
five hedges, and not even the ones that matter here — the interface-component
and `$Proxy` arms are what actually admit this store.

`CRATONVM_DBG=coerce` now also prints both loader ids and the value's
superclass chain, matching the `lang_class.rs` sibling.

Test: `a_proxy_is_assignable_to_the_other_loaders_copy_of_its_interface_by_name`
(`classloading/src/class.rs`) builds the two-copy hierarchy explicitly — two
same-named `ContextConfiguration` ids, a proxy listing the forked one — and
asserts both arms plus a negative. It characterises `is_assignable_to_name`,
which five other paths depend on and none of which had such a test.

### What is still open — read this before believing the fix

* **Not verified end-to-end.** `endToEndTestsForBeanOverrides` runs on the
  Azure Spring host; this was verified only at the unit level. Re-run
  `onem.sh … endToEndTestsForBeanOverrides` (49 s) to confirm.
* **But it is now much less conditional than the first attempt was.** The
  name-walk version only worked if the proxy had recorded an interface *named*
  `ContextConfiguration` — leaving the doc's never-ruled-out alternative (the
  forked loader's proxy definition recorded **no** interfaces at all) as a live
  way for the fix to do nothing. The shared predicate does not depend on that:
  `vn.contains("$Proxy")` matches `jdk/proxy3/$Proxy27` on its name alone, and
  the interface-component arm fires on `ContextConfiguration` being an
  annotation type, before any interface list is consulted. Either arm admits
  this store.
* If a rejection still prints after this change, the enriched diagnostic is the
  next read: it means the store reached `Array.set` with a component that is
  *not* an interface and a value that is *not* proxy-named, which would be a
  different finding from the one recorded here.
* **The root cause is untouched.** If the split is real it is still there; this
  is a mitigation at the point of surfacing.
  `user-loader-parent-chain-was-unmodelled-rust-side` remains the neighbourhood.
* ~~**`Array.set`'s refusal has NO test.**~~ **CLOSED** — two now, in
  `vm/src/runtime/interpreter/tests.rs`:
  * `aastore_refuses_a_real_mismatch_and_still_fails_open_where_it_must` — the
    control (concrete component, unrelated concrete value → refused) asserted
    *together with* two of the fail-open arms (interface component, `$Proxy`
    value → accepted). Deliberately paired: the refusal alone would pass against
    a predicate that refuses everything, and the lenient arms alone are exactly
    what a degenerate `true` satisfies. Class names are neutral
    (`cratonvm/test/Aastore*`) because `synthetic_implements`, the last fail-open
    arm, is a table of specific name pairs and a JDK-ish name could match it and
    make the control vacuous.
  * `array_set_routes_through_the_shared_aastore_predicate` — a source witness
    that `reflect_array_element_assignable` still calls the shared predicate,
    since the behavioural test above cannot see the wiring and `native-builtins`
    cannot host a test of its own (~620 pre-existing compile errors in its test
    targets from the fallibility migration). Matched on text within the
    function's own bounds, never on line numbers.

  **Both were shown to fail before being trusted.** Degenerating the predicate
  to `return true` fails the control; deleting the call from `Array.set` fails
  the witness. A control never shown to fire is indistinguishable from one that
  cannot.

  The blocker recorded here earlier — "needs synthesized class files" — was
  wrong. `ClassManager::ensure_synthetic_class` fabricates a named class
  directly in the class manager, which is all `array_descriptor_of` needs, and a
  reference array's own class id *is* its component class id, so the whole
  fixture is a dozen lines against a default `SharedVm`.

  The loader-split arm — the one this whole entry is about — is covered too, by
  `aastore_fails_open_across_a_split_loaders_two_copies_of_one_name`. It builds
  the two-copy state that `ensure_synthetic_class` will not produce directly, by
  fabricating the second copy under its own name and renaming it in place. That
  leaves `ClassManager`'s name index stale, which is safe *on this path only*:
  the predicate reads `class.name` from the store and recovers `comp_id` from
  the array's own class id, so no by-name lookup is consulted. The test says so,
  because the trick is not safe to copy into a test that does exercise name
  resolution.

  It covers both scenarios — the value being the other copy, and a subclass
  whose superclass edge reaches it — each asserted against `is_subclass_of`
  being `false` first, so an acceptance cannot be legitimate subtyping in
  disguise. A third assertion keeps an unrelated class refused; without it, all
  of the above would also pass against a degenerate `true`.

  **Finding from mutating it: the predicate's two split checks are not two.**
  The explicit `value_class.name == comp_name && value_class_id != comp_id`
  early return is *subsumed* by the by-name superclass walk immediately below
  it, because that walk starts at `value_class_id` itself and its first
  iteration tests the same name equality. Disabling the walk fails the test;
  disabling the fast path changes nothing. Left in place — a redundant early
  return is not a wrong one, and deleting it is a separate decision — but nobody
  should read those as independent defences.

### Two things ruled out — do not repeat them

* **Not the young-GC livelock**, however much `jcmd GC.heap_info` looks like it.
  On the older binary that did hang, it read `Young 1024.0 MB / 1.0 GB (100.0%
  used)` with `Old 135.9 MB (6.6%)`, frozen byte-for-byte across eight minutes —
  the exact signature in `young-gc-trigger-livelock-under-nonmoving-sweep`. It
  was not that: `CRATONVM_DBG_YOUNG_TRIGGER=1` reported
  `free_list=1002MB live=21MB threshold=921MB`, i.e. a young generation with
  1 GB free. The 100 % is a high-water cursor the non-moving sweep never
  retreats, and the GOOD control binary reaches 100 % too and then finishes.
* **Not the per-`get` cost, confirmed independently.** A build carrying the
  variant of the collections fix that *does* pay a `size()` dispatch per `get`
  hung identically to one that does not, on the same host, same method.

---

## 2026-08-10: closed. The hang was a compiled `invokestatic` binding its owner by NAME

Azure `20.80.105.49`, worktree `/data/cratonvm-aot-20260810`, real JDK 25
(`/data/toolchain/jdk-25`), repaired classpath (0 of 253 missing), one process
per method.

| method | before (`58ffc3e60`) | after (`3f8c53583`) | HotSpot, same host |
|---|---|---|---|
| `endToEndTests` | `found=1 succ=1` | `found=1 succ=1` | agrees |
| `endToEndTestsForBeanOverrides` | **hang** (killed at 16 min and again at 22 min, one core at 98 %) | **`found=1 succ=1`**, 8 m 15 s | `found=1 succ=1`, 175 nested tests, < 5 min |

### The `Array.set` fix works, and it unmasked the next defect

Verified end-to-end, which the entry above could not: a full run under
`CRATONVM_DBG=coerce` produced **zero** `Array.set` rejections. (The 28
`DBG_COERCE` lines it did produce are all `Field.set`, on
`org/apache/logging/log4j/Level` — a *different* site, and a separate finding
left open below.)

And with the refusal gone, the method ran on into a phase it had never reached
and spun — the same unmasking shape the top of this page predicted for the
*previous* fix, one layer down.

### Two readings above are wrong, and both cost real time

* **"The nested suite is discovering nothing and then spinning."** It is not.
  The `alter` / `onStart` / `onFinish`-with-no-`onTestStart` sequence is where
  the *log* stops, not where the *run* stops: `LoggingListener` is the TestNG
  engine's, and everything after that point is JUnit Jupiter, which logs
  nothing at that level. `--stack-sample-ms 10000` showed the run moving
  through 26 distinct Spring test classes after it.
* **"Zero stack samples with a core pinned means the loop is in JIT-compiled
  code."** The sampler produced **82 records in 13 minutes at a 10 s interval**
  — ~100 % of expected. Whatever produced zero samples on the older binary, the
  loop this page is about is fully visible to it. Read sampler *coverage*
  (serviced/expected) before concluding anything from an empty result.

### Where it actually spun

15 consecutive samples at an identical 244-frame depth, leaf cycling among
three methods of one class:

```
GenericTypeAwareAutowireCandidateResolver.isAutowireCandidate
  ResolvableType.isAssignableFrom            (x2)
    ResolvableType$WildcardBounds.get        <- the loop lives here
      ResolvableType.resolveType / getType / isUnresolvableTypeVariable
```

`WildcardBounds.get` is

```java
ResolvableType candidate = type;
while (!(candidate.getType() instanceof WildcardType || candidate.isUnresolvableTypeVariable())) {
    if (candidate == NONE) return null;
    candidate = candidate.resolveType();
}
```

An instrumented copy of `ResolvableType.java` compiled against the fixture and
prepended to the classpath (`/data/aot-runs/shadowcls`) printed the state at
iteration 65+:

```
candEqNone=false  candClass=…  noneClass=…  sameClass=false
candLoader=jdk.internal.loader.ClassLoaders$AppClassLoader
noneLoader=org.springframework.core.test.tools.CompileWithForkedClassLoaderClassLoader
```

`candidate` **was** `NONE` — the application copy's `NONE`, while `get` was
running in the forked copy, whose `NONE` is a different object. The reference
comparison could never hold. HotSpot has the same two copies (the forked loader
extends `testClassLoader.getParent()`, so `org.springframework.*` misses the
parent and it defines its own) and never mixes them.

The instrumented run also converted the hang into a failure and completed,
showing exactly **3** occurrences — this is a small, specific defect, not a
pervasive one.

### Root cause

`--nojit` passes the same method with the loop-detector never firing. That is
the arm the top of this page asked for and never ran, and it is decisive: the
interpreter resolves a field/method owner through the caller's own loader, and
the JIT did not.

`jit_invoke_dispatch`'s `invoke_kind == 3` (`invokestatic`) arm resolved its
callee through `invoke_or_native(info.class_name, …)` — the flat global
binary-name map. `JitInvokeInfo::declaring_class_id` exists precisely so this
cannot happen, and the `invoke_kind == 1` (`invokespecial`) arm has used it
since BUG-JIT-INVOKESPECIAL-LOADER-20260726. The fix landed on invokespecial and
stopped, leaving behind a comment — *"invokestatic: no receiver, no retarget
concern"* — that is true of the DISPATCH and false of the IDENTITY.

So a static call made from the forked `ResolvableType` landed in the
application copy and handed back an application-copy `ResolvableType`.

### What landed (`3f8c53583`)

`jit_static_owner_override` in `vm/src/jit/helpers.rs`, consulted by the
`invoke_kind == 3` arm, plus `invoke_static_shared_on_class` in `vm_exec.rs`
(the `invoke_special_shared_impl` body, whose "walk to the declaring class and
dispatch on exactly that class" is also JVMS §5.4.3.3's static lookup once
there is no receiver to retarget).

**Strictly additive by construction.** Three gates, cheapest first — no
call-site class id; the process-wide `any_defining_loader_registered` latch;
and `classify_loaded_name` reporting anything but `Ambiguous` — and then the
loader-faithful owner is used *only when it differs from the global by-name
answer*. A process with one copy of the name takes the identical previous path,
including everything `invoke_or_native` carries that the on-class form does not
(native-override order, the `SyntheticStub`-yields-to-real-bytecode rule, the
signature-polymorphic intercepts).

The same commit resolves the "separate decision" the entry above left: the
subsumed `value_class.name == comp_name && value_class_id != comp_id` early
return in `aastore_element_assignable` is gone, folded into the by-name walk
whose first iteration already covered it. The split-loader test stayed green,
which is the confirmation that argument needed.

### It is not a silent fix

Under `CRATONVM_DBG=coerce` the override prints when it fires. On this method
it fired **97 695 times in 87 s**, and the callees are exactly the machinery in
question:

```
158318  Assert.notNull(Object,String)V
 15887  MergedAnnotation.missing()
  8808  ClassUtils.isInnerClass(Class)Z
   763  SynthesizedMergedAnnotationInvocationHandler.createProxy(MergedAnnotation,Class)
   626  SerializableTypeWrapper.unwrap(Type)
```

`createProxy` is the call that mints the `jdk/proxy3/$Proxy27` whose store into
a `ContextConfiguration[]` is what the whole first half of this page is about.
**The loader split this page could only hypothesise is the same defect**, and
`Array.set` adopting the lenient `aastore` predicate was a mitigation at the
point of surfacing, exactly as it said.

### Coverage

* `a_compiled_invokestatic_resolves_its_owner_in_the_callers_own_loader`
  (`vm/src/jit/helpers.rs`) — two copies of one name, a call site inside each,
  and **three** assertions: the fix, plus the two negatives (`Unique` name, and
  a caller whose loader agrees with the global map) that are what keeps every
  ordinary dispatch on the old path. Shown to fail three ways: making the
  override never fire, making it always fire, and dropping the latch gate each
  fail the corresponding test.
* `jit_invokestatic_owner_override_is_gated_before_it_is_consulted` — a source
  witness for the two process-wide pre-gates, which a behavioural test cannot
  arm without leaking a global latch into the whole test binary.

### A/B regression sweep

The change can only reach a process that has a user-defined loader AND a
name with two definitions, so the sweep targeted exactly that: every
spring-framework test class that uses `@CompileWithForkedClassLoader`, plus
`BeanRegistrationsAotContributionTests`, `MergedAnnotationsTests` and
`ResolvableTypeTests`. Results in `/data/aot-runs/ab-{base,fix}/results.tsv`.

### Still open, and NOT this page's defect

`Field.set` refuses 26+2 times per run on
`org/apache/logging/log4j/Level` — `expected` cid 5003 (loader 2) vs `arg_class`
cid 1454 (loader 3), one binary name with two definitions, the caller being
`LoggerConfig$Builder`. Same family, different site: the `Field.set` path has
no loader-faithful owner resolution either. It does not affect this class's
verdict (both methods pass with it happening), so it is recorded rather than
fixed here.
