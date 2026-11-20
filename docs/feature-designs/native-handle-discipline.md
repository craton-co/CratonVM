# Native root handles

**Status:** Shipped (default on) as an API and a convention — **not
mechanically enforced.**

## What it does today

The scoped handle API exists and is widely adopted: roughly 405
`HandleScope::new` / `.root(` sites across the tree. The storage-generic core
is `types/src/handle.rs` (`HandleStorage`, `RootedHandle` — an opaque `u32`
slot, deliberately not a pointer — and a LIFO `HandleScope`). The
native-facing wrapper is in `native-api/src/registry.rs` (`NativeHandle`,
`NativeHandleScope`, whose `Drop` closes the scope on normal return, early
return and unwind alike).

The rule is: **new native code must use the scoped handle API when an object
survives an allocating or re-entrant operation.**

## What is not built yet

**Nothing enforces the rule.** There is no clippy lint, no guard test and no CI
grep that fails when a native holds a raw `ObjectRef` across an allocating
call. The tests under "Enforcement and validation" below exercise the handle
type itself, not its adoption. Validation against a moving collector is
likewise still owed.

## Problem

`ObjectRef` is a raw heap pointer. A moving collection can relocate its
object while Rust native code is inside `invoke`, allocation, a Java
callback, or another re-entrant operation. Reusing the pre-call pointer is a
stale-reference bug even if another copy of the same pointer was pinned.

## API

The storage-generic core is in `cratonvm_types::handle`:

```rust
pub trait HandleStorage {
    fn root(&mut self, object: ObjectRef) -> u32;
    fn unroot(&mut self, slot: u32);
    fn get(&self, slot: u32) -> ObjectRef;
}

pub struct RootedHandle { /* opaque slot */ }
pub struct HandleScope<'a, S: HandleStorage + ?Sized> { /* RAII */ }
```

Native implementations use the trait-object-safe wrapper:

```rust
let mut scope = NativeHandleScope::new(context);
let receiver = scope.root(receiver);
let value = scope.invoke(/* may allocate and collect */)?;
let receiver = scope.get(&receiver);
scope.set_field(receiver, 0, value)?;
```

`NativeHandle` is deliberately neither `Copy` nor an `ObjectRef`.
`NativeHandleScope` dereferences to `NativeContext`, so existing context
operations remain available while the guard owns the mutable borrow. Its
`Drop` closes the exact nested scope on normal return, `?`, early return, and
unwind.

The older `pin_native_root`/`read_native_pin`/`unpin_native_roots` triad is a
compatibility API for code awaiting mechanical conversion. It is not the
pattern for new code because a raw pre-call `ObjectRef` remains available
and can still be read accidentally.

## VM storage and GC contract

Each `JvmThread` owns:

- `handle_slots: Vec<Option<ObjectRef>>`;
- `handle_scope_bases: Vec<usize>`.

Opening a scope records the current slot length. Rooting appends a slot.
Closing truncates to the recorded base, so nested scopes release only their
own handles.

Every live slot participates in current-thread root enumeration and in both
cross-thread deposited snapshots (cooperative safepoint and native-blocked).
After a moving collection, `update_all_roots`, safepoint resume, blocked wake,
or the leaked-blocked-region safepoint fallback rewrites each owning slot
through the collector pointer map before that mutator resumes. Consequently
`scope.get(handle)` always reads the current address; the handle never caches
a heap pointer.

Root publication follows the same rule as native pins: code that establishes
a long-lived batch before peer-triggered collection refreshes its deposited
root snapshot. Cooperative JIT safepoints now perform that publication for
compiled execution.

## Enforcement and validation

The type-level core tests slot remapping and nested LIFO cleanup. The native
wrapper tests normal, nested, and unwinding exits. Moving-GC validation must
root an object, force relocation during a re-entrant call, and assert that a
subsequent handle read yields the remapped address.
