# H15-2 — `instanceof` does not read the `getClass()` alias that `Class.isInstance` already reads, and the "second copy" objection is spent

**Status: DIAGNOSED, NOT FIXED.** No Rust written (diagnosis lane). The patch in
§5 is written out and applied nowhere.

**Date** 2026-08-20
**Lane** H15
**Subject** `RImmutableFactoryTypes` — the one of the five `SUITE=all` failures
that also fails the narrower `SUITE=core` gate (63/64), and `H0-2` §4/§5
**Companion** `H15-1` (why all five are Compatible-mode defects), `H15-3` (the
other four)
**Binary** `C:/craton/target-jdkonly-h2/release/cratonvm.exe` @ `fe59bf9d9`
**Oracle** HotSpot 25.0.3+9, `/c/Program Files/Microsoft/jdk-25.0.3.9-hotspot`

Claims are **MEASURED** (ran it) or **ARGUED** (read it).

---

## 1. The divergence, in one line

**MEASURED**, both VMs, same class file, same minute:

| | `Map.of("k","v") instanceof AbstractMap` | `AbstractMap.class.isInstance(…)` | `getClass().getName()` |
|---|---|---|---|
| **HotSpot 25.0.3+9** | `true` | `true` | `java.util.ImmutableCollections$Map1` |
| **CratonVM, Compatible** | **`false`** | `true` | `java.util.ImmutableCollections$Map1` |
| **CratonVM, `--jdk-only`** | `true` | `true` | `java.util.ImmutableCollections$Map1` |

The `getClass()` names are **identical**. `getSuperclass()` walked from that
mirror is **identical** on both VMs:

```
java.util.ImmutableCollections$Map1
  -> java.util.ImmutableCollections$AbstractImmutableMap
  -> java.util.AbstractMap
  -> java.lang.Object
```

So the VM will tell you, through reflection, that the receiver's class is
`Map1`, that `Map1`'s superclass chain passes through `AbstractMap`, and that
`AbstractMap.class.isAssignableFrom(o.getClass())` is `true` — and the
`instanceof` opcode next to it says `false`. **Two implementations of one JVMS
rule, disagreeing, in the same expression.** The vector's own header predicted
exactly this shape ("the reflective pair is right and the opcodes are not") and
this lane confirms it is still true today.

`RImmutableFactoryTypes` reports **one** divergence out of **219 checks**
(**MEASURED**: `1 divergence(s) from HotSpot 25: [Map.of(k,v) must be instanceof
AbstractMap …]`). 218 of 219 pass. It is a narrow, findable bug — as the brief
predicted — and it is the whole reason `SUITE=core` is 63/64.

---

## 2. It is NOT a depth bug, and it is NOT a `getClass()` lie

Two hypotheses a reader will reach for. Both **MEASURED FALSE**.

**Not depth.** A user-declared `D extends C extends B extends A` answers
`instanceof A` **true** at three levels up, and so do
`LinkedHashMap instanceof AbstractMap` (2 up), `TreeSet instanceof
AbstractCollection` (2 up), `ArrayList instanceof AbstractCollection` (2 up) and
`Collections.emptyList() instanceof AbstractCollection` (2 up). All `true` on
both VMs. The chain length is irrelevant.

**Not a lying alias.** `Class.forName("java.util.ImmutableCollections$Map1") ==
Map.of("k","v").getClass()` is **`true`** on CratonVM, and that same mirror's
`getSuperclass()` chain reaches the real `AbstractMap.class`. The alias is
**correct and consistent**; the opcode simply never consults it.

**And it is not `Collections.unmodifiable*`.** The one row where both VMs answer
`false` —
`Collections.unmodifiableList(...) instanceof AbstractCollection` — is
**correct on both**: the real `Collections$UnmodifiableList` is not an
`AbstractCollection`. That negative control is what pins the defect to the
*immutable* family rather than to the wrapper machinery in general.

---

## 3. The full cell set, re-measured today

`H0-2` §4 tabulated twelve divergent cells on 2026-08-20 and this directory's
own standing trap says a triage page is stale the day after it is written. **All
twelve re-measured, and all twelve still divergent**, on this binary, plus the
`instanceof` vs `isInstance` split for every one of them. Probe (write it
anywhere and compile with the oracle's `javac`):

```java
import java.util.*;
public class H15Ra {
    static void line(String label, Object o, boolean io, Class<?> t) {
        System.out.println(label + " | " + t.getSimpleName()
                + " | instanceof=" + io
                + " | isInstance=" + t.isInstance(o)
                + " | getClass=" + o.getClass().getName());
    }
    public static void main(String[] a) {
        Object l0 = List.of(), l1 = List.of("x"), l3 = List.of("x","y","z");
        Object lc = List.copyOf(new ArrayList<>(List.of("x")));
        Object ul = Collections.unmodifiableList(new ArrayList<>(List.of("x")));
        line("List.of()",        l0, l0 instanceof RandomAccess, RandomAccess.class);
        line("List.of(1)",       l1, l1 instanceof RandomAccess, RandomAccess.class);
        line("List.of(3)",       l3, l3 instanceof RandomAccess, RandomAccess.class);
        line("List.copyOf",      lc, lc instanceof RandomAccess, RandomAccess.class);
        line("unmodifiableList", ul, ul instanceof RandomAccess, RandomAccess.class);
        Object mc = Map.copyOf(new HashMap<>(Map.of("k","v")));
        line("Map.copyOf", mc, mc instanceof AbstractMap, AbstractMap.class);
        Object s3 = Set.of("a","b","c");
        line("Set.of(3)", s3, s3 instanceof AbstractCollection, AbstractCollection.class);
        System.out.println("binarySearch(List.of,\"c\")="
                + Collections.binarySearch(List.of("a","b","c","d","e"), "c"));
    }
}
```

**MEASURED**, CratonVM Compatible — every row `instanceof=false`,
`isInstance=true`, `getClass=` the correct JDK name:

```
List.of()        | RandomAccess      | instanceof=false | isInstance=true | java.util.ImmutableCollections$ListN
List.of(1)       | RandomAccess      | instanceof=false | isInstance=true | java.util.ImmutableCollections$List12
List.of(3)       | RandomAccess      | instanceof=false | isInstance=true | java.util.ImmutableCollections$ListN
List.copyOf      | RandomAccess      | instanceof=false | isInstance=true | java.util.ImmutableCollections$List12
unmodifiableList | RandomAccess      | instanceof=false | isInstance=true | java.util.Collections$UnmodifiableRandomAccessList
Map.copyOf       | AbstractMap       | instanceof=false | isInstance=true | java.util.ImmutableCollections$Map1
Set.of(3)        | AbstractCollection| instanceof=false | isInstance=true | java.util.ImmutableCollections$SetN
binarySearch(List.of,"c")=2
```

**MEASURED**, HotSpot 25.0.3+9 — every row `instanceof=true`, `getClass=` the
same seven names, `binarySearch=2`.

`H0-2` §4's reading of the `RandomAccess` row is confirmed and worth restating,
because it is the row with a **contract** attached rather than a curiosity:
`Collections.binarySearch`, `reverse`, `shuffle`, `fill`, `copy` and `swap` all
branch on `list instanceof RandomAccess`. Every one of them takes the
`ListIterator` fallback on a `List.of(...)` under CratonVM. The **answers stay
correct** (`binarySearch` returns `2` on both VMs) — which is precisely why no
value-diffing vector can see it, and why `N5` in `H15-1` exists.

And note the shape, sharpened by the `isInstance` column: the receiver's
`getClass()` reports
`java.util.Collections$UnmodifiableRandomAccessList` — **a class whose name IS
the assertion** — while the opcode beside it denies the interface that name
promises, and reflection agrees with the name.

---

## 4. The mechanism, traced to named functions

Three source facts, all **ARGUED** from the tree at `fe59bf9d9`, that together
close it.

### 4.1 The receiver's stamp has `java/lang/Object` as its superclass, by construction

`vm/src/vm/vm_init.rs:1746-1820`. Eleven `cratonvm/internal/Unmodifiable*`
stamps are registered; each is given an accurate **interface** list
(`UnmodifiableList` gets `List`, `Collection`, `Serializable`; `UnmodifiableMap`
gets `Map`, `Serializable`; …) and then, at **`vm_init.rs:1813`**:

```rust
                class_manager.set_superclass(cid, Some(object_id));
```

That single line is why the interface cells are all correct and every
superclass cell is wrong. `Map.of(...)`, `List.of(...)`, `Set.of(...)` and the
`copyOf` family allocate one of these stamps in Compatible mode
(`native-collections/src/lib.rs`, `alloc_unmod_wrapper` → the immutable variant
that sets the `UNMOD_FIELD_IMMUTABLE` marker in slot 1).

### 4.2 The opcodes consult four predicates, and none of them can see the alias

`vm/src/runtime/interpreter/opcodes.rs`, `op_instanceof` (fn at `:2150`,
decision at `:2298-2317`) and `op_checkcast` (fn at `:2338`, decision at
`:2487-2506`) share one disjunction:

```rust
    is_subclass_of(obj_class_id, target_class_id)
        || loader_aware_name_assignable(shared, obj_class_id, target_class_id, &target_class_name)
        || lambda_proxy_satisfies(shared, obj_class_id, target_class_id)
        || synthetic_implements(shared, obj_class_id, &target_class_name)
        || proxy_instance_satisfies_target(shared, obj_ref, &target_class_name)
        || annotation_proxy_satisfies_target(shared, obj_ref, &target_class_name)
```

* `is_subclass_of` walks the stamp's real chain — which is `Object` (§4.1) —
  so it declines every superclass target.
* `synthetic_implements` (`vm/src/runtime/interpreter/typecheck.rs:1266`) is
  **name-only** and its collection arm is gated on
  `obj_name.starts_with("java/util/")`. The stamp is `cratonvm/internal/…`, so
  the gate is false and the whole heuristic declines — which is *correct*, since
  it can only affirm interfaces and would have no honest answer for
  `AbstractMap` anyway.
* the two `proxy_*` predicates are instance-aware but are about proxies.

**There is no arm that reads the receiver's display class.** Note that the two
instance-aware arms establish that `obj_ref` **is in scope** at both decision
sites — the plumbing §5 needs already exists and is already used.

The same six-way disjunction has three further copies that would need the same
arm for tier-consistency: `vm/src/jit/helpers.rs:8502` and `:8679`, and
`vm/src/vm/vm_exec.rs:20309`.

### 4.3 `Class.isInstance` has the arm, and its comment says why it was added

`native-builtins/src/lang_class.rs:3908-3927`, the last thing
`native_class_is_instance` tries before answering `0`:

```rust
    // Consistency with `getClass()`: synthetic collection wrappers (the
    // `List.of`/`Map.of`/`Collections.unmodifiable*` families) and other
    // JDK-aliased objects carry an internal craton stamp class as their raw
    // header `class_id`, but `Object.getClass()` reports a cosmetic *display*
    // class (e.g. `java.util.ImmutableCollections$List12`) via
    // `getclass_display_class_id`. The internal stamp is not registered as a
    // subtype of that display class, so the raw-id check above returns false
    // for `List12.class.isInstance(List.of("a","b"))` even though
    // `obj.getClass() == List12.class`. `Class.isInstance` must agree with
    // `getClass()`, so also test the object's display class id. […]
    if let Some(disp) = crate::getclass_display_class_id(ctx, target_class_id, target) {
        if disp == this_class_id || ctx.is_subclass(disp, this_class_id) {
            return Ok(Some(Value::Int(1)));
        }
    }
```

**That is the entire defect, stated by the tree itself.** The rule
*"the subtype answer must agree with `getClass()`"* was recognised, written
down, and implemented — **for the reflective spelling only**. The opcode
spelling of the same rule was never given the same arm. The comment even names
the application that forced it (Spring's `GenericConversionService.convert`),
which is the same class of consumer that hits the opcode path.

---

## 5. The patch — written out, NOT applied

### 5.1 First, the objection this has to clear

`H0-2` §5 declined exactly this fix, and its reasoning was good:

> *"The obvious patch is to add an object-aware arm to `INSTANCEOF` … **That
> would be the second copy of one rule**, and this directory's history is
> largely a record of what a second copy costs: `E18-1` found the fourth copy of
> one search rule, `E27-1` the fifth …"*

**That objection no longer applies, and §4.3 is why.** The rule already has a
second implementation — `getclass_display_class_id`, in `native-builtins`, with
`Class.isInstance` as its consumer. The cost was paid on 2026-08-something and
the copy exists. What is proposed here is **not a third copy**: it is giving the
existing function **a second caller**.

That is mechanically available, checked rather than assumed:

* `vm/Cargo.toml:144` — `cratonvm-native-builtins = { path = "../native-builtins" }`.
  **The `vm` crate already depends on `native-builtins`**, and `native-builtins`
  does not depend on `vm`, so there is no cycle.
* `crate::vm::NativeContextImpl` (defined `vm/src/vm/vm_exec.rs:5570-5573` as
  `{ pub shared: &'a SharedVm, pub thread: &'a mut JvmThread }`) is a two-field
  struct the JIT constructs in five places (`vm/src/jit/helpers.rs:12462, 13212,
  13227, 13340, 13570`) from **precisely** the two values the opcode already
  holds. `typecheck.rs` opens with `use super::*`, so `JvmThread` and
  `SharedVm` are already in scope there.
* `getclass_display_class_id` is `pub(crate)` today
  (`native-builtins/src/lib.rs:26336`) and would need widening to `pub`.

### 5.2 The one real hazard, and why the patch does not use that function directly

`getclass_display_class_id`'s size-dependent arms call
`getclass_collection_size`, which does `ctx.invoke_virtual(this, "size", "()I")`
— **it runs Java code, and the heap can move under it.** In
`native_class_is_instance` that is the last statement before returning and the
receiver is not used afterwards. **In the middle of `op_instanceof` it is not
safe**: `obj_ref` is a bare Rust local at that point and the surrounding code
re-reads it (`op_checkcast` reads `shared.mem.heap.class_id_of(obj_ref)` again
in its failure path). Pinning across it would work but adds a GC root to the
hottest opcode in the interpreter for no benefit —

**— because the size is irrelevant to the question.** `Map1` and `MapN` have
identical supertypes; so do `List12`/`ListN` and `Set12`/`SetN`. The subtype
answer needs only the **family**, and the family needs only the slot-1 marker
(and, for lists, the slot-0 `RandomAccess` probe, which is a pure `ctx.is_subclass`
call and runs no Java). So the patch adds a **coarser exit from the same
`match`**, not a parallel table.

### 5.3 Hunk 1 — `native-builtins/src/lib.rs`, after `getclass_display_class_id` (currently ends ~`:26421`)

```rust
/// The display class of `class_id`/`this` resolved only as far as its FAMILY —
/// enough to answer a SUBTYPE question, and cheap enough to answer one from
/// inside an opcode.
///
/// [`getclass_display_class_id`] is the authority and this is a restricted exit
/// from the same classification: same `collection_display_kind` stamp table,
/// same `getclass_immutable_marker` slot, same `getclass_backing_is_*` probes.
/// The ONE thing it does not do is pick between the size-discriminated forms
/// (`Map1`/`MapN`, `List12`/`ListN`, `Set12`/`SetN`), because
/// `getclass_collection_size` reaches the size through
/// `invoke_virtual(this, "size")` — it RUNS JAVA CODE, and the caller this
/// exists for (`op_instanceof` / `op_checkcast`) holds `obj_ref` as a bare Rust
/// local the collector cannot see. The size cannot change the answer anyway:
/// `Map1` and `MapN` have identical supertypes, and so do the List and Set
/// pairs. Every remaining probe here reads fields or calls `ctx.is_subclass`
/// and runs no Java.
///
/// `None` means "no display alias" — the caller keeps the stamp.
///
/// docs/known-issues/jdk-only/H15-2-the-opcode-does-not-read-the-alias-the-reflection-does-20260820.md
pub fn getclass_display_family_class_id(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
    this: ObjectRef,
) -> Option<cratonvm_types::ClassId> {
    let name = ctx.class_name_of_id(class_id)?;
    let kind = match collection_display_kind(&name) {
        Some(k) => k,
        None => return jdk_concrete_getclass_alias(&name)
            .and_then(|alias| getclass_resolve_name(ctx, alias)),
    };
    let immutable = getclass_immutable_marker(ctx, this);
    let display = match kind {
        // Size-independent for subtype purposes: the `…N` form stands for the
        // whole family, and `AbstractImmutableList`/`…Collection` sit above both.
        GetClassDisplay::CollList => {
            if immutable {
                "java/util/ImmutableCollections$ListN"
            } else if getclass_backing_is_random_access(ctx, this) {
                "java/util/Collections$UnmodifiableRandomAccessList"
            } else {
                "java/util/Collections$UnmodifiableList"
            }
        }
        GetClassDisplay::CollSet => {
            if immutable {
                "java/util/ImmutableCollections$SetN"
            } else {
                "java/util/Collections$UnmodifiableSet"
            }
        }
        GetClassDisplay::CollMap => {
            if immutable {
                "java/util/ImmutableCollections$MapN"
            } else {
                "java/util/Collections$UnmodifiableMap"
            }
        }
        GetClassDisplay::CollSortedSet => "java/util/Collections$UnmodifiableSortedSet",
        GetClassDisplay::CollNavigableSet => "java/util/Collections$UnmodifiableNavigableSet",
        GetClassDisplay::CollUnmod => "java/util/Collections$UnmodifiableCollection",
        GetClassDisplay::Stamp | GetClassDisplay::Fixed(_) => return None,
    };
    getclass_resolve_name(ctx, display)
}
```

*(`GetClassDisplay::Fixed` is unreachable from `collection_display_kind` and is
folded into the `None` arm rather than special-cased; if the enum grows a
non-collection variant later this arm is the one that must be revisited.)*

### 5.4 Hunk 2 — `vm/src/runtime/interpreter/typecheck.rs`, a new instance-aware predicate beside `proxy_instance_satisfies_target`

```rust
/// Does the receiver's DISPLAY class — the one `Object.getClass()` reports —
/// satisfy `target_class_id`?
///
/// A `cratonvm/internal/Unmodifiable*` stamp carries `java/lang/Object` as its
/// superclass (`vm_init.rs`, the eleven `unmod_specs`) and an accurate
/// INTERFACE list. That is why `Map.of(…) instanceof Map` is right and
/// `Map.of(…) instanceof AbstractMap` is wrong: the interfaces are declared and
/// the superclass chain is not there to walk.
///
/// `Class.isInstance` has answered this correctly since the
/// `GenericConversionService` fix (`lang_class.rs`, the "Consistency with
/// `getClass()`" arm) by consulting the same display map. This is that arm, at
/// the opcode. It is NOT a second copy of the rule — it is a second caller of
/// the one function that implements it.
///
/// Can only ADMIT: it runs after `is_subclass_of` has declined, and the display
/// class is the class HotSpot genuinely reports for this receiver, so admitting
/// exactly its supertypes cannot over-admit. The negative control is
/// `Collections.unmodifiableList(…) instanceof AbstractCollection`, which stays
/// FALSE on both VMs because `Collections$UnmodifiableList` genuinely is not an
/// `AbstractCollection`.
///
/// Runs no Java code and pins nothing — see
/// `getclass_display_family_class_id` for why the size probe is excluded.
///
/// docs/known-issues/jdk-only/H15-2-the-opcode-does-not-read-the-alias-the-reflection-does-20260820.md
pub(crate) fn display_class_satisfies_target(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj_ref: cratonvm_types::ObjectRef,
    obj_class_id: ClassId,
    target_class_id: ClassId,
) -> bool {
    // Cheap screen first: only the eleven internal stamps have a display alias,
    // and the memo in `getclass_display_family_class_id` negative-caches every
    // other ClassId — but the `NativeContextImpl` build below is not free, so
    // decline before it for anything that is not one of them.
    let is_stamped = shared
        .classes
        .class_manager
        .read()
        .get_class(obj_class_id)
        .is_some_and(|c| c.name.starts_with("cratonvm/internal/Unmodifiable"));
    if !is_stamped {
        return false;
    }
    let mut ctx = crate::vm::NativeContextImpl { shared, thread };
    let Some(disp) =
        cratonvm_native_builtins::getclass_display_family_class_id(&mut ctx, obj_class_id, obj_ref)
    else {
        return false;
    };
    disp == target_class_id
        || shared
            .classes
            .class_manager
            .read()
            .is_subclass_of(disp, target_class_id)
}
```

### 5.5 Hunk 3 — wire it into both opcodes

`vm/src/runtime/interpreter/opcodes.rs`, in **both** `op_instanceof` (`:2298`)
and `op_checkcast` (`:2487`), append one term to the disjunction. It goes
**last**, after every cheap predicate, because it is the only one that builds a
`NativeContextImpl`:

```rust
                            || annotation_proxy_satisfies_target(
                                shared,
                                obj_ref,
                                &target_class_name,
                            )
+                           || display_class_satisfies_target(
+                               shared,
+                               thread,
+                               obj_ref,
+                               obj_class_id,
+                               target_class_id,
+                           )
```

**Borrow note, checked against the current bodies:** in `op_instanceof` the
disjunction's value is bound to `result` and only then is `thread` used again
(`thread.frames[frame_idx].stack.push`), so the `&mut thread` borrow ends before
the push. In `op_checkcast` the disjunction is bound to `cast_ok` inside a block
that ends immediately after; the failure path re-borrows `shared` only. Neither
needs restructuring. **This has not been compiled** — that is the first thing to
check.

### 5.6 Optional hunk 4 — the three tier twins

For tier-consistency (an answer that flips when a method compiles is worse than
a consistently wrong one), the same term belongs at
`vm/src/jit/helpers.rs:8502`, `vm/src/jit/helpers.rs:8679` and
`vm/src/vm/vm_exec.rs:20309`, which today end their disjunctions at
`synthetic_implements_public`. **Deliberately listed separately**: those three
sites already lack the two `proxy_*` arms the interpreter has, so they are
*already* less capable than the interpreter and this is a pre-existing
asymmetry, not one the patch introduces. Landing 5.3–5.5 alone is correct and
does not make the gap worse; landing 5.6 as well closes it. Whoever does the
latter owes a tier-differential measurement, because there is no vector that
would catch a divergence between them (`H7-1` N1 asks for exactly that
instrument and it still does not exist).

---

## 6. What to re-measure before believing any of it

**PREDICTED**, each with its falsifier:

1. `RImmutableFactoryTypes` goes **GREEN** in `SUITE=core` (64/64) and in
   `SUITE=all`. Falsifier: it still reports the `AbstractMap` divergence,
   meaning the display resolution did not fire — check that `is_stamped` is
   true for the receiver by dropping the screen temporarily.
2. The §3 probe answers `instanceof=true` for **all seven** rows, matching the
   HotSpot column byte for byte. This is the tight one: it covers the
   `RandomAccess` cells, which no vector asserts, so it must be run by hand.
3. `Collections.unmodifiableList(...) instanceof AbstractCollection` stays
   **FALSE**. This is the negative control and the thing an over-broad patch
   breaks first.
4. `RCollections`, `RJdkMapViews`, `RChmKeySetView`, `RJdkViews` stay green —
   they are the vectors that exercise these carriers hardest.
5. `--jdk-only` is **unmoved at 104/104**. Strict mode never allocates these
   stamps (`H4-1` §2: every allocator is `SyntheticStub` and is dropped), so the
   new arm's screen is false for every receiver there and the patch is inert.
   Falsifier: any movement at all in the strict arm means the screen is matching
   something it should not.

---

## 7. NOMINATIONS

**N1 — `H0-2` §5's "principled fix" (make the containers real) and this patch
are not alternatives, and the record should say so.** §5 there rejects the
opcode arm in favour of retagging the map/set cluster. `H4-1` §2 has since
measured that the retag **cannot happen**: `NativeKind::allowed_in` is
unconditionally `true` for `Compatible`, so no tag on any registrar changes what
`alloc_unmod_wrapper` does in the mode where the defect lives. The cluster fix
is a **real-container migration**, not a retag, and it is large. This patch is
~90 lines and closes twelve measured cells today. It should be landed as the
interim and explicitly marked as such, with a line saying it becomes dead code
the day `Map.of` returns a real `ImmutableCollections$Map1`.

**N2 — `getclass_display_class_id` runs Java code and three of its callers do
not obviously pin.** §5.2 established the hazard for the opcode. The three
existing callers (`lang_class.rs:3923`, `lib.rs:26517`, `lib.rs:26588`) should
be audited for whether the receiver is used after the call; `:26517` and
`:26588` are the `getClass()`/`toString()` natives, where it likely is not, but
"likely" is what this directory exists to replace. One afternoon.

**N3 — the interpreter has six subtype predicates and the three JIT/exec twins
have four.** §5.6. Nobody has written down that the JIT's `instanceof` lowering
cannot answer a proxy question the interpreter can. Whether that is reachable
(a proxy receiver in a compiled `instanceof`) is unmeasured; if it is, it is a
tier-dependent wrong answer, which is the exact defect shape `H7-1` N1's
proposed `RJitMapTierDiff` vector was designed to catch and which nothing
currently catches.

---

## INDEPENDENT REPRODUCTION (lane H0, 2026-08-20)

On the round-5 binary, compatible mode, probe saved as
`regression-suite/probes/OpcodeVsReflectionProbe.java`. Each cell prints
`instanceof` and `Class.isInstance` **for the same receiver and the same type**,
so a divergence is visible without a HotSpot column at all.

| receiver | test | HotSpot | CratonVM `instanceof` / `isInstance` |
|---|---|---|---|
| `Map.of()`, `Map.of(k,v)`, `Map.copyOf` | `AbstractMap` | true | **`false` / `true`** |
| `List.of()`, `List.of(1,2,3)` | `AbstractCollection` | true | **`false` / `true`** |
| `List.of()`, `List.of(1,2,3)` | `RandomAccess` | true | **`false` / `true`** |
| `Set.of(x)` | `AbstractCollection` | true | **`false` / `true`** |
| `Collections.unmodifiableList` | `RandomAccess` | true | **`false` / `true`** |

**Seven of seven receivers diverge, in the direction this record states.** The
two paths disagree *with each other* on one object at one instant — the
`a-reflective-native-and-its-bytecode-opcode-are-twins-that-drift` species,
and here the twins have drifted far enough that the object's own answer depends
on which door asked.

**Method note, because my first attempt got the wrong answer.** I first probed
`instanceof Map`, `instanceof List`, `instanceof Set`, `instanceof Function`,
`instanceof Comparator` across nine receivers and found **zero** divergence —
and had I stopped there I would have recorded "not reproduced" against a lane
that was right. Those are *interfaces*; the twelve cells are **abstract
superclasses** (`AbstractMap`, `AbstractCollection`) and `RandomAccess`. The
probe reported its own reach, not the defect. `a-narrow-probe-reports-its-own-reach`
is a standing note in this project and it still cost me a wrong conclusion I
nearly published.

**Two things the same run establishes in passing:**

* **`H15-3`'s `Function.andThen` stand-in is real and visible by class name:**
  `f.andThen(g).getClass()` is `java.util.function.Function$AndThen` on
  CratonVM against `Function$$Lambda/0x…` on HotSpot.
* `Comparator.naturalOrder().reversed()` yields
  `java.util.Collections$ReverseComparator2` where HotSpot on the *same JDK*
  yields `Collections$ReverseComparator`. Not investigated, not in any record I
  can find, and noted here only so it is written down somewhere.
