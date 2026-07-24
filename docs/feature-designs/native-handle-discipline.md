# Native handle discipline — ending the stale-`ObjectRef` bug class

Status: **PARTIAL / PILOT.** The rooted-handle type, the `NativeContext`
trait API, the VM-side per-thread slot storage, and the GC root-scan splice
are implemented. The pilot migration (5 methods in
`native-builtins/src/lang_string.rs`) demonstrates the pattern. **The
post-move remap step is NOT yet wired into `vm/src/memory/gc.rs`** — see
[§6](#6-known-gap-post-move-remap-not-yet-wired) before relying on this for
correctness under a moving collection. That file was out of scope for the
change that introduced this design; it needs a small, mechanical follow-up
patch described below.

---

## 1. The problem

`ObjectRef` (`types/src/value.rs`) is a raw, niche-optimized pointer into the
GC heap. CratonVM's moving collector relocates objects; when it does, every
`ObjectRef` value pointing at the moved object goes stale unless something
rewrites it. Native method implementations routinely need to hold a
reference across a re-entrant call — `invoke`, `new_object_initialized`,
`new_array`, or any other operation that can run Java code — and Java code
can always allocate, which can always trigger a GC.

The natural-looking code:

```rust
let this = /* ObjectRef from an arg */;
let buf = ctx.new_array(ArrayElementType::Char, cap); // may GC-move `this`
ctx.set_field(this, 0, Value::Object(Some(buf)));     // `this` may be stale here
```

silently corrupts state instead of crashing: a stale `ObjectRef` doesn't
fault, it resolves to whatever now occupies the vacated from-space slot —
usually a reused, unrelated `java.lang.Object` — so the write above lands on
the wrong object, or a field read comes back with plausible-looking garbage.
`docs/known-issues/README.md` documents this as the project's **#1 recurring
bug family**: 37+ independently discovered, independently fixed occurrences,
each one a fresh investigation because nothing in the type system flags the
mistake.

### 1.1 The existing partial fix

`NativeContext::pin_native_root` / `read_native_pin` / `unpin_native_roots`
(`native-api/src/registry.rs`, backed on the VM side by
`JvmThread::native_pin_roots`, `vm/src/vm/vm_exec.rs:3611`,
`vm/src/memory/roots.rs:153`, and the remap loop in
`vm/src/memory/gc.rs:398`) works, and is the mechanism most existing fixes
use:

```rust
let this_pin = ctx.pin_native_root(this);
let buf = ctx.new_array(ArrayElementType::Char, cap);
let this = ctx.read_native_pin(this_pin, this); // re-read through the pin
ctx.unpin_native_roots(this_pin);
```

It closes the staleness gap, but not the *discipline* gap: `this` before and
after pinning is the exact same `ObjectRef` type. Nothing stops a future edit
(or a reviewer skimming a long function) from reading the pre-call `this`
instead of the post-call one — the code still compiles, still runs, and is
still wrong. Nearly every one of the 37+ fixes in `docs/known-issues/` is
exactly this shape: a *correct* pin/read pair existed somewhere upstream, and
a *later* edit introduced a fresh raw read that bypassed it.

## 2. The fix: a handle is not an `ObjectRef`

A `RootedHandle` never stores a raw pointer. It stores an opaque slot id and
reads *through* that slot, into GC-owned storage, on every access. If a GC
runs and updates the slot (the storage owner's job — see §5), the very next
read reflects the new address automatically. There is no local pointer copy
to go stale, because there is no local pointer copy at all — the type itself
makes the old mistake impossible to express, rather than merely detectable.

## 3. Two layers

### 3.1 `cratonvm_types::handle` — storage-generic core

```rust
pub trait HandleStorage {
    fn root(&mut self, r: ObjectRef) -> u32;
    fn unroot(&mut self, slot: u32);
    fn get(&self, slot: u32) -> ObjectRef;
}

pub struct RootedHandle { /* slot: u32 */ }
pub struct HandleScope<'s, S: HandleStorage + ?Sized> { /* ... */ }
```

`types` sits below `vm` in the dependency graph and cannot see `JvmThread` or
any GC machinery — the same reason `loader_pin`/`mirror_pin` live here
instead of in `vm`. `HandleStorage` is the narrow interface that lets
`RootedHandle`/`HandleScope` be defined, and unit-tested, without a circular
`types -> vm` dependency. `HandleScope` is an RAII guard: every handle
rooted through it is released, LIFO, on drop (or explicit `.pop()`) — the
generic analogue of `native_pin_roots`' base-index-and-truncate discipline,
expressed as a guard. See `types/src/handle.rs` for the full implementation
and its unit tests (in-memory `HandleStorage`, no VM needed).

This layer is *not* what native method implementations call directly (see
§3.2) — it exists so the pattern is independently defined and testable, and
as the reference shape the VM-facing layer's `handle_root`/`handle_get`
mirror.

### 3.2 `NativeContext` — the trait-object-safe layer

`native-api/src/registry.rs` extends the `NativeContext` trait every native
method receives as `ctx: &mut dyn NativeContext`:

```rust
fn handle_scope_push(&mut self);
fn handle_scope_pop(&mut self);
fn handle_root(&mut self, r: ObjectRef) -> u32;
fn handle_get(&self, slot: u32) -> Option<ObjectRef>;
```

Raw `u32` slots, not `RootedHandle`, because `&mut dyn NativeContext` can't
hand back a `RootedHandle<'_, Self>` without naming `Self` — a trait object
erases it. Default impls mirror the existing no-op/pass-through convention
`pin_native_root` & co. use for mock/test contexts with no moving GC:
`handle_scope_push`/`pop` are no-ops, `handle_root` delegates to the
already-default `pin_native_root` (cast to `u32`) as the closest existing
stand-in, and `handle_get`'s default returns `None` (there is nothing
genuine for the delegated `pin_native_root` default to hand back — its own
default doesn't retain anything either). This keeps every other
`NativeContext` implementor (native-collections/native-io/native-builtins
test mocks, `native-api/tests`) compiling unchanged; only the VM's real
`NativeContextImpl` needs to override them.

**Discipline:** any native code that holds a reference across a call that
can allocate — `invoke`, `new_object_initialized`, `new_array`, anything
that can run Java — MUST hold it as a handle, not a bare `ObjectRef` local,
for the entire span. Usual shape:

```rust
ctx.handle_scope_push();
let this_h = ctx.handle_root(this);
let arr = ctx.new_array(ArrayElementType::Char, len); // may GC-move `this`
let this = ctx.handle_get(this_h).unwrap_or(this);    // current address
ctx.handle_scope_pop();
```

Scopes nest: an inner push/pop pair fully contained inside an outer one only
releases its own handles, mirroring `pin_native_root`'s base-index nesting.

**No automatic cleanup on early return.** Unlike `types::HandleScope`
(§3.1), the raw `NativeContext` methods are plain trait calls — there is no
`Drop` firing `handle_scope_pop` for you. An early `return`/`?` between
`handle_scope_push` and `handle_scope_pop` must call `handle_scope_pop`
itself first (see `native_sb_init_capacity`'s OOM path in §4 for a worked
example). This is the same discipline `unpin_native_roots` already required
at every existing early-return pin site; it is not a regression. A
`Drop`-based convenience wrapper around `&mut dyn NativeContext` is a
plausible low-risk follow-up once more call sites adopt the raw API (see
§7) — deliberately not built as part of this pilot, to keep the trait
surface and the diff small.

## 4. Worked examples (the pilot migration)

`native-builtins/src/lang_string.rs` converts 5 `String`/`StringBuilder`/
`StringBuffer` constructors off the raw pin triad, as the reference example
for future migrations:

* `native_string_init_abstract_string_builder` — the base case: `this`
  crosses one allocating call (`ctx.new_array`).
* `native_sb_init_default`, `native_sb_init_string` — same shape, showing
  the pattern is a mechanical, uniform replacement regardless of what
  computation happens around the allocating call.
* `native_sb_init_charsequence` — `this` crosses **two** allocating calls
  (`invoke_to_string`, which runs arbitrary Java `toString()` and can
  allocate/GC at will, then `new_array`) while staying valid the whole time
  through a single root/scope pair. A raw pin would need re-reading after
  *each* call individually; a handle just stays current.
* `native_sb_init_capacity` — the early-return case: the OOM path calls
  `ctx.handle_scope_pop()` before `return Err(...)`, matching the note in
  §3.2 about early returns needing an explicit pop.

Every conversion is a drop-in replacement of the same shape:

```rust
// before
let this_pin = ctx.pin_native_root(this);
let buf = ctx.new_array(ArrayElementType::Char, cap);
let this = ctx.read_native_pin(this_pin, this);
ctx.unpin_native_roots(this_pin);

// after
ctx.handle_scope_push();
let this_h = ctx.handle_root(this);
let buf = ctx.new_array(ArrayElementType::Char, cap);
let this = ctx.handle_get(this_h).unwrap_or(this);
ctx.handle_scope_pop();
```

`sb_ensure_capacity` (the shared grow helper other `StringBuilder`/
`StringBuffer` natives funnel through) was deliberately left on the raw pin
API for this pass — it is a much higher-fan-in call site and a good
candidate for a *second* migration pass once the pattern above has had time
to prove out, rather than folding into the initial pilot.

## 5. VM-side storage

### 5.1 `JvmThread` fields (`vm/src/threading/jvm_thread.rs`)

```rust
/// One entry per live handle slot; `None` = released.
pub handle_slots: Vec<Option<ObjectRef>>,
/// Stack of `handle_slots` lengths recorded by each `handle_scope_push`.
pub handle_scope_bases: Vec<usize>,
```

Append-only, base-truncate — exactly `native_pin_roots`' own shape, placed
immediately after it. The difference from `pin_native_root`'s convention is
*where* the base index lives: `handle_scope_push`/`pop` record and consume
the base on the thread itself, so nested native calls each get a plain
push/pop pair instead of every caller threading a base index through (as
`pin_native_root`'s callers must via `unpin_native_roots(base)`).

### 5.2 `NativeContextImpl` (`vm/src/vm/vm_exec.rs`, "handle scope support"
block, appended right after the existing `pin_native_root`/`read_native_pin`/
`unpin_native_roots` cluster)

* `handle_scope_push` — push `handle_slots.len()` onto `handle_scope_bases`.
* `handle_scope_pop` — pop a base and truncate `handle_slots` back to it
  (no-op / defensive no-panic if the stack is already shorter, mirroring
  `unpin_native_roots`' own `base < len` guard).
* `handle_root` — push `Some(r)`, return the new index as `u32`. Applies the
  same `blocked_access_debug` census-visibility guard `pin_native_root`
  does: rooting while this thread's blocked-region flag is raised is
  invisible to both the STW root scan and the blocked-thread fold.
* `handle_get` — `handle_slots.get(slot as usize).copied().flatten()`.

### 5.3 Root scan (`vm/src/memory/roots.rs`, spliced immediately after the
existing `native_pin_roots` scan at the same site, ~line 153)

```rust
for slot in &thread.handle_slots {
    if let Some(obj_ref) = slot {
        roots.push(*obj_ref);
    }
}
```

A live (`Some`) slot is a GC root exactly like a `native_pin_roots` entry. A
`None` hole (a released slot the next `handle_scope_pop` truncate hasn't
reached yet, or already truncated away — the `Vec` only ever holds `None`
holes transiently within a still-open outer scope) contributes nothing.

## 6. Known gap: post-move remap not yet wired

Root scanning (§5.3) is only half of what a **moving** collector needs. The
other half — rewriting a live root's `ObjectRef` in place once its object
has actually relocated — is a *separate* pass, `update_all_roots` in
`vm/src/memory/gc.rs`, which `native_pin_roots` also participates in
(`vm/src/memory/gc.rs:398`, right after the JIT/shadow-stack remap steps):

```rust
for obj_ref in &mut thread.native_pin_roots {
    let old_addr = obj_ref.as_ptr() as usize;
    if let Some(&new_addr) = pointer_map.get(&old_addr) {
        debug_assert!(new_addr != 0, "GC pointer map contains null address");
        *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
    }
}
```

**`handle_slots` has no equivalent block yet.** `vm/src/memory/gc.rs` was
out of scope for the change that introduced this design (a different agent
owns that file), so this is a required, mechanical follow-up, not an
oversight to design around:

```rust
for slot in &mut thread.handle_slots {
    if let Some(obj_ref) = slot {
        let old_addr = obj_ref.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
    }
}
```

...inserted right next to the `native_pin_roots` block above, inside
`update_all_roots`.

**Practical impact until that lands:** a handle is fully correct under a
**non-moving** collection (the common case when nothing relocates — root
scanning alone is sufficient to keep the object alive) and strictly no
*worse* than a bare `ObjectRef` under a **moving** one. It is not yet the
"can never go stale" guarantee this design is meant to deliver end-to-end.
Do not treat the handle API as a complete fix for moving-GC staleness until
the `gc.rs` block above lands and is verified (e.g. with
`CRATONVM_DBG_STALE_OBJREF=1`, the existing stale-`ObjectRef` detector
described in `gc/src/stale_objref_debug.rs`, extended to also cover
`handle_slots` reads).

## 7. Migration guide

1. Find a native method that copies an `ObjectRef` into a local and later
   uses it after a call to `ctx.invoke`, `ctx.new_object_initialized(_with_class_id)`,
   `ctx.new_array`/`ctx.try_new_array`, or any helper that transitively calls
   one of those (e.g. `invoke_to_string`, `sb_ensure_capacity`).
2. Wrap the span from before the first such call to after the last read of
   the reference with `ctx.handle_scope_push()` / `ctx.handle_scope_pop()`.
3. Replace the raw local with `let x_h = ctx.handle_root(x);` before the
   first allocating call.
4. Replace every post-call read of the raw local with
   `ctx.handle_get(x_h).unwrap_or(x)` (the `unwrap_or` fallback covers the
   same "handle out of range" edge `read_native_pin`'s `fallback` parameter
   already covers — should never trigger for a slot this same scope minted,
   but costs nothing to keep).
5. On every early-return path between the push and the pop, call
   `ctx.handle_scope_pop()` first (§3.2) — there is no automatic cleanup.
6. If migrating a *shared* helper (like `sb_ensure_capacity`) called from
   many sites, prefer a dedicated pass once the pattern has proven out at
   its current call sites, rather than folding it into an unrelated change.

## 8. Files touched by this design

* `types/src/handle.rs` (new) — `HandleStorage`, `RootedHandle`,
  `HandleScope`, unit tests.
* `types/src/lib.rs` — `pub mod handle;` + re-exports.
* `native-api/src/registry.rs` — the 4 new `NativeContext` methods.
* `vm/src/threading/jvm_thread.rs` — `handle_slots` / `handle_scope_bases`
  fields + constructor init.
* `vm/src/memory/roots.rs` — root-scan splice.
* `vm/src/vm/vm_exec.rs` — `NativeContextImpl`'s 4 method bodies.
* `native-builtins/src/lang_string.rs` — pilot migration (5 methods).
* **Not yet touched (§6):** `vm/src/memory/gc.rs`'s `update_all_roots` —
  needs the post-move remap block.
