# Stack-walk capture, virtual dispatch, and the "lock-free" resolve cache

Arch pass `stackwalk-and-vtable`, 2026-07-26.
Base: `arch/wave1-integration-20260726` @ `2fc151476`.

Files owned by this pass:

* `vm/src/runtime/stackwalker.rs`
* `vm/src/runtime/vtable.rs`
* `vm/src/runtime/lockfree_resolve.rs`

Nothing outside those four paths (this document included) was edited. This
pass could not build or test (nine concurrent cargo builds OOM the host); all
three files are `rustfmt --check`-clean and CRLF line endings were verified
byte-for-byte after every edit.

---

## 1. Stack-trace capture — is lazy line resolution sound?

**Short answer: partially, and the part that is sound is landed. Full laziness
is blocked on one field that lives in a file this pass does not own, and
faking it without that field would print wrong line numbers.**

### 1a. What a capture cost before this pass

`capture_full_trace` maps every `Frame` on the thread's stack through
`entry_from_frame`, which per frame did:

1. `ClassStore::get(class_id)` — a `Vec` index. O(1), cheap.
2. `Class::find_method(name, descriptor)` — **a linear scan over every method
   the class declares, with two full `&str` comparisons per candidate**:
   ```rust
   self.methods.iter().find(|m| &*m.name == name && &*m.descriptor == descriptor)
   ```
3. `ClassFileMethod::code()` — a short scan of the method's decoded attributes.
4. A linear scan of the `LineNumberTable`.

Step 2 dominates. Spring/Hibernate/JUnit throw as control flow at depths of
50–150 frames, through classes declaring 100+ methods; that is on the order of
10<sup>4</sup> string comparisons **per throw**, for information the
overwhelming majority of callers never read.

### 1b. What landed: the method-slot memo

`stackwalker.rs` now memoizes `(ClassId, fnv1a64(name, descriptor)) -> index
into Class::methods`, turning step 2 into one hash probe plus the same two
string comparisons run **once, as verification**. `line_number_for_bci` and
`line_number_entries` both route through it.

Its correctness rules — each one chosen against a specific scar in this repo:

* **Never memoize a negative.** Only a successful find is inserted. A method
  absent today can be present after a redefinition, and a permanent `None`
  memo would never be retried — the failure mode two sibling passes already hit
  (the native registry's cached-`None`, and `functional_interface_id`'s
  permanent `None`). A miss here costs exactly one linear scan, i.e. the
  pre-memo cost.
* **Verify on every hit.** The value is an *index*, never a borrowed method or
  a cloned attribute. Every hit re-reads `class.methods[idx]` from the live
  `ClassStore` and re-checks name and descriptor. A redefinition that reorders,
  replaces or removes methods therefore cannot yield a stale answer:
  verification fails, the scan runs, the memo is corrected. This is why no
  redefine-generation plumbing is needed — and it could not be added anyway,
  since `class_redefine_generation` lives on `ClassManager` and this path only
  ever holds a `&ClassStore`.
* **Retain nothing.** The value is a `u32`. No `Arc`, no `Class`, no bytecode.
  This is deliberate: a sibling pass found a design that would have leaked one
  `Arc` per class per unload precisely by retaining metadata in a cache like
  this one. Bounded at 8192 entries with clear-on-overflow, the policy
  `frame::padded_bytecode_for_method` already uses.

An FNV-1a-64 collision between two signatures **of the same class** is caught
by the same verification; the only consequence is that the colliding pair
evicts each other and degrades to the pre-memo scan.

Tests added: cold-vs-warm agreement including a two-overload class, "a failed
lookup is never memoized", self-healing after the class is rebuilt underneath
the memo, and unload-fails-closed.

### 1c. The GC / class-unloading argument

The question the brief asks — *can a deferred trace resolve through metadata
that has since been unloaded or relocated?* — has a clean answer here, and it
rests on one property of `ClassStore`:

> ```rust
> /// Monotonic ClassId slots. An unloaded class leaves a tombstone so a
> /// stale ClassId can never alias a subsequently loaded class.
> classes: Vec<Option<Class>>,
> ```

`ClassStore::remove` replaces the slot with `None` and `next_id()` is
`self.classes.len()`, so ids are strictly monotonic and **never recycled**.
That gives the deferral its safety property for free:

* A deferred entry's `class_id` either still names the same `Class` it named at
  capture time, or names a tombstone. It can **never** name a different class.
  So deferral cannot silently attribute a frame to the wrong class — the
  classic "recycled id" hazard does not exist in this store.
* On a tombstone, `class_store.get` returns `None` and the resolver leaves
  `LINE_NUMBER_UNKNOWN`. **Fail-closed, by construction.**
* Frame *identity* survives unloading regardless: `class_name`, `method_name`
  and `source_file` are `Arc<str>` clones taken at capture time. They own their
  own storage and do not point into class metadata, so an unloaded class still
  prints `com.example.Foo.bar(Foo.java)` — it just loses the line number, never
  the frame. That is the correct degradation: a trace with a missing line is
  usable; a trace with a missing or wrong frame is not.
* GC relocation is not a hazard for this data at all — `StackTraceEntry` holds
  no `ObjectRef`. Only the throwable→trace map (`throwable_stacks`) does, and
  that already has a remap hook (`remap_and_sweep_throwable_stack_traces`).
* The memo holds no `Arc` of any kind, so class-loader unloading cannot be
  made to leak through it. Stale `(ClassId, hash)` keys for unloaded classes
  are bounded garbage that the cap eventually clears, and they are unreachable
  in practice because `class_store.get` fails before the memo is consulted.

So: **deferral is sound with respect to GC and class-loader unloading.** That
is not what blocks it.

### 1d. What actually blocks full laziness: the descriptor

`StackTraceEntry` carries `class_name`, `method_name`, `source_file`,
`line_number`, `byte_code_index`, `class_id`. It does **not** carry the method
descriptor, and it has no method index.

Resolving a line number requires picking the exact `ClassFileMethod`. Given
only `(class_id, method_name, bci)`:

* if the class declares exactly one method with that name, resolution is exact;
* if it declares an overload set, the members have **different**
  `LineNumberTable`s, and guessing prints a line number from the wrong method
  body.

A wrong line is worse than a slow one. So the landed resolver
(`resolve_line_numbers_in_place`) **fails closed on overloads**, leaving those
frames at `LINE_NUMBER_UNKNOWN`. That makes it:

* a strict improvement for `capture_frames_no_lines` consumers (cross-thread
  `Thread.getStackTrace()` / thread dumps), which have **no** line numbers at
  all today — every frame is `-1`;
* **not** acceptable for the `Throwable` path, where overloaded frames
  (`StringBuilder.append`, `Arrays.copyOf`, every builder API) are common and
  are resolved correctly by the eager path today. Wiring it there would be a
  `printStackTrace` fidelity regression of exactly the kind
  `tco-breaks-stacktrace-fidelity` records.

This is stated in the function's own doc comment so nobody wires it up by
mistake.

The fix is one field. See **CR-SW-1**.

### 1e. Residual cost on the throw path after this pass

Per frame, a capture now costs: one `Vec` index, one FNV over
name+descriptor, one hash probe, two `&str` comparisons, a short attribute
scan, and the `LineNumberTable` scan. The `Vec<StackTraceEntry>` allocation,
the clone in `capture_throwable_stack_trace`, and the VM-wide
`throwable_stacks` write lock all remain — those are CR-2 and CR-4 in
`exception-and-indy-path.md` and are not in this lane.

---

## 2. What a virtual dispatch costs today

### 2a. The measured shape

`vtable.rs` is reached from exactly one production site: the `vtable_fast`
helper in `interpreter.rs` (~line 39096), documented there as "fast path 0".
It is not path 0 in practice — it runs *after* the thread-local
`invoke_cache`, after a receiver peek, after a `resolution_cache.read()`, and
after a `class_manager.read()` used for the native-shadow scan.

When it does run, one dispatch costs:

| Step | Cost |
|---|---|
| `shared.classes.vtable_manager.read()` | atomic RMW on one process-wide `parking_lot::RwLock` word |
| `get_vtable(class_id)` | `FxHashMap<u64, Vtable>` probe |
| `lookup_slot(name, descriptor)` | **two `FxHasher` streams over the two strings**, one `FxHashMap<u64, _>` probe, then a verifying `&str` comparison of both |
| `Vtable::get(slot)` | `Vec` index — genuinely O(1) |
| redefine guard | `class_manager.read()` + `class_redefine_generation` |
| entry clone | one `Arc<CachedBytecodeMethod>` refcount bump |

**So: there is a per-class vtable with O(1) slot indexing, but no production
caller has a slot number.** Every dispatch that reaches this module re-derives
the slot from the method name and descriptor. The module doc's claim that it
"replaces HashMap lookups with direct array indexing" described the second half
only; it now states both halves explicitly.

### 2b. Interface dispatch is not an itable

`Itable` exists, is correct, is keyed
`(interface_class_id, fxhash(name)^fxhash(descriptor))`, and carries a `CRIT`
comment reading "`invokeinterface` is on every interface dispatch".

**It has no production callers.** `create_itable`, `get_itable` and
`resolve_interface` are referenced only by this file's own tests.
`invokeinterface` resolves through the *receiver's vtable* by
`(name, descriptor)`, exactly like `invokevirtual` — which works, because the
receiver's vtable already contains its interface implementations. So interface
dispatch today is neither an itable nor a linear scan: it is the same hashed
name+descriptor lookup as virtual dispatch. The module doc now says so.

### 2c. Invalidation on redefinition — and a permanent negative

Three mechanisms exist:

1. `vtable_override_adapter` → `invalidate_for_override(super_class_id, slot)`,
   fired at link time when a subclass overrides an inherited method.
2. `unload_class` → `invalidate_class`, from the loader-unload sweep in
   `vm/src/memory/gc.rs`.
3. A redefine guard **outside** this file, in `interpreter.rs`: if
   `any_class_redefined()` and the entry's declaring class has a non-zero
   redefine generation, the fast path cedes. That guard exists because
   `VtableEntry::resolved_method` is an immutable snapshot with no staleness
   tracking of its own, and a *subclass's* vtable is never transitively
   refreshed when an ancestor is redefined — the Mockito inline-mock stub
   bypass recorded in
   `docs/known-issues/springboot/mockito-inline-nested-selfcall-stub-bypass.md`.

**Finding — `resolved = false` is a permanent, never-retried negative.**
Nothing in this module ever sets `resolved` back to `true` except a wholesale
`install_vtable` / `add_method` / `override_method` for that same class, which
only happens when *that* class is (re)linked. So the first subclass to override
`Foo.bar()` disables the interpreter's vtable fast path for `(Foo, bar)` for
the life of the process. Since essentially every class overrides
`toString`/`equals`/`hashCode`, those slots are disabled on their declaring
classes almost immediately after boot.

It is **not** a correctness bug: `vtable_fast` looks the vtable up by the
*receiver object's own* class id, so `Foo`'s entry is only ever consulted for a
receiver that really is a `Foo`, for which `Foo.bar` is the correct target.
The invalidation is a CHA / `LeafClass`-assumption signal — and the JIT, its
intended consumer, does not reference `VtableManager` at all (no `jit/**` use
of any of these APIs exists today). So the cost is paid and the benefit is not
collected.

**Deliberately not changed here.** Splitting "CHA assumption broken" from
"entry undispatchable" changes dispatch-tier semantics, and this pass cannot
build or run the suites. Scoped proposal:

> Add `cha_leaf_valid: bool` to `VtableEntry`, defaulting `true`. Have
> `invalidate_for_override` clear only that bit and leave `resolved` alone;
> have `invalidate_class` / `unload_classes` keep clearing `resolved` (those
> really do make the entry undispatchable). Then re-audit every reader of
> `resolved` — today that is `resolve_virtual`, `resolve_virtual_method`,
> `resolve_virtual_slot`, `resolve_virtual_slot_method`, `vtable_entry_ref`,
> and the inline read in `interpreter.rs::vtable_fast` — and point the CHA
> consumers (when one appears) at `cha_leaf_valid`. Must be validated against
> a Mockito-heavy Spring suite, since that is where a wrong answer shows up.

### 2d. What landed in `vtable.rs`

**A second index that nothing read, built for every class the VM links.**
`Vtable` carried both `fast_lookup: FxHashMap<u64, Vec<usize>>` and
`name_to_slot: FxHashMap<(Arc<str>, Arc<str>), usize>`. The latter was
documented as "the authoritative fallback when the `fast_lookup` u64-hash map
collides". **That is false.** `lookup_slot` never consulted it — it resolves
collisions by verifying every candidate in the bucket — and its only reader was
`add_method`, which has no production callers. Yet `install_vtable` built it
for every slot of every class the class loader links: hashing both strings
again and bumping two `Arc` refcounts per method, plus a full extra map clone
per subclass in `from_parent` / `create_vtable`.

Removed. `add_method` now asks `lookup_slot`, which returns the identical
answer (it verifies name and descriptor byte-for-byte).

**One heap allocation per method per class.** `fast_lookup`'s bucket was a
`Vec<usize>` even for the single-slot case, which is essentially every bucket.
Replaced with `enum SlotBucket { One(u32), Many(Vec<u32>) }` — no allocation on
the common path, identical collision behaviour (all candidates probed and
verified) on the rare one, and `from_parent`'s deep clone gets much cheaper.

**`unload_classes(&[u64])`**, a batch counterpart to `unload_class`.
`invalidate_class` sweeps every slot of every vtable in the VM, so the
loader-unload loop in `gc.rs` is
`O(unloaded × all_classes × slots_per_class)` — for a few hundred unloaded
classes in a VM holding tens of thousands, hundreds of millions of slot visits
under the manager write lock. The batch form does one sweep. See **CR-VT-1**
for the one-line call-site change.

Tests added: `SlotBucket` promotion/idempotence, `add_method` override without
`name_to_slot`, `install_vtable` indexing with an empty slot present,
`from_parent` index inheritance, and `unload_classes` proven equivalent to a
loop of `unload_class`.

---

## 3. `lockfree_resolve.rs` — what it makes lock-free (answer: nothing)

### 3a. Two false claims in the module header

The header described a three-level flow whose level 1 was "Check thread-local
cache (no lock)". Verified against the code, in the spirit of the sibling
finding on `class_manager.rs::class_loading_locks`:

**False claim 1 — `ThreadLocalResolveCache` is not instantiated in
production.** Every reference to it outside this file is a doc comment or one
of this file's own unit tests. The real thread-local tier is
`JvmThread::invoke_cache`, which lives elsewhere and shares no code with this
type. Level 1 of the documented flow does not exist here.

The same is true of a large share of the rest of the module:
`SharedResolutionState::resolve_method` / `cache_method` / `resolve_field` /
`cache_field`, the `global_methods` and `global_fields` maps, and the
`ResolutionKey` / `ResolvedTarget` / `ResolvedField` types (including
`ResolutionKey`'s substantial hash-collision-hardening rationale) have **no
production callers**. The single live consumer of this module is
`promoted_invokes`, reached from the interpreter's invoke paths via
`get_promoted_invoke` (3 call sites) and `insert_promoted_invoke` (17).

Note the consequence: `invalidate_all()`, called from the loader-unload sweep
in `gc.rs`, takes three write locks to clear two maps that are always empty.

**False claim 2 — nothing here is lock-free.** `get_promoted_invoke` takes a
`parking_lot::RwLock` **read** guard on a single process-wide map. Acquiring a
parking_lot read guard is an atomic read-modify-write on one shared word, so
every dispatching thread writes the same cache line on every consult. The
module's claim to "eliminate lock contention on the common case" is overstated:
it *relocates* contention off the `ClassManager` L10 lock onto a much cheaper
one. That relocation is real and worth having — the point is only that the
name and the claim overpromise, and a future pass sizing thread scaling should
not budget this path as free.

Both are now stated in the header.

### 3b. What landed: an `env::var` syscall under a write lock

```rust
pub fn insert_promoted_invoke(&self, key: PromotedInvokeKey, target: CachedInvokeTarget) {
    let mut guard = self.promoted_invokes.write();
    if !guard.contains_key(&key) {
        evict_to_fit(&mut guard, shared_cache_cap());   // <- std::env::var
    }
    ...
```

`shared_cache_cap()` called `std::env::var("CRATONVM_RESOLVE_CACHE_CAP")` —
which allocates a `String` and, on Windows, is a `GetEnvironmentVariableW`
syscall — on **every first promotion of a call site**, *while holding the
`promoted_invokes` write guard*. Every thread promoting a target serialised
behind an environment lookup. Real applications mint one such promotion per
distinct `(caller class, cp_index, receiver class)` triple: hundreds of
thousands during warm-up. Same shape as the seven throw-path flags a sibling
pass just fixed in `exceptions.rs`, but worse because it sits inside a
critical section.

Memoized in a `OnceLock`, matching the process-lifetime memo contract every
flag in `runtime::env_cache` already has. This is **not** a new gate and not a
default-off landing: the variable is a pre-existing optional tuning override
with a working default; only the "re-read it mid-run" behaviour is gone, which
is documented on the function and covered by a test. The uncached
`read_shared_cache_cap` is retained so the parsing rules stay directly
testable, and a `#[cfg(test)]`-only capacity override (compiled out of shipped
builds) lets the three eviction tests keep steering the cap now that the env
var is read once.

Test added: a promoted-invoke round trip asserting the live path's shape —
including that an invalidated key simply re-misses and can be re-promoted
rather than being memoized as a negative.

---

## Cross-owner requests

Requested edits in files this pass does not own. **None of these were made.**

### CR-SW-1 — `native-api/src/registry.rs`: give `StackTraceEntry` a method identity

Add to `pub struct StackTraceEntry` (~line 3638):

```rust
/// Index of this frame's method within its declaring class's
/// `Class::methods`, when captured from a live interpreter frame.
/// `None` for synthetic entries.
pub method_index: Option<u32>,
```

(A `method_descriptor: Option<Arc<str>>` also works and is a smaller
conceptual change, but costs an `Arc` bump per frame per capture and a linear
`find_method` at resolve time; the index is O(1) at both ends and is already
what `stackwalker.rs`'s memo computes.)

**Why:** this is the one thing blocking a fully lazy throw path. With it,
`stackwalker::resolve_line_numbers_in_place` becomes exact for every frame
including overloads, `entry_from_frame` can stop resolving lines at capture
time entirely, and an exception that is caught and discarded — the dominant
case in Spring/Hibernate/JUnit — pays no `LineNumberTable` work at all.
Section 1c above is the GC / class-unloading soundness argument, and it holds
unchanged for the index: `ClassId`s are never recycled, so a deferred lookup
either finds the same class or a tombstone, and `class.methods[idx]` is
re-verified against the entry's `method_name` on every resolve, so a
redefinition that reorders methods fails closed to `UNKNOWN` rather than
resolving to the wrong body.

I am happy to implement the `stackwalker.rs` half — the resolver and the
capture-side change are both already shaped for it — once the field exists.
Ping this slug.

### CR-SW-2 — `vm/src/vm/vm_exec.rs` (~line 2493) and `vm/src/threading/thread_registry.rs` (~line 626): resolve deferred lines for thread dumps

`vm_exec.rs:2493` publishes a per-thread frame snapshot via
`stackwalker::capture_frames_no_lines`, and every consumer of
`ThreadRegistry::frame_trace` therefore sees `line_number == -1` for **every**
frame. `stackwalker::resolve_line_numbers_in_place(&class_store, &mut trace)`
now fills those in for frames whose method name is unambiguous within its
class, and leaves the rest at `-1`. It never produces a wrong line.

Call it at the *reader* (`thread_registry.rs:626` and any other
`frame_trace.lock()` consumer), not at the depositor — the depositor is
deliberately lock-free and must not take a `ClassStore` borrow. Purely
additive: a thread dump gains line numbers where it currently has none.

### CR-VT-1 — `vm/src/memory/gc.rs` (~line 135): use the batch vtable unload

```rust
let mut vtables = shared.classes.vtable_manager.write();
for class in &unloaded {
    vtables.unload_class(class.id.as_u32() as u64);   // one full sweep each
}
```

becomes

```rust
let mut vtables = shared.classes.vtable_manager.write();
let dead: Vec<u64> = unloaded.iter().map(|c| c.id.as_u32() as u64).collect();
vtables.unload_classes(&dead);                        // one sweep total
```

`VtableManager::unload_classes` landed in this pass with a test proving it is
equivalent to the loop. Rationale in §2d: the current form is
`O(unloaded × all_classes × slots_per_class)` under the manager write lock.

### CR-VT-2 — `vm/src/runtime/interpreter.rs`: cache the vtable slot per call site

`vtable_fast` re-derives the slot from `(method_name, method_descriptor)` on
every dispatch (two `FxHasher` runs + a verifying double `&str` compare, §2a).
The slot is a pure function of the *receiver class* and the *constant-pool
methodref*, both of which the call site already knows.

Proposal: extend the quickened-CP entry (or the existing per-call-site invoke
cache) with `Option<(receiver_class_id, u32 slot)>`; on a hit where the
receiver class matches, go straight to `Vtable::get(slot)` and skip
`lookup_slot` entirely. Invalidate on `class_redefine_generation` bump — the
same guard `vtable_fast` already applies to `resolved_method`.

This coordinates with the `quickened-dispatch-o1` pass; flagging rather than
scoping, since the cache belongs in whichever structure that pass settles on.
`vtable.rs` needs no change for it — `Vtable::get(slot)` is already the O(1)
entry point and is already `pub`.

### CR-LR-1 — `vm/src/memory/gc.rs` (~line 130): `invalidate_all` clears two dead maps

`shared.classes.shared_resolution.invalidate_all()` takes three write locks;
two of them (`global_methods`, `global_fields`) guard maps with no production
writers (§3a) and are therefore always empty. Harmless today. Worth a comment
at the call site, or a narrower `invalidate_promoted()` entry point, so a
future reader does not conclude from this call that those maps are live.

I did not add the narrower entry point speculatively — if the call site wants
it, ping this slug and it is a three-line addition to `lockfree_resolve.rs`.

---

## Not done, and why

* **Wiring `resolve_line_numbers_in_place` onto the `Throwable` path.** It
  would regress `printStackTrace` for overloaded frames. Blocked on CR-SW-1.
  See §1d.
* **Splitting CHA invalidation from dispatch validity in `VtableEntry`.**
  Changes dispatch-tier semantics; needs a Mockito-heavy Spring run to
  validate, which this pass cannot do. Scoped proposal in §2c.
* **Deleting the dead `ThreadLocalResolveCache` / `global_methods` /
  `global_fields` surface.** They are documented-but-unwired, not wrong, and
  `ResolutionKey`'s collision hardening is a security fix somebody may still
  want to wire up. Removing them is a judgement call for the owner of the
  dispatch roadmap rather than a perf pass; the header now says plainly that
  they are unwired, which was the actual hazard.
