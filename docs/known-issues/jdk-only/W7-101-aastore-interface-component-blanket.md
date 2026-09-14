# `aastore` fails open on every interface-component array

> **This record was filed as `W7-39` and is now `W7-101` (renumbered
> 2026-08-13, lane C18).** `W7-39-jca-missing-algorithms.md` was filed 23 hours
> earlier the same day and keeps the number; two lanes allocated it without
> being able to see each other. **Any `W7-39` you meet in the tree refers to
> the JCA record** — except two Rust comments that still cite this file's old
> path (`vm/src/runtime/interpreter/typecheck.rs:782`,
> `vm/src/runtime/interpreter/tests.rs:91`), which are NOMINATION N1 in
> `INDEX.md`. All 11 in-directory references, in
> `W8-C10-1-typecheck-hatch-audit-and-aastore-precedence.md`, were rewritten.

**Status: FIXED IN SOURCE (lane C10, this record's own commit). NOT EXECUTED —
no binary was built or run in the lane that applied it, so every "after" value
is PREDICTED and §6 is what settles it. The "before" values are EXECUTED.**

Filed 2026-08-12 by the diagnosing lane, which measured the oracle and could not
apply the fix (`typecheck.rs` was outside its boundary). Closed 2026-08-12 by
lane C10, which owns that file.

Sibling of `W7-38-jit-aastore-never-called-its-own-check.md`. The two are
independent: W7-38 is "the check is never reached in compiled code", this is
"the check itself declines to answer for a whole family".

**The fix is not the six lines the original §4 nominated.** Deleting the
interface blanket alone would have moved one of the two red rows and left the
other exactly as red, for a reason the original analysis did not see — see §3.

---

## 1. The two arms, in source order

`vm/src/runtime/interpreter/typecheck.rs`, `aastore_element_assignable` — the
predicate shared by the interpreter's `aastore` opcode, the `jit_aastore` helper
and reflective `Array.set`.

**Arm A**, ~65 lines above arm B:

```rust
// Resolve the element's runtime class id; an unknown/synthetic class id
// (no loaded class entry) is treated as assignable (fail open).
let value_class_id = shared.mem.heap.class_id_of(value_ref);
if value_class_id == ClassId::new(0) {
    return true;
}
```

**Arm B**, the one this record was opened for:

```rust
// Component is an INTERFACE → fail open. Proving a value implements an
// interface is unreliable in this VM (dynamic proxies, annotation
// proxies, and synthetic classes implement interfaces at runtime / by
// name, invisibly to the static hierarchy). A genuine ArrayStoreException
// essentially always involves a concrete-class component (Number[],
// String[], …); for an interface[] we don't risk a spurious throw.
if cm.get_class(comp_id).map_or(false, |c| c.is_interface()) {
    return true;
}
```

## 2. What the oracle says

HotSpot 25.0.3 (Microsoft build 25.0.3+9-LTS), measured this wave, not recalled
(`scratchpad/c10/Oracle.java`, run under `-Xint`):

```text
String[] as Object[]      <- Integer   -> ArrayStoreException | java.lang.Integer
Object[]                  <- Integer   -> OK
Comparable[]=new String[1]<- Integer   -> ArrayStoreException | java.lang.Integer
Comparable[]=new Comp[1]  <- Integer   -> OK
Comparable[]=new Comp[1]  <- Object    -> ArrayStoreException | java.lang.Object
Runnable[]                <- lambda    -> OK
Runnable[]                <- String    -> ArrayStoreException | java.lang.String
String[][] as Object[]    <- Integer[] -> ArrayStoreException | [Ljava.lang.Integer;
String[] as Object[]      <- null      -> OK
null array                <- Integer   -> NullPointerException
String[] as Object[] idx 5<- Integer   -> ArrayIndexOutOfBoundsException
```

Row 3 confirms the original record's correction of the brief that started it:
the DECLARED type of the reference you store through never enters the check.
JVMS §6.5 *aastore* checks against the array's **actual runtime component
type**, which for `Comparable[] c = new String[1]` is `String`.

Row 4 is the delicate one and the reason a naive fix is wrong: the component
really is the interface `Comparable`, and `Integer` **implements** it rather
than extending it. A fix phrased as "is the value's class a *subclass* of the
component" breaks that row.

## 3. Why the original nomination was not sufficient

`ClassId(0)` **is `java/lang/Object`**. It is the first class this VM loads
(`ClassManager::bootstrap_core_classes` puts `java/lang/Object` first;
`ClassStore::add` hands out dense ids from zero), and the identity is recorded
as a MEASUREMENT elsewhere in the tree, not an inference —
`vm/src/native/jni.rs`'s `jclass` encoding note reports a C probe calling
`FindClass("java/lang/Object")` and getting `(nil)` back for precisely this
reason, and `vm/src/memory/reclaim_guard.rs` and
`vm/src/runtime/interpreter/tests.rs` both state it independently.

So arm A's comment — "an unknown/synthetic class id (no loaded class entry)" —
described something that is not what the test matches. What the test actually
matched was *every value whose runtime class is `java.lang.Object`*. And arm A
runs first. `Comparable[] <- Object` returned `true` at arm A and never reached
arm B at all.

The original §4 predicted "s02/s03 green after". Only `s03` would have moved.
`s02` would have stayed red with the blanket gone, which reads as "the deletion
was wrong" and is the most expensive possible outcome for the next lane.

Two arms, one symptom, and the arm that fires is not the arm the record was
about. **When two fail-open guards can produce the same wrong answer, fixing the
one you found does not tell you whether it was the one that fired** — source
order does, and it is cheap to check.

## 4. Why the stated rationale of arm B no longer holds

The comment's justification is sound in itself: dynamic proxies, annotation
proxies and synthetic classes acquire interfaces at runtime, invisibly to the
static hierarchy, so a naive interface check would throw spuriously on them.

But **every one of those cases is already handled by code that sits below the
blanket and can never be reached because of it.** In source order, after it:

1. `cm.is_subclass_of(value_class_id, comp_id)` — and this **does** walk
   implemented interfaces, transitively through super-interfaces
   (`classloading/src/class.rs`, `is_subclass_of_inner`, the
   `for &iface_id in &self.interfaces` loop, verified by reading it). It answers
   `Comparable[] <- Integer` on its own. The blanket was not protecting a check
   that cannot handle interfaces; it was short-circuiting one that can.
2. `value_class_id.as_u32() >= 0x8000_0000` — synthetic lambda/proxy ids, fail open.
3. the `$Proxy` / `AnnotationProxy` name test — fail open.
4. `class_chain_reaches_proxy_instance(...)` — fail open.
5. `synthetic_implements(shared, value_class_id, comp_name)` — the name-based
   fallback for synthetic classes, fail open.

Hatches 2–5 are *exactly* the populations the blanket's comment cites, and they
were added after it. So the blanket was redundant with its own justification: an
early return that pre-empted five more precise successors, contributing nothing
but the false negatives in §2.

**A fail-open guard written before its precise replacements existed does not
become harmless when they arrive — it becomes the reason they never run.** The
comment kept reading as current because everything it says about proxies is
still true; what changed is that it was no longer the only thing standing
between them and a spurious throw.

## 5. The fix as applied

`vm/src/runtime/interpreter/typecheck.rs`, three changes.

**Arm A** — stop treating `java.lang.Object` as "unknown", but keep the hatch
for the OTHER `ClassId(0)` face. The all-zero header the collector leaves over a
reclaimed span also reads as `java.lang.Object` (the H2-CID0 family), and
`GenerationalHeap::reclaimed_hole_at` is the precise successor for exactly that
ambiguity — it has no false positives, because a live object is never inside a
free block, never past the allocation frontier and never in the inactive
semispace. So the arm now asks it instead of failing open on every
`new Object()`:

```rust
let value_class_id = shared.mem.heap.class_id_of(value_ref);
if value_class_id == ClassId::new(0)
    && shared.mem.heap.reclaimed_hole_at(value_ref.as_ptr() as usize).is_some()
{
    return true;
}
```

The job arm A's comment *claimed* — fail open when the value's class has no
loaded entry — is done ~30 lines below by
`cm.get_class(value_class_id).is_none()`, which asks the store instead of
pattern-matching an id. Cost is bounded: only a value that is BOTH `ClassId(0)`
AND bound for a non-`Object[]` component reaches the call, because an
`Object[]`/`Serializable[]`/`Cloneable[]` component and an ARRAY value both
return earlier.

**Arm B** — deleted. Control now reaches `is_subclass_of` and the five hatches.

**New, and the thing that makes the deletion safe:** a loader-blind interface
walk after `is_subclass_of`.

```rust
if cm.is_assignable_to_name(value_class_id, comp_name) {
    return true;
}
```

The blanket was incidentally covering one legitimate case that nothing else
does. An interface component is the one relation the by-name superclass walk
above it cannot reach, because interfaces are not on the superclass chain. Under
a split loader (`@CompileWithForkedClassLoader`) the value's `interfaces` vector
can name the OTHER loader's copy of the component, and `is_subclass_of` compares
`ClassId`s, so it refuses a legal store — which is the `AotIntegrationTests`
shape this predicate was patched for in the first place. `is_assignable_to_name`
(`classloading/src/class.rs`) already exists for exactly this, in JIT
`checkcast`/`instanceof`, and walks supers AND interfaces BY NAME.

It cannot rescue the two red rows: `java/lang/Object` reaches no
`java/lang/Comparable` node under any name, and `java/lang/String` reaches no
`java/lang/Runnable`. Verified by reading `is_assignable_to_name_inner`, and
independently by the oracle rows `Comparable[] <- String -> OK` (String IS
Comparable) versus `Runnable[] <- String -> ASE`.

Note it SUBSUMES the by-name superclass walk above it. They are one check with a
hot allocation-free fast path, not two independent defences — the same thing
that file's own comment records having learned once already, so it is said in
the code rather than left to be rediscovered.

## 6. Vector, and the executed before-numbers

`regression-suite/src/RArrayStoreTiers.java` (existing, filed with W7-38)
carries the four interface shapes as `s02`, `s03`, `s08`, `s09`.

**EXECUTED, 2026-08-12**, orchestrator run on the wave binary (which predates
W7-38's codegen fix, so it is a clean pre-fix baseline):

| run | result |
|---|---|
| HotSpot 25.0.3 | `PASS RArrayStoreTiers (63 checks)` |
| CratonVM, JIT on | 16 divergences |
| CratonVM, `--nojit` | 10 divergences |

The four rows this record owns, under `--nojit` — i.e. this predicate, with the
JIT excluded as a variable:

| shape | HotSpot | CratonVM before | arm |
|---|---|---|---|
| `s02 Comparable[] <- Object` | ArrayStoreException | **no-throw** | A (`ClassId(0)`) |
| `s03 Runnable[] <- String` | ArrayStoreException | **no-throw** | B (interface blanket) |
| `s08 Comparable[] <- Integer` | no-throw | no-throw | must stay |
| `s09 Runnable[] <- lambda` | no-throw | no-throw | must stay |

**After (PREDICTED):** `s02`/`s03` green under `--nojit`, `s08`/`s09` unchanged.
If `s08`/`s09` go red, the deletion was not the right fix and a hatch below is
missing a population.

A second, wider fixture landed with this record —
`regression-suite/src/RArrayStoreInterfaces.java`, 27 shapes / 108 checks, green
on HotSpot with the JIT **and** under `-Xint`, and mutation-checked (emulating
the blanket in pure Java yields 36 divergences). Four rows are not enough to
separate "the check works" from "the check is absent", because a blanket allow
gets every LEGAL row right for free — so that fixture pairs every legal store
with an illegal store into the same array and prints
`illegal stores refused: N/12` beside `legal stores admitted: N/15`. See §5 of
`W8-C10-1-typecheck-hatch-audit-and-aastore-precedence.md`.

Reproduce:

```sh
javac -d <outdir> regression-suite/src/RArrayStoreTiers.java
<cratonvm>          -cp <outdir> RArrayStoreTiers   # both tiers
<cratonvm> --nojit  -cp <outdir> RArrayStoreTiers   # interpreter only
```

Red only WITHOUT `--nojit` is W7-38. `s02`/`s03` red WITH `--nojit` is this
record.

## 7. Not fixed here

Three findings from the same audit are deliberately NOT in this change, each
with its own reason. They are in
`W8-C10-1-typecheck-hatch-audit-and-aastore-precedence.md`:

* the `Serializable[]` / `Cloneable[]` early return, which diverges from the
  oracle on three measured rows and is **kept** — with the reasoning, so the
  next lane does not have to re-derive it;
* `aastore`'s exception PRECEDENCE, which is inverted in the interpreter
  (ArrayStoreException where HotSpot gives ArrayIndexOutOfBoundsException) —
  a nomination, different file;
* the residual over-admissions in `synthetic_implements`' substring arm.
