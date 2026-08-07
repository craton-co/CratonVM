# `--jdk-only`: the snapshot iterator that stands in for `HashMap$KeyItr` had no `remove()`

**Status:** FIXED in source 2026-08-06 (lane L13, JDK-only wave 2). Not verified
against a binary — this lane could not build or run. Companion:
[`L4-jmx-iterator-remove-default-method.md`](L4-jmx-iterator-remove-default-method.md),
which diagnosed this half and fixed the independent `--real-jdk` half of
`RJdkJmx`.

## The chain

`regression-suite/src/RJdkJmx.java:133` under `--jdk-only`:

```
javax/management/NotCompliantMBeanException: sun.management.GarbageCollectorImpl: remove
Caused by: java/lang/UnsupportedOperationException: remove
    at com/sun/jmx/mbeanserver/MXBeanSupport.findMXBeanInterface(MXBeanSupport.java:95)
    at java/util/Iterator.remove(Iterator.java:102)
```

`Iterator.java:102` is the body of the interface **default** method, whose whole
text is `throw new UnsupportedOperationException("remove")`. Every link
re-verified on this host against JDK 25.0.3 (`javap -p`, `javap -p -c --module
java.management`), not from the brief:

1. `com.sun.jmx.mbeanserver.MXBeanSupport.findMXBeanInterface` builds a
   `Util.newSet()` (a plain `HashSet`) of candidate MXBean interfaces and
   reduces it in place. The bytecode carries, in order:
   `Set.size:()I` (the loop guard), three `Set.iterator:()Ljava/util/Iterator;`
   sites, and at bci 156

   ```
   invokeinterface java/util/Iterator.remove:()V
   ```

   **The constant-pool class is `java/util/Iterator`.** That single fact decides
   the registration breadth below.
   `sun.management.GarbageCollectorImpl` implements two MXBean interfaces
   (`GarbageCollectorMXBean` extends `MemoryManagerMXBean`), so the reduce loop
   is entered and `it.remove()` is reached.

2. `HashSet.iterator()` is CratonVM's `native_hs_iterator`
   (`native-collections/src/lib.rs`). Under `--jdk-only` its
   `try_alloc_synthetic("java/util/HashMap$KeyItr", …)` is refused — correctly;
   no JDK declares that class, the real one is `HashMap$KeyIterator` — and it
   falls back to `real_snapshot_iterator`.

3. `real_snapshot_iterator` returns a real `java.util.Arrays$ArrayItr`. Verified
   on JDK 25.0.3:

   ```
   class java.util.Arrays$ArrayItr<E> implements java.util.Iterator<E> {
     private int cursor;
     private final E[] a;
     java.util.Arrays$ArrayItr(E[]);
     public boolean hasNext();
     public E next();
   }
   ```

   **No `remove()`.** So `it.remove()` resolves to the throwing interface
   default and the MBean registration dies.

4. Compatible mode survives because two things line up there and neither does in
   strict: the fabrication succeeds, giving an iterator with a backing pointer
   in `MAP_KEY_ITR_FIELD_BACKING`; and the force-native entry for
   `("java/util/Iterator","remove","()V")`
   (`vm/src/runtime/interpreter/native_override.rs`) routes the call to
   `native_itr_remove_noop`, which dispatches by receiver class onto
   `native_map_key_itr_remove`. In strict both halves fail, and the second fails
   *even if the first is fixed*: `native_itr_remove_noop` had no
   `Arrays$ArrayItr` arm, and its registration was `Bridge`, which
   `resolve_native_dispatch_wave1` (`vm/src/vm/vm_exec.rs:824`) sends to the real
   bytecode whenever bytecode exists — and `admit_forced_native_id` passes
   `bytecode_available: true` by construction.

## Why the cheap fix is wrong

`findMXBeanInterface`'s reduce loop is

```java
while (candidates.size() > 1) {
    for (Class<?> intf : candidates)
        for (Iterator<Class<?>> it = candidates.iterator(); it.hasNext(); ) {
            ...
            it.remove();
            continue reduce;
        }
    ...
    throw new IllegalArgumentException(... "implements more than one MXBean interface");
}
```

`remove()` **must write through to the backing `HashSet`**, because `size()` is
the loop's termination condition. Wrapping the snapshot in a mutable
`ArrayList` and handing back *its* `ArrayList$Itr` — the obvious one-liner —
mutates a private copy, leaves `size()` unchanged, and converts a clean failure
into a livelock or an `IllegalArgumentException`. That is the trap; it is not
taken here.

## What changed

All in `native-collections/src/lib.rs`.

### 1. A GC-rooted side table for the backing collection

`Arrays$ArrayItr` has exactly two fields and no room for a backing pointer, so
the pointer lives beside the object:

```rust
fn snapshot_itr_backing_table() -> &'static Mutex<StdHashMap<usize, SnapshotItrBacking>>
struct SnapshotItrBacking { backing: ObjectRef, route: SnapshotItrRoute, last_removed_cursor: i32 }
enum SnapshotItrRoute { SetLike, TreeSet }
```

keyed by `widened_obj_key(iterator)` — the same GC-stable
`(identity_hash << 32) | generation` key every other overlay in this crate uses.
The route is recorded by the *creator*, which knows whether the collection is
set-shaped or a native `TreeSet`, rather than re-guessed at removal time.

`last_removed_cursor` replaces `MAP_KEY_ITR_FIELD_LAST_RET`. It can, because
`ArrayItr.next()` is real bytecode (`return a[cursor++]`) and nothing of ours
runs there: the element last returned is always `a[cursor - 1]`, and the JDK's
`lastRet < 0` guard becomes "cursor is 0" (never called `next()`) or "cursor is
unchanged since the last removal" (called `remove()` twice).

`real_snapshot_iterator` grew a `backing: Option<(ObjectRef, SnapshotItrRoute)>`
parameter; `native_itr_remove_noop` grew an `"java/util/Arrays$ArrayItr"` arm
routing to the new `native_snapshot_itr_remove`, which deletes `a[cursor - 1]`
from the backing through the *existing* removal paths (`native_ksv_remove` /
`native_hs_remove` / the newly extracted `ts_remove_element`) and does **not**
rewind `cursor` — `a` is a snapshot, so rewinding would re-visit an element.
This mirrors `native_map_key_itr_remove` exactly.

### 2. How the side table is GC-rooted (and swept)

This crate has recorded bugs where an object-keyed side table was rooted in one
collector path and not another (`tm_fast_table`, `LinkedHashMap$Node`), so the
new table is wired into **all four** places a `widened_obj_key`-keyed table has
to appear, not just the root scan:

| hook | what it does for this table |
| --- | --- |
| `for_each_overlay_ref` | the single funnel behind `gc_scan_collection_overlay_roots` (root scan, moving young) **and** `gc_update_collection_overlay_refs` (post-move remap). Walked on both `for_rooting` settings — a live iterator whose backing was reclaimed would remove into freed memory. |
| `gc_overlay_roots_for_collection_with_keys` | the precise per-owner rule used by the non-moving young marker and `old_gen_gc`'s mark BFS. The owner is the **iterator**, so the backing is retained exactly while the iterator is reachable — a dead iterator cannot call `remove()`. |
| `gc_prune_dead_collection_overlays` | batched removal of dead keys, alongside `ll_overlay` / `lhm_overlay` / `tm_*` / `ts_array_table` / `cslm_comparator_table`. |
| `clear_overlay_entries_for_key` | the recycled-identity path in `widened_obj_key` plus the prune's per-key clear. |

Both halves survive a moving young collection:

* the **stored ref** is remapped by `gc_update_collection_overlay_refs` through
  `for_each_overlay_ref`, and rooted by either the whole-table scan or the
  per-owner rule depending on which collector is running;
* the **key** is relocation-invariant by construction (an identity hash travels
  with the object header), and `overlay_owner_keys` — the address→key reverse
  index the per-owner rule consults — is rebuilt onto post-move addresses by the
  same remap.

The sweep is not optional here. This is the highest-turnover table of the set:
one entry per `HashSet.iterator()` in strict mode, and every entry *roots a
collection*. Skipping the prune would pin dead collections, not merely leak a
few words.

`real_snapshot_iterator` also pins `backing` across every allocation it performs
and reads it back through the pin before storing it, so the table can never be
seeded with a from-space reference.

**No GC-crate change is needed.** `register_gc_root_provider` already publishes
this crate's `scan` / `roots_for_owner` / `roots_for_matching_owners` / `remap` /
`prune` closures to `cratonvm_gc::external_roots`, and all five reach the new
table through the funnels above.

### 3. Registration breadth: `Intrinsic` on `java/util/Iterator.remove()V`

`register_iterator_protocol_natives` now registers that one triple with
`register_with_kind(…, NativeKind::Intrinsic)` — the crate's only non-`Bridge`
registration. `ListIterator.remove`/`set`/`add` deliberately keep the block's
ambient `Bridge`.

**A narrower registration was considered and does not work.** The obvious
narrower gate is `("java/util/Arrays$ArrayItr","remove","()V")`, but the
force-native site keys on the *declaring* class of the resolved method
(`dispatch_virtual.rs`: `force_native_over_real_jdk_bytecode(declaring_name, …)`)
or on the constant-pool class, and the `javap` above shows the CP entry is
`InterfaceMethod java/util/Iterator.remove:()V`. A receiver whose class declares
no `remove()` resolves to the `Iterator` default, so `java/util/Iterator` is the
only name the lookup will ever present. A registration on `Arrays$ArrayItr`
would never be consulted.

**§1.4 justification.** `Intrinsic` is the "may shadow real bytecode" exception
and needs a reason:

* The bytecode it shadows is the `Iterator.remove()` default method, whose body
  is an unconditional `throw`. Strict mode's usual preference — "the real class
  bytes are authoritative" — buys nothing here, because the real bytes are a
  refusal.
* The receiver only reaches that refusal because of a *previous* CratonVM
  substitution: `java.util.HashSet` is allocated in CratonVM's own compact
  layout, so `HashSet.iterator()` is intercepted and the real
  `HashMap$KeyIterator` (whose `remove()` is inherited from
  `HashMap$HashIterator` and works) can never be constructed. The Intrinsic
  restores an implementation the VM itself removed.
* The retirement path is named and unchanged: stop giving `java.util.HashSet` a
  CratonVM layout under `--jdk-only`, at which point `iterator()` is not
  intercepted, the real `HashMap$KeyIterator` runs, and this registration can go
  back to `Bridge` and then away. That is a lane-scale change, not a patch.

**Why the breadth is safe: the native falls through.** ~~`Intrinsic` means the~~
The registration plus the force-native list route *every* `Iterator.remove()` in
the VM to
`native_itr_remove_noop`, in both modes.
(**Corrected 2026-08-07:** the breadth is not something `NativeKind::Intrinsic`
causes. On the cold interpreter paths registration alone already routes the
triple, and `Intrinsic` only exempts the native from the `--jdk-only` yield —
[§1 of *Natives over real JDK
classes*](../../architecture/natives-over-real-jdk-classes.md). The breadth
claim itself, and therefore the fall-through argument, is unchanged.)
A native that answered `UnsupportedOperationException`
for all of them would be far worse than the bug. So after the by-class match
misses, the dispatcher now hands the call to the receiver's own real bytecode
via `invoke_virtual_bytecode_only`, under two guards:

* `args.len() == 1` — the same callback is also registered for
  `ListIterator.set(Object)` and `.add(Object)`; a 2-argument call must not be
  turned into a `remove()`.
* `!is_class_synthetic_stub(cn) && method_exists(cn, "remove", "()V")`.
  `method_exists` walks the **superclass chain only, not interfaces**, which is
  exactly the discrimination needed: `HashMap$KeyIterator` answers `true` (via
  `HashIterator`), a real `Arrays$ArrayItr` answers `false`, and a receiver whose
  only `remove()` is the throwing default answers `false` and gets the identical
  UOE without a pointless re-entry. `method_exists` also answers `true` for a
  compatibility stub (whose methods live in the native registry, not in a class
  file) where there is *no* bytecode to reach — hence the stub half of the test.

The net effect on strict mode is that the Intrinsic is behaviour-neutral for
every receiver except the one CratonVM substituted. In Compatible mode the
fall-through is the only behaviour change, and it is strictly an improvement:
receivers outside the match list previously got an unconditional
`UnsupportedOperationException` and now run their own real `remove()`.

*Known adjacent wart, deliberately not touched:* `native_itr_remove_noop` routes
`ListIterator.set`/`add` by class too, so `set(Object)` on an `ArrayList$Itr`
receiver reaches `native_al_itr_remove`. That predates this change, is unaffected
by it (those registrations stay `Bridge`), and is out of scope.

### 4. The other `real_snapshot_iterator` callers

Four call sites, and the write-through is correct or inert for each:

| caller | backing passed | `remove()` behaviour |
| --- | --- | --- |
| `make_iterator_from_array` (over-allocated array path) | `None` | UOE — correct. It is handed a bare `Object[]` and knows of no collection; the real `Arrays.asList(a).iterator()` throws here too. |
| `native_hs_iterator` | `Some((this, SetLike))` | writes through to the `HashSet` / key-set view / `ConcurrentHashMap$KeySetView`, via the same `is_key_set_view` discrimination `native_map_key_itr_remove` uses. This is the defect. |
| `native_ts_iterator` | `Some((this, TreeSet))` | writes through to the native `TreeSet`, reaching parity with the fabricated `TreeSet$Itr` (field 2 = owning set) it stands in for. |
| `native_ts_descending_iterator` | `Some((this, TreeSet))` | same. Removal is by **element**, not by index, so the reversed snapshot changes nothing. |

`alloc_real_snapshot_iterator_of` — the shared helper, also called from
`native-builtins/src/util_concurrent_ext.rs` to mint a
`CopyOnWriteArrayList$COWIterator` — is **unchanged**. That iterator records no
backing, its class is not in the match, and the real `COWIterator` declares its
own `remove()` that throws, so the new fall-through runs that real bytecode and
the answer is the JDK's own `UnsupportedOperationException`. Genuinely
immutable backings keep throwing, which is correct behaviour and not a bug.

## Baselines that move (not this lane's files)

One registration changes kind, `Bridge` → `Intrinsic`. No registration is added
or removed, so `total_rows` is unchanged at 11876.

* `scripts/baselines/jdk-only-kind-map-25-linux.tsv:6772` —
  `java/util/Iterator	remove	()V	0	bridge	0	1` becomes `… intrinsic …`.
  A kind change on an existing native is exactly what that gate exists to
  surface; it is adjudicated here, not silent.
* `scripts/baselines/jdk-only-bridge-ratchet.json` — every affected counter goes
  **down**, so `scripts/jdk-only-bridge-ratchet.py` (which fails only on
  `observed > frozen + slack`) passes unchanged. For an accurate re-freeze:
  `bridge.rows` 10304 → 10303, `bridge.shadows_bytecode` /
  `bridge_shadows_bytecode` 4581 → 4580, `bridge.without_acc_native` /
  `bridge_without_acc_native` 9528 → 9527, `registrations.bridge` 10304 → 10303,
  `registrations.intrinsic` 681 → 682.

L4's `--real-jdk` fix moves the same file in the opposite direction (+8 bridge
rows). Re-take the census once, with both landed, rather than twice.

`native-builtins/tests/stub_ratchet.rs` counts `SyntheticStub` only and is
untouched.

## How to verify

```sh
cargo build --release -p cratonvm-cli

javac -d regression-suite/build regression-suite/src/RJdkJmx.java
java -cp regression-suite/build RJdkJmx                          # HotSpot 25 oracle
target/release/cratonvm --jdk-only  -cp regression-suite/build RJdkJmx
target/release/cratonvm --real-jdk  -cp regression-suite/build RJdkJmx
```

* The `--jdk-only` arm must stop dying at `RJdkJmx.java:133` with
  `NotCompliantMBeanException: … : remove`. With L4's half landed too, both arms
  must reach `PASS RJdkJmx (49 checks)`, byte-identical to HotSpot.
* The `--real-jdk` arm is the regression check for the fall-through: it must be
  unchanged from before this commit, because the Intrinsic is a no-op in
  Compatible mode (a registered native already won there) and the only new
  behaviour is that receivers outside the match list now run their own
  `remove()` instead of throwing.
* Strict-mode collection regression, since `native_hs_iterator` and both TreeSet
  iterators changed shape:
  `target/release/cratonvm --jdk-only -cp <probes> JdkOnlyCollectionViewProbe`
  and `StrictIterPrimitivesProbe` must be unchanged.

## The single observation that would falsify this

A `--jdk-only` run that reaches `RJdkJmx.java:133` and now hangs, or fails with
`IllegalArgumentException: … implements more than one MXBean interface`, instead
of throwing. Either would mean `native_snapshot_itr_remove` ran but did **not**
write through — the reduce loop's `candidates.size()` never dropped — which
would point at the recorded `backing` being the wrong object (a snapshot copy of
the set rather than the set itself) rather than at the dispatch chain, and would
leave every claim above about registration and rooting intact.

The second-best falsifier: a strict run with `CRATONVM_HS_ITR_DBG=1` showing
`native_hs_iterator` was never called on this path, which would mean the
receiver is some other iterator entirely and this document names the wrong
producer.
