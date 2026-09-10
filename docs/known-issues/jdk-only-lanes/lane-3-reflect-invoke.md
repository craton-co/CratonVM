# Lane 3 — core reflection and `java.lang.invoke`

**Scope: 251 §1.4 shadows over 36 classes, from 246 registration sites.**
Prefixes: `java/lang/reflect/`, `jdk/internal/reflect/`, `sun/reflect/`,
`java/lang/invoke/`.

Read [`lane-0-integration-and-gates.md`](lane-0-integration-and-gates.md) §2-§6 first. Method, preconditions and
landing protocol: [`../jdk-only-lane-operations.md`](../../contributing/jdk-only-lane-operations.md).

---

## 1. Shape of the lane

```text
  35  java/lang/reflect/Field                 11  java/lang/invoke/MethodHandle
  33  java/lang/reflect/Method                11  sun/reflect/generics/...
  25  java/lang/invoke/MethodHandles$Lookup        TypeVariableImpl
  23  java/lang/invoke/MethodHandles           9  java/lang/invoke/MethodType
  23  java/lang/reflect/Constructor            7  java/lang/invoke/MemberName
  14  java/lang/reflect/InaccessibleObjectException  <- lane T
  12  java/lang/reflect/InvocationTargetException    <- lane T
```

Almost one registration site per row: this lane is a long tail of hand-written
registrations, not a few parameterised loops. That makes waves smaller and
individually cheaper than L1's or L4's, and it means the source-scanning drift
gate can actually see your work — unusually for this campaign.

`InaccessibleObjectException` and `InvocationTargetException` are lane T's
throwable registrar. Not yours.

## 2. Your first dependency is already satisfied — check it

Reflection lookups compare **binary** names. `java/lang/Class.getName` was
returning the internal slash form whenever the native yielded
(`java/lang/Object` instead of `java.lang.Object`), silently, and it propagated
into every JDK name comparison. It is now a reviewed `Intrinsic` (L0 §7), which
took `ClassNameSweep` from 24 diffs of 24 to 2.

**Re-confirm this on your tree before diagnosing any name-shaped failure here**,
and if you see a slash in a name, suspect a *different* accessor rather than
re-deriving the same finding.

The residual worth knowing: with the shadow dial armed,
`Class.forName(Nested.class.getName())` still throws
`ClassNotFoundException: ClassNameSweep$Nested`. That is `forName0` declining at
a dispatch door — and `forName0` **is** `ACC_NATIVE` in the image, so contract
§1.5 makes `Bridge` correct for it. It is a dial artefact, not a retirement
target.

## 3. The three instrument traps that have already burned this area

These are recorded findings, not hypotheticals. Every one produced a wrong
conclusion first.

- **`getDeclaredFields` on `java.lang.reflect.*` returns 0 on a healthy image.**
  Core reflection deliberately hides its own fields. A reflective field walk
  across this package reports ABSENT at every link on a VM that is working
  perfectly. Do not use a field walk as your instrument here.
- **A skip list can hide the real caller.** `ReflectionFactory` calls
  `setAccessible` from a package the caller-sensitive skip list hides, so the
  apparent caller is not the real one. **Print the throw stack first**, before
  theorising about which access check fired.
- **A bare exception-type assertion cannot say which check fired.**
  `InaccessibleObjectException` has several distinct sources; assert on the
  message or the frame, not the type.

## 4. `MemberName`, `MethodHandle`, `MethodType`: expect reviewed `Intrinsic`s

`java/lang/invoke/MemberName` (7) and `MethodHandle` (11) are the most
VM-coupled rows in the campaign after `Class`. `MemberName` in particular
carries fields (`clazz`, `name`, `type`, `flags`, `method`, `resolution`) that a
real JVM fills during `MethodHandleNatives.resolve`. Where a field is written
only by a VM and this VM's layout differs, §1.4's remedy returns null or a wrong
value — which is the reviewed-`Intrinsic` case, not a retirement.

Follow L0 §7's protocol exactly, and note the cost it names: an `Intrinsic` is
exempt at every dispatch door **and** exempt from the census by construction, so
tagging removes the row from the population the dial can ever ask about. Report
all three numbers (unarmed / yielded / tagged) or leave it a `Bridge`.

`MethodHandles$Lookup` (25) and `MethodHandles` (23) are different: much of
their surface is ordinary Java over the JDK's own checks, so they are plausible
straight retirements. Probe the *access-control* answers — `lookupModes`,
`privateLookupIn`, a cross-module `findStatic`, `unreflect` on a
non-accessible member — because that is what the JDK's bytecode gets right and a
hand-written native tends to approximate.

## 5. `Field`/`Method`/`Constructor` — 91 rows, one shared story

They share `AccessibleObject` (bucket B) and a copy-on-`getDeclaredX` model:
the JDK hands out *copies* with a shared root, and `setAccessible` writes the
`override` flag on the copy. Two probe requirements follow:

- **Ask the same object twice.** `getDeclaredField("x") == getDeclaredField("x")`
  is `false` on HotSpot, and `.equals` is `true`. A retirement that changes
  copying changes both answers; print both.
- **`setAccessible` then read, on a fresh copy.** The flag must not leak between
  copies, and must survive on the one you set it on.

Also print `getGenericType`/`getGenericParameterTypes` for at least one generic
member: `sun/reflect/generics/reflectiveObjects/TypeVariableImpl` (11 rows) is
reached only through those paths, and it is otherwise untested territory.

## 6. Coordination

- **L0** owns `java/lang/Class`. Most reflection entry points route through it;
  if a failure resolves to a `Class` row, hand it to L0 rather than tagging it.
- **L7** owns the loader and its bootstrap failures. `setAccessible` and module
  access answers depend on module state that L7 may still be repairing —
  re-price after any L7 landing.
- **Lane T** owns the two throwable classes in your prefix.

## 7. The increment loop

1. Funnel from a dump: owns slot, kind `Bridge`, image `Code`, `invocations > 0`
   in **your** instrument's run.
2. Probe + HotSpot oracle. No build needed.
3. Fill `RETIRED_SHADOW_L3_TRIPLES`, sorted and unique. Note that
   `java/lang/reflect/`, `java/lang/invoke/`, `jdk/internal/reflect/` and
   `sun/reflect/` must be present in `RETIRED_SHADOW_PREFIXES` — an entry
   outside every prefix silently answers "not retired" and is invisible in a
   workload.

   **This lane adds its own prefixes; L0's skeleton commit does not exist.**
   That plan was dropped — see L0 §4, "How a lane adds its table" — because
   pre-adding a broad prefix keeps every test green while deleting the second
   guard `a_prefix_alone_retires_nothing` provides. Add the narrowest prefixes
   that cover your table, in the same commit as the table.
4. Build token (L0 §5); one build per wave.
5. `N refusals, 0 survivors`.
6. Probe-tree A/B, `--jdk-only` corpus, `SUITE=all` at `TIMEOUT=600`, `all`-arm
   count.
7. Full gate set. Kind-map rows. Commit. Do not push.

## 8. Measured, 2026-09-10

### The population is 242, not 251

Re-derived from a `--dump-native-registry` taken with `--explain-jdk-only`,
which is what populates the image columns (see the trap below), minus lane T's
two throwable classes:

```text
owns_slot + kind=bridge in the prefix set   346 rows / 38 classes
  A  declared with Code   212       D  ACC_NATIVE (§1.5: Bridge is CORRECT)  73
  B  inherited Code        30       C  declared, no Code                     21
                                    E  class absent 3      F  method absent 7
A+B, the goal's population  242 rows / 35 classes
```

`Field 35 + Method 33 + Constructor 23 = 91` reproduces §5 exactly, so the top
of §1's table was sound; the 251 came from a different tree. **D is 73 rows** —
nearly a third of the prefix set is `ACC_NATIVE` in the image, where §1.5 makes
a `Bridge` correct and there is nothing to retire.

### Instrument

`apps/probes/L3ReflectInvokeSurface.java`, **254 rows**, oracle deterministic
across two runs. **150 of the 242 dispatched** in its own run, so precondition 4
is satisfied for 150 and open for 92 (lane 0's probe reached 74%; this reaches
62%).

### 34 rows disagree with HotSpot UNARMED — before any retirement

These are live defects, not retirement candidates. Note the unit: `arm3.sh`
prints `d(hs,base)=58`, which is diff LINES at two per changed row. **34 rows.**

**Four rows where the VM grants access HotSpot refuses — but only ONE of them
is news.** Read the code before filing any of these:

| row | HotSpot | this VM | status |
|---|---|---|---|
| `setAccessible` leaks to a fresh handle | `false` | **`true`** | **defect** |
| `Lookup.unreflect` on a private method | `IllegalAccessException` | NO-THROW, working handle | documented residual |
| `Lookup.findVirtual` on a private method | `IllegalAccessException` | NO-THROW | documented residual |
| `privateLookupIn(java.base)` | `IllegalAccessException: does not open` | NO-THROW, modes 31 | documented residual |

The three residuals are **one root cause and it is deliberate.** Both guards
short-circuit identically —

```rust
// native-builtins/src/lang_invoke.rs:6520 (find*) and :6744 (unreflect*)
if (modes & LK_MODE_PRIVATE) != 0 {
    return Ok(());
}
```

— and the PRIVATE bit means "may reach privates of its own lookup class/nest",
not "may reach any private anywhere". `lk_enforce_find_access`'s own doc
comment states the omission and the reason: *"nestmate relationships,
`protected`-receiver rules, module `exports`/`opens` — is NOT enforced… it can
only admit something HotSpot would refuse, never refuse something HotSpot
admits"*, because `lookupClass` comes from a stack walk and a wrong answer
there must never become a refusal.

So this lane's contribution here is a **measurement of a residual the code
describes in prose**: a concrete reproducer (`L3Foreign`, a non-nestmate), and
confirmation that the one-directional claim holds — every one of these admits
what HotSpot refuses, and none refuses what HotSpot admits.
`W4-1-publiclookup-allowedmodes-never-checked.md` is **not** wrong to say the
gate is CLOSED; it is closed for the PRIVATE-bit-clear case it gates.

**The leak is the genuine defect,** and it has nothing to do with those guards.
It follows from §5's copy model failing: `getDeclaredField("x") ==
getDeclaredField("x")` is **`true`** here and `false` on HotSpot, for `Field`,
`Method` and `Constructor` alike. The VM hands back the *same* object, so one
`setAccessible(true)` grants access to every holder of that member. §5
predicted the pairing; it is now measured. And the copy rows classify
**`BAD -> OK`** — *yielding to bytecode repairs the copy model*, so this
defect is closed by the retirement rather than blocking it.

**`java.lang.reflect.Proxy` is down entirely** (5 rows): every row fails with
`IllegalArgumentException: L3ReflectInvokeSurface$Iface referenced from a
method is not visible from class loader`. Re-price after L7.

**Nine `MethodHandles` combinators are broken:** `zero` (`InternalError: Failed
to link speciesData to speciesCode`), `empty` and `countedLoop`
(`NoClassDefFoundError: java/lang/invoke/BoundMethodHandle`), `arrayLength` and
`arrayConstructor` (`IllegalArgumentException: not an array: class [I` — they
refuse a genuine array class), `spreadInvoker`/`exactInvoker`/`invoker`
(return **null** instead of the value), `throwException` (`NPE: cannot invoke
MethodTypeForm.basicType() because this.form is null`).

The rest are shape: `MethodHandle.toString` and `Lookup.toString` drop their
type/lookup-class, `NPE` messages are `null` where HotSpot is helpful, and
three access rows throw from the call site rather than from
`Reflection.newIllegalAccessException` — which is §3's caller-skip-list area,
visible only because `access()` prints the throwing frame.

### Two censuses the armed run printed for free

```text
descriptor-coercion: total=105 primitive-into-reference[read=105]
  -- field reads whose value contradicted the slot's descriptor and was DESTROYED
JVMS 6.5 uninstantiable-receiver: java/lang/invoke/MethodHandle (abstract, lang_invoke.rs:10776)
                                  java/lang/invoke/VarHandle   (abstract, lang_invoke.rs:2946)
```

The first explains the yielded `Field.getX` family answering `0`/`NaN`/`0.0`
across the board: 105 destroyed reads, per
`docs/known-issues/.../G30-1-the-silent-reference-slot-coercion-20260817.md`.
The second is a native handing back an instance of an ABSTRACT class, which no
bytecode could have produced.

### Retirement: blocked on the armed arm, and why

**Both halves of this lane aborted the VM under the dial, at different rows —
and it is ONE defect, not two.**

```text
reflect/ armed   dies row 126  Lookup.unreflect     invoke.rs:2845
invoke/  armed   dies row 166  MethodHandle.bindTo  invoke.rs:2845
```

Same line, same message: `start byte index 1 is out of bounds for string of
length 0` — the unguarded tail slice in `split_method_descriptor_ref`. Both
halves reach it with an empty descriptor by different routes. **Fixed** in the
preceding commit; a malformed descriptor must not be able to take the VM down.

This was very nearly written up as two independent blockers, on the strength of
two different failing rows in two different packages. Two crashes at two call
sites is not two bugs until you have read the panic site of each — and here the
second stderr capture cost one command and removed a whole line of enquiry.

Arming the two halves separately is still the right method while the counts are
being taken, because the first whole-prefix run scored `delta=-58` — which
reads as a spectacular improvement and was the artefact described below.

### Three instrument defects found and fixed, all of which produced a wrong number

1. **A crashing arm scored as a PERFECT match.** `Field.getChar` yielded `0`,
   the probe printed it raw, and one NUL byte made `diff` answer `Binary files
   ... differ` — a single line matching neither `^<` nor `^>`, so
   `grep -c '^[<>]'` returned **0**. Only the line-count guard caught it, and it
   would not have caught a full-length run with a NUL in it. `arm3.sh` now
   passes `-a` and counts NULs per arm; `scrub()` escapes every control
   character, so a probe can no longer emit a byte its own comparator chokes on.
2. **Six access-control rows were vacuous.** They targeted `Holder`, a NESTED
   class — a nestmate, whose private members `main` may read with no
   `setAccessible` at all. The oracle said so itself: `private read without
   setAccessible` answered `NO-THROW 13` on **HotSpot**. A row where the oracle
   does not throw is not measuring an access check. The fixture is now
   `L3Foreign`, a sibling top-level class, and it found three of the four
   missing checks above on its first run.
3. **The funnel bucketed 346 rows as "class absent from the image"** when the
   dump simply had not run the adjudication pass. The schema publishes
   `image_adjudication` at the top level precisely so a null column is never
   ambiguous, and the funnel ignored it. It now refuses to bucket an
   un-adjudicated dump instead of answering confidently.

### Interface carriers: 8 of 9 never dispatch, and one does

Nine registrations name a class the JDK does not treat as the declaring class
of the method — `TypeVariable` (6), plus `.equals` on `GenericArrayType`,
`ParameterizedType` and `WildcardType`. The oracle prints the resolution rather
than asserting it: `TypeVariable.equals` resolves to `TypeVariableImpl`,
`getTypeName` to `Type`, `ParameterizedType.equals` to `ParameterizedTypeImpl`.

The tempting conclusion is that a door asking about the declaring class can
never ask about the interface, so all nine are dead. **That conclusion is
wrong, and the data says so twice.** `GenericArrayType.equals` counted `inv=1`
while its three siblings counted zero, and the wider pattern is worse for the
rule:

```text
Field.canAccess       inherited from AccessibleObject   inv=6   FIRES
Field.setAccessible   inherited from AccessibleObject   inv=0   never
```

Two methods, one carrier, one bucket, opposite outcomes — and the probe calls
both many times. So a registration on a class that merely INHERITS the method
can fire in this VM; whether it does is decided per method by something this
lane has not identified. **8 of 30 bucket-B rows dispatched, 22 did not.**

Do not delete anything on the heuristic. What lane 3 can honestly hand over is
the measurement: 92 of the 242 never dispatched under an instrument that calls
many of them (70 bucket-A, 22 bucket-B), and the `canAccess`/`setAccessible`
pair is the cheapest reproducer for whoever owns dispatch routing.

`Executable.getParameters` is **not** in the dead set on any reading — the
oracle resolves it to `java.lang.reflect.Executable` and it counted `inv=1`
(`Constructor`) and `inv=3` (`Method`).

---

## 9. Done

Every bucket-A/B row in the prefix set is retired, classified as C/D/E/F, a
reviewed `Intrinsic` with its probe, or blocked with the blocker named — with
the `invoke` package's VM-coupled rows explicitly separated from the ones that
were genuinely retirable.
