# Lane 3 — core reflection and `java.lang.invoke`

**Scope: 242 §1.4 shadows over 35 classes** — re-measured 2026-09-10 from
an adjudicated `--dump-native-registry`; the 251/36 this line carried came
from a different tree and counted rows lane T owns. **24 retired, 48
(class, name) pairs deferred with reasons** — see §8.
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
- **L7** owned the loader and its bootstrap failures, and **has landed and
  retired** (2026-09-10): the builtin loader hierarchy links, and the null
  `java.lang.Module` family was already closed by L0's `Class.getModule` tag.
  So the module state `setAccessible` depends on is no longer moving under you —
  re-price once against the
  [`the-builtin-classloader-could-not-link-and-getname-was-never-tagged-20260910`](../jdk-only/the-builtin-classloader-could-not-link-and-getname-was-never-tagged-20260910.md) record, then treat it as
  stable. Anything whose remedy is a `Class` row is L0's.
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

**And the split is exactly where the code says it is, which is the useful
result.** The same non-nestmate target, through core reflection instead of
`Lookup`, is *correctly refused*:

```text
FG private field read without setAccessible        IllegalAccessException  BOTH VMs
FG private method invoke without setAccessible     IllegalAccessException  BOTH VMs
FG private constructor without setAccessible       IllegalAccessException  BOTH VMs
```

Only the throwing frame and the message wording differ — ours constructs the
exception at the call site, HotSpot in `Reflection.newIllegalAccessException`.
So [`../jdk-only/L15-nestmate-access-field-and-constructor.md`](../jdk-only/L15-nestmate-access-field-and-constructor.md)
is **independently corroborated** by this instrument: its
`caller_may_access_member` funnel fires on all three of the field, method and
constructor paths it claims to cover.

Core reflection checks the caller; `java.lang.invoke` deliberately does not.
That is one sentence to hand to whoever closes the `Lookup` residual, and it
took a non-nestmate fixture to say it.

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

**Nine `MethodHandles` combinators answer wrongly:** `zero` (`InternalError:
Failed to link speciesData to speciesCode`), `empty` and `countedLoop`
(`NoClassDefFoundError: java/lang/invoke/BoundMethodHandle`), `arrayLength` and
`arrayConstructor` (`IllegalArgumentException: not an array: class [I` — they
refuse a genuine array class), `spreadInvoker`/`exactInvoker`/`invoker`
(return **null** instead of the value), `throwException` (`NPE: cannot invoke
MethodTypeForm.basicType() because this.form is null`).

**Reconcile these against
[`../jdk-only/W7-19-methodhandles-compatible-residuals.md`](../jdk-only/W7-19-methodhandles-compatible-residuals.md)
before filing any of them as new.** That page is the live home for
`MethodHandle` residuals and already carries the adjacent hazards — notably
that `MethodHandles.empty`/`zero` allocate **17** slots while
`lang_invoke::alloc_method_handle` and `classloader::alloc_method_handle`
allocate 21 with *different field orders*, so a bare field read is
out-of-bounds on three of the four populations. Two of the rows above are
`empty` and `zero`. Lane 3's contribution is the measurement against a
current binary and a probe row per combinator; the diagnosis belongs on W7-19.

This is the second time in this lane that a row which looked like a fresh
defect was an area someone had already mapped. The access-control rows below
were the first. **One grep of `docs/known-issues/` per finding**, before the
write-up and not after.

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

### Wave 1 is CORPUS-CLEAN, and that is the number that counts

```text
--jdk-only, 132 vectors, lane L3's 24 the only retirements in the binary
    132 passed, 0 failed
```

Worth stating plainly because the route here was expensive. Lane **L0**'s wave
shipped 54 retirements validated only by the shadow dial, and the same arm came
back **97 of 132**. Two withdrawal rounds (-16, then the whole
`java/lang/Module` family) got it to 127 and four vectors still failed, so L0's
table was emptied outright. With it gone and lane 3's 24 the only retirements
left, the arm is 132/0 — which attributes every one of those failures to L0's
`Class` family and none to this lane.

**The lesson this lane inherited, and the reason its own table survived
contact with the corpus:** a dial arm is a screening instrument, not evidence.
Wave 1's 24 were each required to pass four rejection rules (`invocations > 0`,
no `OK -> BAD` row, no `BAD -> BAD` row, and at least one row whose correct
answer differs from the family's default), and 48 (class, name) pairs were
rejected by them. L0's wave applied none of the last three. See the ops page's
new "what skipping the two-binary step actually cost" section.

### The armed arm, and wave 1: 24 retired of 242

With the descriptor panic fixed, all three arms complete on one run of the
whole prefix set — `rc=0/0/0`, 255/255/255 lines — which confirms the two
aborts were one defect.

```text
d(hs,base) 68 diff lines    d(hs,armed) 224    delta +156

per ROW (254)   OK -> OK   132   retire
                BAD -> OK   10   yielding FIXES it -> retire
                OK -> BAD   88   hold
                BAD -> BAD  24   investigate
```

**142 of 254 rows are retirable behind a `delta` of +156.** The aggregate says
"retire nothing" for the second lane running.

The ten `BAD -> OK` rows are the argument for doing this at all:

```text
45/82/103  F/M/C copy == is false        yielding REPAIRS the copy model
251        setAccessible does not leak   yielding fixes the LEAK that follows
133        privateLookupIn(java.base)    yielding restores IllegalAccessException
246        private field read            yielding gives HotSpot's exact frame
134 accessClass · 138 LK toString · 36 NPE message · 236 getCallerClass
```

So the retirement **closes** the `lk_enforce_*` access-control residual rather
than merely measuring it, and closes the `setAccessible` leak with it.

**`RETIRED_SHADOW_L3_TRIPLES` — 24 triples**, core reflection's metadata
accessors on `Field`, `Method` and `Constructor` plus three `MethodType`
accessors. Prefixes added: `java/lang/reflect/` and `java/lang/invoke/` only —
`jdk/internal/reflect/` and `sun/reflect/` are lane 3's too and are
deliberately **absent**, because wave 1 retires nothing under them and a
prefix with no table entry behind it is exactly what L0 §4 warns about.

Ratchets moved by **+24 in all three configurations** (1939→1963, 1950→1974,
1939→1963) and the kind map amended **24 rows for 24 triples, 0 missed** — a
clean 1:1, unlike L0's 54 triples over 56 registrations.

### Why the other 218 are not in the table

A row is not a triple. Only triples a row-to-triple mapping can justify are in
it: the tag names one method of one class, the census holds exactly one A/B
triple for that name, `invocations > 0` in this probe's own run, and **every**
attributed row is `OK -> OK` or `BAD -> OK`. 48 (class, name) pairs were
reached and rejected, each with its reason recorded. Four rules did the work,
and three of them removed something that had looked clean:

* **`invocations == 0`** — precondition 4, per triple, from the dump. 92 of
  242.
* **held** — any `OK -> BAD` row. 88 rows, and they cluster: **32 are
  `Field`'s primitive accessors**, which the same run explains — the
  descriptor-coercion census reports **105** field reads whose value
  contradicted the slot's descriptor and was DESTROYED. Ten more are the
  generic-signature family, where yielding erases `Map<String,List<T>>` to
  `Map`, `T` to `Number` and `T[]` to `[LNumber;`. Five more are annotations
  coming back empty — **the same family lane 0 held**, so that is one
  cross-lane root cause and not two lane findings.
* **`BAD -> BAD`** — the bytecode is wrong too, so "yielding is correct here"
  is false. Not a regression; not a justified retirement either. This is what
  removed `Method.invoke` and `Constructor.newInstance`, which looked like
  clean seven-row and five-row keeps until the non-nestmate rows 247 and 248
  were attributed to them. **A mapping keyed on the row's tag misses rows that
  drive the same triple under another tag**, and the miss runs in the
  dangerous direction.
* **agreement at a DEFAULT value** — twelve entries. The sharp one is
  `Field.setBoolean`, whose only row sets `z` to `false` and reads it back
  through `getBoolean`, which row 15 proves broken: retiring it on
  `false == false` would be lane 0's `Module.isOpen` mistake exactly.

`the_l3_held_triples_are_not_retired` pins 26 of these, at least one per rule,
so a later wave cannot take them on the family's reputation.

**Structurally not retirable: the `java.lang.invoke` surface**, minus the three
`MethodType` accessors. §4 predicted `MemberName`/`MethodHandle` would be
VM-coupled and the measurement agrees — 14 `MethodHandles`, 8 `MethodHandle`
and 5 `CallSite` rows hold or are wrong both ways, and the run's
uninstantiable-receiver census names `MethodHandle` and `VarHandle` as abstract
classes a native instantiates. Those need the reviewed-`Intrinsic` protocol or
a repair, not a retirement.

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

## 8b. Wave 2, measured 2026-09-11: 6 of the 12 default-value deferrals clear

Wave 1 deferred **twelve** triples for one reason — they agreed with HotSpot
only at the value a blanket yield returns anyway (`false`, `null`, `0`), and an
agreement that cannot be distinguished from the default is not evidence. Wave
2's instrument gave each a row whose correct answer is the OTHER value. 16 rows
over 12 triples, on `cratonvm-p18` (six lanes' tables), all four arms 271 lines
with no NULs.

**6 promote, 6 reject.** Every promoted row is `OK -> OK` at a NON-default
oracle value, so the deferral reason is discharged:

| triple | the row that discharged it | oracle |
|---|---|---|
| `Constructor.getExceptionTypes` | NON-EMPTY on a two-exception ctor | `[IllegalStateException, IOException]` |
| `Constructor.isSynthetic` | census across four fixtures | `synthetic-ctors:0` |
| `Constructor.isVarArgs` | TRUE on `VarCtor(String...)` | `true` |
| `Field.isSynthetic` | TRUE on `this$0` | `synthetic:this$0` |
| `Method.isBridge` | TRUE on a generic override | `bridges:1` |
| `Method.isSynthetic` | count on an enum | `synthetic-methods:1` |

The six rejections, each by a rule rather than a judgement:

* **Still default-bound — 4.** `Field.isEnumConstant`, `Method.isDefault`,
  `Method.isVarArgs`, `Field.trySetAccessible`. Wave 2 gave them a
  non-default row where it could, but at least one attributed row still has
  `false` as the oracle, so the family's blanket answer still passes it.

  **WRONG, corrected 2026-09-11 from the same run's output: three of these four
  clear.** The criterion in that sentence is not the one this wave set out. The
  deferral asks for ONE row where the oracle is not the default and the VM
  agrees; it does not ask that every attributed row be non-default, which
  almost nothing would satisfy. Re-read off `w2p18`, all three arms:

  ```text
  row  61  M isVarArgs                    hs |true|  base |true|  armed |true|
  row  64  M isDefault iface              hs |true|  base |true|  armed |true|
  row 256  W2 F isEnumConstant TRUE       hs |true|  base |true|  armed |true|
  row 260  W2 F trySetAccessible FALSE    hs |false| base |true|
  ```

  `true` is not the blanket answer for a `()Z` yield, so the first three agree
  at a non-default value and their deferral reason is discharged — by the
  ORACLE and BASE arms, which need no dial at all. Only
  `Field.trySetAccessible` fails, and for the stronger reason below: it is a
  real BAD, not a default-bound unknown.

  **Two rows of the three were never attributed to their triple, and the cause
  is the trap the last bullet of this section names.** `l3w2map.tsv` is 16 of
  16 `W2 ` tags, so wave 1's `M isVarArgs` (probe line 400) and `M isDefault
  iface` (line 403) drive `Method.isVarArgs()Z` and `Method.isDefault()Z` under
  a wave-1 tag and the tag-keyed map could not see them. I wrote that warning
  in this section and then shipped the verdict it warns about. **A mapping
  keyed on a tag needs a completeness check against the TRIPLE, not against its
  own key set** — the map being internally consistent is exactly what made it
  look finished.

  `Field.isEnumConstant` is the one that was mapped and still misjudged, so the
  map is not the whole cause: the criterion itself was wrong. Both defects had
  to be present for the verdict to come out as it did.
* **`BAD -> BAD` — 2.** `Field.trySetAccessible` answers `true` where HotSpot
  answers `false` for a JDK-internal field — **a real access-control defect,
  not a retirement candidate.** The row is
  `System.class.getDeclaredField("props").trySetAccessible()`: HotSpot `false`,
  this VM `true`.

  **Priced 2026-09-11, and it is smaller than the source comment implies.**
  `lang_reflect.rs` says the VM "doesn't enforce module-level deep-reflection",
  which reads as an absent subsystem. It is not: the module state the JDK's own
  check needs is tracked and already queryable from natives —
  `Module.isOpen(String)` and `isOpen(String, Module)` are registered and
  backed by `ctx.is_package_open_unqualified` / `is_package_open_to`, which read
  a real `module_registry` (`vm_exec.rs:10088`). What is missing is the CHECK at
  this one door, not the data behind it.

  Three things anyone taking it should know, none of which is the code:

  1. **The existing carve-out is one hard-coded row, not a model.**
     `check_class_loader_define_class_is_encapsulated` fires only for declaring
     class `java/lang/ClassLoader` and member `defineClass`. Extending it by
     adding `System.props` and friends would close this row and generalise to
     nothing — the anti-pattern of a literal table in front of a registry that
     already knows the answer.
  2. **`is_package_open_to` FAILS OPEN on an empty registry** (`return true`).
     Anything built on it is vacuous for the window before the module graph is
     populated, so the check needs its own positive control: a row that must be
     DENIED, asserted to be denied, or the gate cannot tell enforcement from
     early-boot.
  3. **The blast radius is every `setAccessible` in the process**, not this
     row. Twelve of the 134 corpus vectors call it directly, and the framework
     suites (Spring, Hibernate, Jackson, ByteBuddy/CGLIB) are built on it — the
     population the source comment was protecting. So this wants its own gate
     set plus the app suites, and it is not a reflection-wave item.

  Recorded as priced, not fixed. A retirement wave is the wrong vehicle: the
  remedy here is a check the VM does not perform, which is the opposite of
  yielding to bytecode. `Constructor.setAccessible` differs only in the
  throwing FRAME; see below.
* **Secondary triple is HELD — 2.** `Field.setBoolean`'s two rows read the
  value back through `Field.get`, and `Constructor.setAccessible`'s through
  `Field.canAccess`. Both readers are held, so a disagreement could be the
  reader's fault. This is wave 1's `Method.invoke` lesson applied in advance:
  *a mapping keyed on the row's tag misses rows that drive the same triple
  under another tag, and the miss runs in the dangerous direction.*

### The 6 are candidates, not landings, and the reason is the dial

The armed arm here is `CRATONVM_ENFORCE_NATIVE_SHADOW=java.lang.reflect`, and
its own census read:

```text
[DIAL_DOOR_CENSUS] armed=true reached=1027 yielded=901 leaked=126
```

**All six promoted rows are `OK -> OK`, which means `armed == base`** — and a
row where armed equals base is exactly the ambiguous case: either yielding
changed nothing, or the dial leaked and never yielded there. 126 of 1027 did
not yield, so these six cannot be cited as "the bytecode is right". A leaky
dial is still usable, but only asymmetrically:

* `armed != base` -> the dial demonstrably fired; the verdict means something.
* `armed == base` -> ambiguous, and no amount of agreement fixes that.

So wave 2's product is a **screen**: it discharges the deferral reason for six
triples and hands them to a wave-3 build, where control-vs-retired on two
binaries answers the retirement question the dial cannot. What wave 2 has
already bought is the other half — **six triples definitively off the list**,
four still default-bound and two with named defects.

### Side-finding: `getStackTrace()[0]` is wrong for every application exception

`Constructor.setAccessible`'s `BAD -> BAD` row has the right exception type and,
once yielded, the byte-identical message; only the top frame differs — HotSpot
`AccessibleObject.throwInaccessibleObjectException`, this VM
`InaccessibleObjectException.<init>`. Chasing that produced a 9-row probe,
`apps/probes/ThrowableCtorFrameSkip.java`, and a correction to a known issue
rather than a new one: **`H22-2` §3b**. The defect is live unarmed, for any
user-defined exception subclass at any depth, and `H22-2` had recorded the
unarmed path as "correct by construction".

Two instrument notes earned in the same hour:

* **An `access()` row's frame column cannot distinguish "a different check
  fired" from "the stack trace is off by a constructor chain".** It is still
  the right column to print — a type alone cannot say which check fired — but a
  frame difference now gets `ThrowableCtorFrameSkip` run against it before it
  is read as an access-control finding.
* **A `grep` stage on ONE arm of a cross-VM diff strips CR and turns every row
  into a disagreement.** Both VMs write CRLF to a file on this host; MSYS
  `grep` in a pipe strips it. My ad-hoc comparison read 9 of 9 rows differing
  where 4 do. The lane's own scripts are safe because both arms go straight to
  a file with no filter, and that was verified rather than assumed (raw diff 52
  == normalised diff 52 on the wave-2 arms). Filter after the diff, never
  before, and never on one side only.

### 8c. Verified on `p19` at `77f9953b1`, and NOT on the current tip

**The tip moved after these numbers were taken** — a third `origin/dev` merge
brought 44 commits, so the tree now carries lane 1's waves 3-4, lane 7's table
and dev's JIT work on top of what `p19` had. `p20` is building and its arms
replace this block; the lane-0 page's §7.3 carries the same warning and the
reasoning for it. Treat the three numbers below as a reading on a named
revision, not as this lane's current state.

This lane's 24 retired triples were re-measured as part of a six-lane binary
rather than alone. `cratonvm-p19.exe`, md5 `c88f146699d0e39cb406b1433ef65a5a`,
from `77f9953b1`:

* three arms **132/0** (`--jdk-only`), **132/0** (`SUITE=all`), **92/0**
  (`SUITE=core`), all with `missing=0`;
* gate set all five arms `rc=0`, `nb-default` re-run at 11 targets / 4285
  passed / 0 failed;
* **52 distinct refused triples, 0 survivors** over all 132 kept reports, so
  this lane's rows yield the slot to bytecode rather than falling through to an
  older native.

The full account, including why the arms were re-run after `dev` brought 19
JIT/C2 commits and why one run header reads a different revision, is in the
lane-0 page's §7.3 — recorded once there rather than twice.

**This does not promote wave 2's six candidates.** Those still need the
two-binary control-vs-retired build of §8b: with `leaked=126` every one of them
is an `armed == base` row, which is exactly the shape a leak produces. A green
corpus on a binary whose table does not contain them is not evidence about
them.

### 8d. Wave 3 is prepared and blocked on two preconditions, not on a decision

The six candidates from §8b, with descriptors, sorted as the table wants them:

```text
java/lang/reflect/Constructor  getExceptionTypes  ()[Ljava/lang/Class;
java/lang/reflect/Constructor  isSynthetic        ()Z
java/lang/reflect/Constructor  isVarArgs          ()Z
java/lang/reflect/Field        isEnumConstant     ()Z    <- added by 8b's correction
java/lang/reflect/Field        isSynthetic        ()Z
java/lang/reflect/Method       isBridge           ()Z
java/lang/reflect/Method       isDefault          ()Z    <- added by 8b's correction
java/lang/reflect/Method       isSynthetic        ()Z
java/lang/reflect/Method       isVarArgs          ()Z    <- added by 8b's correction
```

**Nine, not six.** The three marked rows were rejected by wave 2 as
"still default-bound" and are not: §8b's correction reads their non-default
agreement straight off the same run. They cost no new measurement, which is
the point — the data was already on disk and the verdict was the thing that
was wrong.

That also settles the twelve deferrals exactly: **9 candidates + 3 rejections**
(`Field.trySetAccessible`, a real BAD; `Constructor.setAccessible`, frame-only
and secondary HELD; `Field.setBoolean`, secondary HELD). The earlier "6 and 6"
double-counted `trySetAccessible` and `setAccessible` across two rejection
categories each.

None is in `RETIRED_SHADOW_L3_TRIPLES` today, so wave 3 takes it **24 -> 33**.
All nine read `bridge` with `kind_stated 0` in the kind map, and
`Method.getExceptionTypes` beside them already reads `synthetic-stub` — the
sibling a previous wave retired, which is why `Constructor`'s is the candidate
and `Method`'s is not.

**Of the four preconditions, two are discharged and two are not** (the same
for all nine):

| # | precondition | state |
|---|---|---|
| 1 | dial ASKED | discharged by wave 2 (§8b) |
| 2 | whole probe tree no worse | discharged by wave 2, 271 lines, four arms |
| 3 | image target carries `Code` | **discharged**, 9 of 9 |
| 4 | `invocations > 0` per TRIPLE | **discharged**, 9 of 9 |

**3 and 4 were answered without a new build**, from the dump wave 2 already
took — `w2p18/L3ReflectInvokeSurface.registry.json`, `schema_version: 5`,
`image_adjudication: true`. All nine come back identical in shape:

```text
owns_slot  true      kind  bridge     invocations  3..7 (invocations_complete true)
image_declaring_method: image_has_class true, declared true,
                        acc_native FALSE, has_code TRUE
```

`acc_native: false` with `has_code: true` puts all nine in bucket A, so §1.5
does not govern them and there is real bytecode to yield to. `owns_slot: true`
means the table edit will not be inert. `invocations_complete: true` matters
separately — the registry's counter saturates on a warm loop, so an incomplete
count is a statement about the counter rather than the dispatch.

**Why p18's answers transfer to p20, stated rather than assumed:**

* Precondition 3 is a property of the **JDK image**, not of the VM build. Same
  JDK 25, same answer.
* Precondition 4 can only move upward. More retirements elsewhere add
  dispatches — a retired producer makes a previously unreachable consumer
  reachable, which is the finding that brought six of seven excluded rows back
  in an earlier wave. A row at `>0` on p18 cannot fall to `0` on p20.
* `owns_slot` would move only if some lane added a competing REGISTRATION.
  None did: the merged tree's registration totals are 13623 / 13991 / 13658,
  byte-identical to this branch's pre-merge readings, so `dev`'s 44 commits
  added no registrations at all.

The control dump in the pipeline is therefore a confirmation, not a dependency.

**One sequencing trap, recorded because it would have destroyed the control:**
the table edit must NOT be applied until `p20` is pinned. The release build
takes the working tree as it stands, so applying the nine before it runs makes
`p20` carry the rows it is supposed to be the control for — and the A/B would
compare a binary against itself, scoring both arms identical and reading as
"the retirement changes nothing".

**Precondition 3 cannot be read off the kind map, and it is worth saying why,
because the file looks like it answers.** Its trailing columns are ordinal,
kind, `kind_stated` and slot ownership — ownership, not image `Code`. The image
columns come only from `--dump-native-registry --explain-jdk-only`; without the
flag every row reports blank and a reader who does not know that sees a
confident-looking `1`. Same trap the lane already recorded for the census.

**Precondition 4 must be measured on the CONTROL binary**, not on a binary
carrying wave 3, because a retired producer can make a zero-invocation consumer
reachable — the finding that brought six of seven excluded rows back in an
earlier wave.

Both turned out to need no launch at all — see above.
The mechanical edit is written and checked for control bytes; it applies the
nine in sorted position and writes LF. Sequence, exploiting the fact that lane
0's verification binary is this wave's control:

1. `p20` — the merged tree, **without** the nine. Lane 0's §7.3 binary, and this
   wave's control. Run the dump on it for preconditions 3 and 4.
2. apply the nine, `p21` — the retired arm.
3. A/B `p20` vs `p21`: probe tree, then the three corpus arms.

A dial arm cannot substitute for step 3 here and §8b says why: with
`leaked=126` every one of these nine is an `armed == base` row, which is
exactly the shape a leak produces. Note this cuts both ways for the three just
added — their non-default agreement discharges the DEFERRAL, which is an
oracle-and-base question, and says nothing about whether the bytecode is
right, which is step 3's question.

## 9. Done

Every bucket-A/B row in the prefix set is retired, classified as C/D/E/F, a
reviewed `Intrinsic` with its probe, or blocked with the blocker named — with
the `invoke` package's VM-coupled rows explicitly separated from the ones that
were genuinely retirable.
