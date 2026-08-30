# The cast that `asType` performs, and this VM did not

**Status: FIXED 2026-08-30, all seven rows, plus two more the fix itself
found.** Worktree `h2-known-issues-206dee`, branch
`claude/jdk-only-mode-handoff-09b48c`. Instrument:
`probes/InvokeCastSweep.java`, 29 rows, both modes.

This closes L5's last recorded residual — *"`invoke`'s reference-argument
cast"* — and the `bindTo` half that W7-19 §5.1 declined with a reason.

## 1. What it did

`MethodHandle.invoke` is specified as `asType(callSiteType)` followed by an
exact invocation, and `asType` CASTS every reference argument to the handle's
declared parameter type. This VM passed them through untouched.

```text
cat = findVirtual(String, "concat", (String)String)
cat.invoke("ab", (Object) Integer.valueOf(3))
  HotSpot   ClassCastException: Cannot cast java.lang.Integer to java.lang.String
  CratonVM  NoSuchMethodError: 'boolean java.lang.Integer.isEmpty()'

len = findVirtual(String, "length", ()int)
len.invoke((Object) Integer.valueOf(3))
  HotSpot   ClassCastException: Cannot cast java.lang.Integer to java.lang.String
  CratonVM  NoSuchMethodError: 'int java.lang.Integer.length()'
```

**The messages name the mechanism.** The `Integer` reached the callee and the
callee's own body then dispatched `isEmpty()` / `length()` on it — so the error
names a method the caller never wrote, at a frame the caller never entered. The
second row shows the RECEIVER was uncast too, not only the parameters.

`NoSuchMethodError` is not a smaller version of the same behaviour. It extends
`Error`, so a `catch (ClassCastException)` — or any `catch (RuntimeException)`
around a reflective dispatch, which is how frameworks wrap one — does not see
it.

`bindTo` accepted the mismatch outright and handed back a handle that looks
perfectly well-typed:

```text
len.bindTo(Integer.valueOf(1))
  HotSpot   ClassCastException
  CratonVM  a MethodHandle of type ()int
```

## 2. Why it was open, and what actually changed

W7-19 §5.1 deferred the `bindTo` cast with an argument worth keeping:

> A `cast` check is an assignability question, and the only predicate
> `NativeContext` offers is `is_subclass`, which answers FALSE for a fabricated
> stand-in against a real JDK interface. `bindTo` is on the Groovy-indy /
> SpEL-FunctionReference / log4j-provider path in this tree, all of which bind
> interfaces and subtypes, so that false negative would be a FALSE
> `ClassCastException` on a hot path — a refusal of working code, which is worse
> than the wrong answer it replaces. **It wants a lane that can run those
> workloads.**

The risk has not gone away; what changed is the ability to price it.

* `probes/CodegenFrameworkSmoke.java` now boots **Groovy, ByteBuddy, Mockito,
  ASM, Javassist and Objenesis as themselves** — 16 rows, 0-diff in both modes.
  That is the lane §5.1 asked for.
* `probes/InvokeCastSweep.java` puts the hazard *in the instrument*: **14 of its
  29 rows exist only to assert the check does NOT fire.**

## 3. The probe is two halves and the second half is the point

| half | asserts | rows |
| --- | --- | ---: |
| **defect** | the cast fires | 7 |
| **hot path** | it does **not** fire on shapes that must keep working | 14 |
| **limit** | cases the predicate deliberately does not judge | 2 |

The hot-path half covers subclass receiver and argument, interface-typed
parameters, a lambda passed as a functional interface, a lambda BOUND to one,
nulls in every position, `Object`-typed parameters, a varargs collector, an
`asType`-retyped handle, boxed primitives, and five ADAPTERS —
`filterArguments`, `insertArguments`, `dropArguments`, `filterReturnValue` and a
double `bindTo`.

The adapters were the ones worth writing down. An adapter keeps the LEAF
member's descriptor in `MH_DESC` while presenting a different parameter list to
its caller, and `filterArguments(cat, 1, intToString)` presents the **same
arity** — so an arity guard cannot see it, and a cast check reading the leaf
descriptor would refuse a correct call. The fix is gated on
`kind_has_authoritative_type`, the same gate `invoke_narrowing_arg_refusal`
already uses.

## 4. One shared predicate, not two

`reference_arg_admitted` answers `Some(false)` only on a POSITIVE mismatch;
every "cannot tell" is an allow:

| clause | why it declines to answer |
| --- | --- |
| null value, or a primitive | not the question |
| `Ljava/lang/Object;`, or an array parameter | accepts anything we would reason about |
| target class does not resolve | we know nothing |
| value is a lambda proxy, a `$Proxy`, or a VM-minted stand-in | its interfaces are acquired at RUNTIME and exist in a table, not the hierarchy |

`is_subclass` and `synthetic_implements_declared` supply the positive answers.
`bindTo` and `invoke` both call it — writing the judgement twice is how two
doors come to disagree about one object, which is the defect species this
campaign has now met on `getPackages()`, on `ModuleFinder`, and on the two
producers of `SnapshotEnumeration`.

### …and this predicate is now the THIRD spelling of one rule

Worth stating plainly, because the campaign keeps finding this shape and this
change adds an instance of it rather than removing one:

| spelling | doors it serves | interface case |
| --- | --- | --- |
| `lang_class::coerce_arg_strict_msg` | `Method.invoke`, `Constructor.newInstance`, `Field.set` | **excluded**, same reasoning, plus a `near`-loader refinement and `argument_reaches_expected_by_name` (SUPERCLASSES only) |
| `lang_invoke::reference_arg_admitted` (new) | `MethodHandle.invoke`, `bindTo` | **excluded** |
| `typecheck::aastore_element_assignable` | the `aastore` opcode, `jit_aastore`, `Array.set` | **judged correctly**, via `ClassManager::is_assignable_to_name` plus proxy / lambda-proxy hatches |

The three agree on the easy cases and the first two share a hole the third does
not have — which is exactly why `x.interfaceArgWrong` and
`ReflectArgTypeSweep`'s `m.interfaceWrong` are the same row twice. The
consolidation is one change: expose `is_assignable_to_name` on `NativeContext`
(as `synthetic_implements_declared` and `aastore_element_assignable` already
are), carry the proxy hatches with it, and let the two reflective spellings call
it. Two of the three producers were written independently and neither knew about
the walk the third already had.

## 5. The interface case was NOT a limit, and closing it broke FFM

The first round shipped with the interface case excluded and
`x.interfaceArgWrong` recorded as a deliberate, measured limit. That was one
lookup short: `ClassManager::is_assignable_to_name` — loader-blind, walking
supers AND interfaces by name, cycle-safe and depth-capped — was already what
`typecheck::aastore_element_assignable` uses for exactly this case, and had no
door onto `NativeContext`. It has one now, and it closed both
`x.interfaceArgWrong` and `ReflectArgTypeSweep`'s `m.interfaceWrong`.

**And it immediately refused working code.**

```text
probes/P1RemainingSweep.java   ffm.downcallHandle.strlen
  before   (MemorySegment)long
  after    throws java.lang.IllegalArgumentException: argument type mismatch
```

`Linker.defaultLookup().find("strlen")` hands back a
`cratonvm.internal.foreign.MemorySegmentImpl`. It really IS a
`java.lang.foreign.MemorySegment`, but the VM mints the class, so it declares no
interfaces and the relationship lives only in the interpreter's
`synthetic_implements` table:

| predicate | answer about that object |
| --- | --- |
| `is_subclass` | false — a ClassId compare over a DAG it is not in |
| `class_assignable_to_name` | false — a by-name walk over supers and DECLARED interfaces |
| `is_class_synthetic_stub` | false — that is about registered synthetic-stub NATIVES, a different concept |
| `synthetic_implements_declared` | **true** — the only door that knows |

The sibling predicate in `lang_invoke.rs` was unaffected because it happened to
call that door. The one in `lang_class.rs` got a hand-written hatch list
instead — proxy names, lambda-proxy ids, `is_class_synthetic_stub` — which is
three of the four populations and the wrong three.

**Nothing in the new probe could have caught it.** `InvokeCastSweep` and
`ReflectArgTypeSweep` were both green; `P1RemainingSweep`, from a lane closed
the day before, is what went red. That is the handoff's own rule paying for
itself: *re-run your family's existing probes on the final binary, not only the
ones you wrote.* A new probe asks the questions its author thought of, and an
FFM carrier was not one of them.

`ReflectArgTypeSweep` now carries `k.downcallHandle` and
`k.arenaSegmentToInterfaceFormal`, so the guard lives beside the defect.

The one row that IS a limit:

```text
x.arrayArgWrong   Arrays.toString(int[]) invoked with a String
  HotSpot ClassCastException   CratonVM ClassCastException   <- already correct
```

Array parameters are still not judged by this predicate — they did not need to
be.

## 6. What else the same question turned up

Asking it of the OTHER reflective doors — `probes/ReflectArgTypeSweep.java`, 63
rows — found `VarHandle` checking neither its receiver nor its value, including
an `Integer` stored into a `String`-declared field with no error and a read
through a receiver of the wrong class that RETURNED a value. Recorded
separately: `varhandle-checks-neither-its-receiver-nor-its-value-20260830.md`.

53 of those 63 rows are 0-diff, and the passing list is what makes the failing
one specific: `Method.invoke`, `Field.set`/`get`, `Constructor.newInstance` and
`Array.set` all refuse correctly across wrong references, wrong receivers,
arity, primitive widening/narrowing/null and `final` fields.

## 7. Verification

```text
probes/InvokeCastSweep.java          29 rows,  7 differing -> 0, both modes
probes/L5ModuleInvokeSweep.java     125 rows,  1 differing -> 0
probes/L8InvokeLookupSweep.java      83 rows,  1 differing -> 0
probes/ReflectArgTypeSweep.java      65 rows,  6 differing -> 5 (all VarHandle, recorded)
probes/P1RemainingSweep.java         29 rows,  0 differing -> 1 -> 0   (the FFM regression)
probes/CodegenFrameworkSmoke.java    16 rows,  0 differing, both modes
probes/DynClassGenSweep.java         42 rows,  0 differing, both modes
probes/ModuleFinderProbe.java        11 rows   probes/ModuleLayerSetsProbe.java  10 rows
probes/NeverNullAccessorSweep.java   63 rows   probes/L3ViewItrSweep.java        22 rows
```

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only -cp probes/out InvokeCastSweep
```
