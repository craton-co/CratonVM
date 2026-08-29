# The `java.lang.invoke` lookup surface — ten defects, and one that rewrote an interned type

**Status: COMPLETE 2026-08-29.** Worktree `h2-known-issues-206dee`, branch
`claude/jdk-only-mode-handoff-09b48c`. Phase 2 long tail; the seven named lanes
of `HANDOFF-20260828-SCOPE.md` are all closed and this is from the ~780 rows
none of them owned.

## 1. Picking the lane

From a live `--jdk-only-report`, filtered on `outcome == "native-won"` as §3 of
the scope doc prescribes:

```text
java/lang/invoke/MethodHandles$Lookup   6 triples
java/lang/invoke/MethodType             5
java/lang/invoke/MethodHandle           3   (type, asType, bindTo)
java/lang/invoke/MethodHandles          1   (lookup)
                                       ---
                                        15
```

Distinct from the DISPATCH surface `L5ModuleInvokeSweep` already closed
(`invoke`/`invokeExact`): nothing here calls a handle. Every row asks what the
lookup or the type object ANSWERS, and most ask what it REFUSES.

`probes/L8InvokeLookupSweep.java`, 83 rows, both modes, HotSpot 25.0.3+9.
**27 differing rows at the start, 1 at the end**, identical in Compatible and
`--jdk-only` throughout — so every one an ordinary defect, none a mode defect.

## 2. The one that was not a refusal

```text
MethodType mt = methodType(int.class, String.class, long.class);
mt.parameterArray()[0] = int.class;

  mt                    HotSpot (String,long)int   CratonVM (int,long)int
  mt.parameterType(0)           java.lang.String            int
```

`parameterArray()` returned the LIVE `ptypes` array. The JDK's entire body is
`return ptypes.clone();` and the clone IS the contract: `MethodType` is
specified both immutable and interned, so handing back the live array lets any
caller rewrite a type other callers already hold. One write, and the type is a
different type for the life of the VM.

### It cost six rows that were not defects

`toString`, `descriptorString`, `changeReturnType`, `appendParameterTypes`,
`wrap` and `unwrap` all read the corrupted array afterwards, and all six showed
up in the first diff. My first reading counted them as separate defects.

The tell was incoherent: `mt.toString()` was wrong while `mt.parameterType(0)`
on the SAME object was right. That only resolves once you notice the probe's own
`isCopy` row runs BEFORE `toString` and is the thing doing the mutating — the
instrument was corrupting the subject and then measuring it.

**Shrinking is what separated them.** Four lines reproduced it; the 83-row probe
could not have, because inside it the corruption is upstream of half the rows.
A diff tells you how many rows differ, never how many defects there are.

## 3. The refusals — nine, in three families

### The finders did not check static-ness, type or finality

```text
findStatic(Target, "instanceM", (int)int)   HotSpot IllegalAccessException  CVM (int)int
findVirtual(Target, "staticM",  (int)int)           IllegalAccessException      (Target,int)int
findGetter(Target, "pubField", String.class)        NoSuchFieldException        (Target)String
findGetter(Target, "staticField", int.class)        IllegalAccessException      (Target)int
findStaticGetter(Target, "pubField", int.class)     IllegalAccessException      ()int
findSetter(Target, "finalField", int.class)         IllegalAccessException      (Target,int)void
```

`IllegalAccessException`, not `NoSuchMethodException`: the member IS found and
the ACCESS is what is refused, and a caller that distinguishes the two learns
different things from them. The `findSetter` row is the sharpest — a handle that
writes a `final` field.

### `findConstructor` validated nothing at all

```text
findConstructor(Target, methodType(Target.class, int.class))  NoSuchMethodException  (int)Target
findConstructor(Target, methodType(void.class, String.class)) NoSuchMethodException  (String)Target
findConstructor(Runnable.class, ()void)                       NoSuchMethodException  ()Runnable
findConstructor(int.class, ()void)                            NoSuchMethodException  ()int
```

Every other finder in this file grew an existence check — `lookup_require_method`
carries the note about the `catch (Exception)` version probes that need one.
This one was missed, so it handed back a working-looking handle for a
constructor the class does not declare.

It also OVERWROTE the return type it was given: whatever you passed was
truncated and `V` appended, so asking with `methodType(Target.class, int.class)`
silently became the `(int)void` lookup instead of the refusal HotSpot gives.
That masked the missing existence check underneath it.

### A null argument was reported as an absent member

```text
findVirtual(null, "m", mt) / (C, null, mt) / (C, "m", null)
  HotSpot   NullPointerException
  CratonVM  NoSuchMethodException        (and NoSuchFieldException for the field finders)
```

**The direction is the defect.** Both wrong answers are CHECKED exceptions that
version-probing code catches ON PURPOSE. So a null slipped past the probe's
guard and came back as "this JDK does not have that method" — a wrong conclusion
the caller then acts on — instead of a stack trace at the line with the bug.

### And two on `MethodType` itself

```text
mt.parameterType(-1)                        HotSpot AIOOBE   CVM NullPointerException
methodType(int, String, (Class[]) null)      HotSpot NPE      CVM built (String)int
```

The first is a `-1 as usize` — 18 446 744 073 709 551 615 — handed to an array
read that answered null, so the caller's eventual NPE came from somewhere else
entirely. The second's own comment said "morePtypes may be null or an array";
HotSpot runs the whole array through `checkPtypes`, which dereferences it.

## 4. The rule every refusal follows, and the two that are exempt

Refuse only on a POSITIVE reading: the class resolved, the member was
enumerated, and it mismatched. "Could not see it" stays an accept, because this
VM substitutes and synthesises JDK classes whose declared members it does not
always model — the rule `lookup_require_field` already documents.

**Two rows needed the exemption, and finding that out took a measurement.** The
evidence rule cannot reach `findConstructor(int.class, …)` or
`findConstructor(Runnable.class, …)`: neither `int` nor an unloaded
`java/lang/Runnable` resolves by name, so both sailed through it and returned
handles. An interface, a primitive and an array declare no constructor EVER, on
any image — that is a property of the KIND, not of what this VM models, so those
three are refused unconditionally.

"We could not see it" and "it cannot exist" look identical in a failing lookup
and deserve opposite treatment.

## 5. Verification

```text
probes/L8InvokeLookupSweep.java   83 rows   27 differing -> 1, both modes
probes/L5ModuleInvokeSweep.java  125 rows   1 differing (unchanged, no regression)
probes/L3ViewItrSweep.java        22 rows   0 differing
```

## 6. The residual

`bindTo` with a wrong REFERENCE type is `ClassCastException` on HotSpot and
answers a handle here. `bindTo`'s own in-tree note records why the fix is not
safe yet: the JDK's body is `type.leadingReferenceParameter().cast(x)`, an
assignability question, and the only predicate `NativeContext` offers is
`is_subclass`, which answers FALSE for a fabricated stand-in against a real JDK
interface. `bindTo` sits on the Groovy-indy / SpEL / log4j paths where
interfaces and subtypes are the norm, so that false negative would be a false
`ClassCastException` on a hot path — a refusal of working code, which is worse
than the wrong answer it replaces.

Same predicate blocks the one residual in
`L5-residuals-module-packages-and-invokeexact-20260828.md` §7. Two lanes now
want the same accessor; whoever adds it closes both.

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only -cp probes/out L8InvokeLookupSweep
```
