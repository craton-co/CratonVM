# H23-2 — the bucket table is typed per RECEIVER, and the two families that wanted it most were declined

**Status:** LANDED as source (`603f2962f`), **NOT BUILT and NOT RUN.** This lane
cannot build. Everything about the *premises* is MEASURED on
`C:/craton/cratonvm-r8.exe` and HotSpot 25.0.3+9; everything about the *effect*
is ARGUED. §5 says exactly which is which and §6 predicts what a build should
show.

---

## 1. The change

`native-collections/src/lib.rs`. `H16` made `HashMap`'s nodes real; the array
holding them was still allocated with the untyped sentinel, so
`table.getClass()` was `[Ljava.lang.Object;` on every non-empty map
(`H23-1` §3). One new function decides the component class **per receiver**, and
three allocation sites take it.

```rust
fn bucket_table_component(ctx: &mut dyn NativeContext, this: ObjectRef) -> ClassId
```

`alloc_bucket_table` grew a `component: ClassId` parameter; its two callers
(`native_map_init_capacity`, `lhm_init_with_cap`) and `map_resize` compute it
from their receiver.

**Three sites, not one.** The other two are where this would have silently half-
worked:

* the **capacity-fallback arm** of `alloc_bucket_table` — the path taken when
  the requested table does not fit and the map starts at the default size — spelled
  its allocation `alloc_ref_array`, which hard-codes the sentinel. A map that lost
  the capacity race would have kept an `Object[]` while every sibling got a typed
  one: a divergence that appears only under memory pressure.
* **`map_resize`** allocated the grown table with `alloc_ref_array` too. Without
  it, a table would be typed at construction and revert to `Object[]` the first
  time the map crossed its load factor — i.e. in exactly the maps large enough
  for anyone to notice. `H23-1`'s case C (40 puts, `len=64`) is that shape.

Class resolution can load a class and therefore allocate, so each site resolves
the component behind its existing pin and re-reads the receiver afterwards.

---

## 2. Why it is per-receiver — MEASURED, one case per process

`scratchpad/h23/Family.java`, HotSpot 25.0.3+9 vs CratonVM r8 `--jdk-only`:

| family | HotSpot table | HotSpot node | CratonVM table | CratonVM node |
|---|---|---|---|---|
| `HashMap` | `HashMap$Node[]` | `HashMap$Node` | `Object[]` | `HashMap$Node` |
| `LinkedHashMap` | `HashMap$Node[]` | `LinkedHashMap$Entry` | `Object[]` | `LinkedHashMap$Entry` |
| `Hashtable` | `Hashtable$Entry[]` | `Hashtable$Entry` | `Object[]` | **`HashMap$Node`** |
| `Properties` | `Hashtable$Entry[]` | *(table null)* | `Object[]` | — |
| `ConcurrentHashMap` | `CHM$Node[]` | `CHM$Node` | **null** | — |
| `WeakHashMap` | `WeakHashMap$Entry[]` | `WeakHashMap$Entry` | **`WeakHashMap$Entry[]`** ✓ | ✓ |
| `IdentityHashMap` | **`Object[]`** | *(flat)* | `Object[]` ✓ | ✓ |

Four things fall out of that table, and each is a rule in the code:

1. **`LinkedHashMap` shares `HashMap`'s inherited `table` field**, so its
   component is `HashMap$Node` and *not* `LinkedHashMap$Entry`. One type serves
   both callers.
2. **`IdentityHashMap` is legitimately `Object[]`** — it stores keys and values
   flat rather than in nodes. The sentinel is the *right* answer there. A sweep
   that treated every `Object[]` table as a defect would have broken it.
3. **`WeakHashMap` is already fully correct** — and the reason is worth stating,
   because it is not a success of this crate: `WeakHashMap` is **not natively
   modelled** (`is_native_bucket_map`'s doc excludes it), so real
   `WeakHashMap.java` bytecode allocates its own `new Entry[cap]`. It is an
   existence proof that real JDK map bytecode runs on this VM and produces a
   properly typed table.
4. **`ConcurrentHashMap`'s table is `null`** for an ordinary 2-entry map on r8.
   `chm_publish_real_table` types it correctly but only fires from specific
   entry points. Separate defect, nominated.

---

## 3. The two declines, and why declining was the correct half of the fix

### 3a. `Hashtable`/`Properties` — the node half is not done

`Hashtable` wants `[Ljava/util/Hashtable$Entry;`. This VM's `Hashtable` nodes
are **`HashMap$Node`** (row 3 above). Typing that table would have produced
exactly the armed hybrid this whole lane exists to avoid. MEASURED, both VMs
agree (`scratchpad/h23/Covar.java`, case `HT`):

```
Hashtable$Entry.getSuperclass()                     = java.lang.Object
Hashtable$Entry.isAssignableFrom(HashMap$Node)      = false
```

`HashMap$Node` is not a subclass of `Hashtable$Entry` by any route, so a real
`Hashtable.rehash()` storing our node into a typed table throws
`ArrayStoreException`. **Node-class-first applies again**, one family over.
Declined, and nominated in §7.

This is the finding I would most want a reader to take from this record: the
same defect shape `H16` fixed for `HashMap` is *still open* for `Hashtable`, and
it was invisible until the array question was asked.

### 3b. `ConcurrentHashMap` — a different class, already handled

The arrays reachable through `alloc_bucket_table`/`map_resize` for a CHM
receiver are its per-**segment** tables, a CratonVM-internal shape with no
HotSpot counterpart. Typing those `HashMap$Node` would be *inventing* a
component type, not restoring one. The real CHM table is built and typed by
`chm_publish_real_table_pinned`, which already does this correctly.

---

## 4. The invariant, and the one thing that could still break it

> **A table may be typed only if every node that can enter it is real.**

### 4a. The covariance premise is measured, not assumed

The `LinkedHashMap` row types a table `HashMap$Node[]` and fills it with
`LinkedHashMap$Entry`. That is only legal if this VM records the subclass edge.
It does — MEASURED on **CratonVM r8** and HotSpot alike, identical output:

```
LinkedHashMap$Entry.getSuperclass()              = java.util.HashMap$Node
HashMap$Node.isAssignableFrom(LinkedHashMap$Entry) = true
Array.newInstance(HashMap$Node, 4)               -> [Ljava.util.HashMap$Node;
Array.set(arr, 0, <a live LinkedHashMap$Entry>)  -> ok, no throw
```

That last line is the exact store my change makes the VM perform, executed
against a genuinely typed array on the target VM. It is the strongest evidence
in this record.

### 4b. The residual: `map_node_class_for` can still fabricate

`map_node_class_for` returns the sentinel when the key or value is not
`Value::Object(_)`, because a real `$Node` declares both as references and
`coerce_field_value_by_descriptor` turns a primitive written at a reference slot
into `NULL`. So the fabrication is **load-bearing for primitive values** — it
cannot simply be removed.

That leaves a theoretical hybrid: a primitive-valued put into a now-typed table.
**I could not reach it from Java.** MEASURED, 14 shapes, one case per process,
all on r8 `--jdk-only`, counting real vs fabricated nodes by walking every chain:

| probe | shapes | fabricated nodes found |
|---|---|---|
| `TableComp` A–E | empty · String→String · 40-entry resize · boxed `Integer` value · `null` value | **0** |
| `SetComp` S1–S4 | `HashSet` of String · **of `null`** · null + String · boxed `Integer` | **0** |
| `IntVal` J1–J5 | `int`→`int` unmaterialised · materialised · 20 of them · + resize to `len=64` · Object→boxed-int | **0** |

`HashSet.add(null)` (S2/S3) was the strongest candidate — `map_node_class_for`'s
own doc names `present_marker`'s legacy `Int(1)` as a producer — and it still
yielded a real `HashMap$Node`. `Value::Object(None)` *matches*
`Value::Object(_)`, so a Java `null` keeps the real node; that is why.

J2/J3/J4 are the other candidate: `int`→`int` mappings go to a side store
(J1 shows `size=1` with `occupied=0` — an all-`Integer`-keyed map has an **empty
table**) and are re-put verbatim through `materialize_hm_int_fast` when a
non-`Integer` key arrives. All 41 materialised nodes in J4 were real.

**This is not a proof.** `0 in 14` does not close it. The remaining producers are
the *Rust-private* ones `map_node_class_for` names — the 42 direct
`native_map_put_pub` call sites "not obliged to store an object" — and I could
not enumerate their receivers without building. **The invariant is measured, not
enforced.** The enforcement patch is written out in §7.1.

---

## 5. What I did NOT verify

* **The change does not compile.** It is `rustfmt` parse-clean (0 parse errors;
  the 8 diff lines are pre-existing formatting the repo knowingly carries). That
  is a PARSE check, not a type check — `scripts/merge-parse-check.sh`'s own
  header says so at length. Nothing here has been type-checked or run.
* **No arm was run.** Not `--jdk-only`, not `SUITE=all`, not `SUITE=core`. The
  binary I probed (`r8`, `025780ff7`) does not contain this change.
* **The after-state of the probe is unmeasured.** Every `[Ljava...` figure in
  this record is a *before*.
* **`Properties` was not separated from `Hashtable`.** `is_hashtable_receiver`
  returns true for both and both are declined; JDK 25 backs `Properties` with a
  side CHM, so it may need a different answer entirely.
* **I did not check the JIT's map fast paths.** `jit_hashmap_put_direct` is named
  in `map_node_class_for`'s doc as having its own direct call sites; whether any
  of them allocates a bucket table independently of these three sites is unknown
  to me.
* **The eager-vs-lazy table divergence is untouched** (`H23-1` §3a).

---

## 6. Prediction, and what would falsify it

**Predicted — corpus:** *nothing moves.* `--jdk-only` **105/105**, `SUITE=all`
**103/105** (`RJdkFunctionCombinators`, `RServiceLoaderDoubleSource`),
`SUITE=core` **65/65**, `RMapGcStress` still a **TIMEOUT (`rc=124`, ~233 s
against a 120 s budget)** and not an objection.

The reasoning is the standing trap, applied to myself: **the corpus asks nothing
about component types**, so a green arm is not evidence that this worked — and
by the same token there is no corpus row this *should* move. A green run is a
non-regression check, nothing more.

**Predicted — probe:** `H23-1` §3's table changes in exactly two rows, cases
B–E, `HashMap` and `LinkedHashMap`:

```
  tableCls   [Ljava.lang.Object;   ->   [Ljava.util.HashMap$Node;
```

Case A (empty map) also becomes typed — which does *not* match HotSpot, where it
is `null`. `Hashtable`, `Properties`, `ConcurrentHashMap`, `IdentityHashMap` and
`WeakHashMap` must be **unchanged**.

**Falsifiers, in the order I would check them:**

1. **Any `ArrayStoreException` naming `cratonvm/synthetic/AnonymousObject$N`.**
   That is §4b's residual firing, and it means the Java-unreachable claim is
   wrong. Most likely in a map-heavy suite (Spring, WildFly, Hibernate) rather
   than the corpus.
2. **`IdentityHashMap` or `WeakHashMap` changing at all.** Both are correct
   today; either moving means `bucket_table_component`'s receiver predicates are
   mis-routing.
3. **`Hashtable`'s table becoming typed.** The decline in §3a failed, and a
   `Hashtable.rehash()` will throw.
4. **A map-shaped `ClassCastException` in `map_resize`.** The resize site is the
   one where I changed an allocation that was previously reached with a
   *guaranteed* `Object[]`; if any consumer keys on the old component, it fires
   there first.
5. **`RMapGcStress` turning into a crash rather than its usual timeout.** The
   resize path is GC-sensitive and I added a class resolution (hence a possible
   allocation) inside its pin region. I believe the re-reads cover it; a
   stale-objref crash would say they do not.

Falsifier 5 is the one I am least confident about, because it is the only place
I introduced an allocation into an existing pinned region rather than following
one.

---

## 7. NOMINATIONS

1. **Enforce §4b's invariant rather than relying on its measurement.** At the
   single node-allocation choke point (`native_map_put_evict_pinned`, the only
   `map_node_class_for` call site), before the node is allocated:

   ```
   if node_cid == ClassId::new(0)
       && ctx.class_id_of_object(buckets) != ClassId::new(0)
   {
       // A fabricated node is about to become a chain HEAD in a typed table.
       // Retype the table to Object[] and republish, THEN allocate the node:
       //   let untyped = ctx.new_ref_array(ClassId::new(0), cap);
       //   copy every element across with get/set_array_element
       //   publish_map_table(ctx, this, untyped, cap as i32)
       // Allocating, so re-read `this`/`buckets`/`key_val`/`value` from their
       // existing pins (this_pin, buckets_pin, key_pin, value_pin) afterwards.
   }
   ```

   Note `class_id_of_object` on a **reference array returns the COMPONENT class**
   (`typecheck::array_descriptor_of`, and `aastore_element_assignable`'s comment
   states it), so the guard is one comparison and no allocation on the hot path.
   Only a chain HEAD ever enters the array — a node linked via `NODE_FIELD_NEXT`
   never does — so this is the complete set of stores that matter.
   **I did not write this**, because an unexercised, GC-sensitive, unbuildable
   reallocation path is a worse risk than the measured residual it guards.

2. **`Hashtable`'s nodes are `HashMap$Node` where HotSpot has
   `Hashtable$Entry`** (§3a). This is a *node*-half defect of exactly the kind
   `H16` closed for `HashMap`, still open one family over, and it blocks typing
   `Hashtable.table`. Fix the node class first, then flip the decline in
   `bucket_table_component`. Same file, so a future lane in
   `native-collections` owns it.

3. **`ConcurrentHashMap.table` is `null` for an ordinary map** (§2 row 5).
   `chm_publish_real_table` produces the right thing but does not fire on the
   plain construct-and-put path.

4. **`HashMap.table` is eagerly allocated where HotSpot allocates lazily**
   (`H23-1` §3a). After this change CratonVM presents a typed but *non-null*
   empty table where HotSpot presents `null` — the change makes case A closer in
   component type and no closer in nullness.

5. **The `new_ref_array` sentinel outside this crate is untouched and much
   larger**: `lang_class.rs` 30 sites, `jmx.rs` 20, `t27_tls.rs` 13,
   `beans_jndi.rs` 7. A `Class[]`/`Method[]`/`Field[]` handed back as `Object[]`
   is the same defect in the reflection surface, and far more likely to be
   *observed* by real code than `HashMap.table` is. See `H23-3`.
