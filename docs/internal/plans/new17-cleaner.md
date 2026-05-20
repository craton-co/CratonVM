# NEW-17 — `Cleaner` / `PhantomReference` / `finalize` modernization

## Goal

Make `java.lang.ref.Cleaner` *actually* run user-supplied cleanup actions when
a referent becomes unreachable, and plumb `DirectByteBuffer.allocateDirect`
through that mechanism so native memory is released on GC.

**Definition of done:** a test that allocates a `DirectByteBuffer`, drops the
strong reference, forces GC, and observes that the native memory counter
returns to zero. Equivalently: the Cleaner runnable registered by
`allocateDirect` executes and releases the underlying native allocation.

## Existing infrastructure (already green)

- `gc::reference::ReferenceProcessor`
  - Tracks `ReferenceType::{Soft,Weak,Phantom,Cleaner,Finalizer}` entries.
  - `process_references(is_marked, free_mb, now_ms)` returns
    `ReferenceProcessingResult { to_enqueue, to_finalize, cleaner_actions, stats }`.
  - Cleaner refs whose referent is unreachable get their `reference_obj`
    address pushed into `cleaner_actions`.
- `gc::reference::CleanerThread`
  - `submit_action(addr)` / `drain_actions() -> Vec<usize>` / `pending_count`.
- `vm::runtime::interpreter::process_references_after_gc`
  - Already submits each `cleaner_actions` entry to `shared.cleaner_thread`.
- `NativeContext::discover_reference(ref_type:u8, ref_obj, referent, queue)`
  - Wired through `vm_exec::discover_reference` into the ref processor.
- `NativeContext::invoke_virtual(receiver, name, desc, args)` — can call
  `Runnable.run()V` on any receiver.
- `allocate_native_memory` / `free_native_memory` — on the Panama FFI path,
  backed by real Rust allocations keyed by `alloc_id: i64`.
- `PhantomReference`/`WeakReference`/`SoftReference` `<init>` already set
  referent + queue fields and call `discover_reference`.

## What is missing

1. **`Cleaner.create()` / `Cleaner.register(obj, action)` are stubs.** They
   allocate 1-field synthetics and never register with the ref processor,
   so no cleaner action is ever discovered or submitted by GC.
2. **Cleaner actions are submitted to `CleanerThread` but never invoked.**
   The `pending_actions` VecDeque grows forever. There is no code path that
   drains it and calls `Runnable.run()` on the associated action.
3. **`ByteBuffer.allocateDirect` is a pure synthetic** — no native memory,
   no cleaner, nothing to release.
4. **`ReferenceType::Cleaner` is not reachable from the NativeContext API.**
   `discover_reference(ref_type: u8, ...)` currently only accepts `0=Weak`,
   `1=Soft`, `2=Phantom`. We need a `3=Cleaner` path.

## Checkpoints

### CP1 — Reference discovery: accept `ReferenceType::Cleaner`

**Files:** `vm/src/vm/vm_exec.rs` (the `discover_reference` impl).

Add match arm `3 => ReferenceType::Cleaner` so native code can register a
phantom-cleaner entry through the existing native API.

**Verify:** `cargo check -p cratonvm-vm`.

### CP2 — Real `Cleaner.create()` and `Cleaner.register()`

**Files:** `native-builtins/src/phases_late.rs::register_p69_cleaner`.

Data model (synthetic layouts):

- `java/lang/ref/Cleaner` — 2 fields
  - field 0 : `Object[]` backing list of live `Cleanable`s (prevents GC of
    the phantom-cleaner reference object itself; the referent is tracked
    via `ReferenceProcessor`).
  - field 1 : `Int` — number of live cleanables (size of field 0 in use).
- `java/lang/ref/Cleaner$Cleanable` — 3 fields
  - field 0 : `Object` — the `Runnable` action.
  - field 1 : `Int`    — `cleaned` flag (0 = pending, 1 = already run).
  - field 2 : `Int`    — index of this cleanable inside its owning
    cleaner's backing array (for O(1) unlinking on explicit `clean()`).

`Cleaner.create()`:
- Allocate a new `Cleaner` synthetic.
- Allocate an initial `Object[16]` backing list, set field 0 / field 1 = 0.

`Cleaner.register(obj, action)`:
- Allocate a `Cleaner$Cleanable` synthetic with field 0 = action,
  field 1 = 0, field 2 = next free slot.
- Append the cleanable into the cleaner's backing array, growing it
  by doubling if full.
- Call `ctx.discover_reference(3 /*Cleaner*/, cleanable, obj, None)` so
  the ref processor will treat the *cleanable* as the phantom reference
  object. When `obj` dies, the ref processor pushes `cleanable`'s address
  into `cleaner_actions`.
- Return the cleanable.

`Cleaner$Cleanable.clean()` (explicit user call):
- If already cleaned (field 1 == 1), no-op.
- Set field 1 = 1 and invoke `Runnable.run()V` on field 0 via
  `ctx.invoke_virtual(runnable, "run", "()V", &[])`.
- This is the synchronous path the user can trigger without waiting for GC.

**Verify:** `cargo check`, `cargo test -p cratonvm-native-builtins`.

### CP3 — Drain `CleanerThread` actions and invoke `Runnable.run()`

**Files:**
- `vm/src/runtime/interpreter.rs` — add `run_cleaner_actions(shared, thread)`.
- Call it from `force_gc_from_native` right after `run_finalizers`, and from
  the top of `safepoint_check` (or more simply, from the tail of
  `process_references_after_gc` once GC releases the lock).

Implementation:

```rust
fn run_cleaner_actions(shared: &SharedVm, thread: &mut JvmThread) {
    let addrs = shared.cleaner_thread.drain_actions();
    for addr in addrs {
        let cleanable = unsafe { ObjectRef::from_raw(addr as *mut u8) };
        // Skip if already cleaned (race with explicit Cleanable.clean()).
        let already = matches!(
            shared.heap.get_field(cleanable, 1),
            Value::Int(1),
        );
        if already { continue; }
        shared.heap.set_field(cleanable, 1, Value::Int(1));
        let action = match shared.heap.get_field(cleanable, 0) {
            Value::Object(Some(o)) => o,
            _ => continue,
        };
        // Invoke run()V — errors are logged and swallowed per the
        // Cleaner contract (exceptions thrown from a cleanup action
        // are caught and logged to System.err by the JDK).
        let class_id = shared.heap.class_id_of(action);
        let class_name = shared.class_manager.read()
            .get_class(class_id).map(|c| c.name.clone());
        if let Some(name) = class_name {
            let _ = crate::vm::invoke_shared(
                shared, thread, &name, "run", "()V",
                &[Value::Object(Some(action))],
            );
        }
    }
}
```

Hook it into `force_gc_from_native` after the existing `run_finalizers`
call. This guarantees that every explicit `System.gc()` path gets a
chance to drain pending cleaners.

**Verify:** Unit test that registers a Cleaner with an action that
increments a static counter, drops the strong reference to the
referent, forces GC, and verifies the counter reached 1.

### CP4 — Real native memory for `DirectByteBuffer` + cleaner wiring

**Files:** `native-builtins/src/servlet.rs`
(`register_p53_byte_buffer::allocateDirect`).

Current synthetic `java/nio/ByteBuffer` field layout (BB_ARRAY, BB_POS,
BB_LIMIT, BB_CAP, BB_MARK, BB_ORDER — 6 fields). We'll add two more
fields for direct buffers: `BB_NATIVE_ADDR = 6`, `BB_NATIVE_ALLOC_ID = 7`.

New behaviour of `allocateDirect(capacity)`:
1. `let (alloc_id, _ptr) = ctx.allocate_native_memory(cap, 8)?;`
2. Allocate an 8-field synthetic buffer.
3. Populate fields 0..5 as before (with an empty byte array as `BB_ARRAY`
   so array-reading methods keep working), set field 6 = alloc_id as Long,
   field 7 = Int(1) (freed flag default 0; set 0 actually).
4. Allocate a "DirectByteBufferCleaner" synthetic whose field 0 = alloc_id.
5. Create a `java/lang/ref/Cleaner$Cleanable` and wire it via
   `discover_reference(Cleaner, cleanable, buffer, None)` — the action
   is a `Runnable` synthetic of class `java/nio/Bits$Deallocator` whose
   `run()V` native implementation invokes `ctx.free_native_memory(alloc_id)`.

Since we need to dispatch `run()` on the Deallocator, we must register
`java/nio/Bits$Deallocator.run()V` as a native method that reads its own
field 0 (the alloc_id) and calls `ctx.free_native_memory(alloc_id)`. This
keeps the cleanup path inside the native layer (no bytecode runnable).

**Verify:** `cargo test` — direct buffer allocation + cleaner test.

### CP5 — Tests

Two Rust unit tests in `native-builtins` (no bytecode required):

1. `new17_cleaner_runs_on_gc` — registers a `Cleaner` with a known
   `Runnable` synthetic whose `run()` flips a static Rust flag;
   force the referent to be unreachable; trigger GC; assert the flag
   flipped.
2. `new17_direct_byte_buffer_releases_native_memory` — allocate a
   1 MiB DirectByteBuffer; record the native alloc count; drop the
   buffer; GC; assert the alloc count decreased.

### CP6 — Roadmap update

Mark NEW-17 as ✅ DELIVERED in `docs/roadmap.md`, noting:
- Cleaner → ref_processor → CleanerThread → invoke_shared plumbing.
- DirectByteBuffer native memory lifecycle.
- Unit tests in `phases_late.rs` under `new17_cleaner_tests`.

## Files touched

| File | Change |
|------|--------|
| `vm/src/vm/vm_exec.rs` | `discover_reference` accepts `3 = Cleaner` |
| `native-builtins/src/phases_late.rs` | Real `Cleaner.create/register/clean` |
| `native-builtins/src/servlet.rs` | `allocateDirect` takes real native memory + registers cleaner |
| `vm/src/runtime/interpreter.rs` | `run_cleaner_actions` draining path, invoked from `force_gc_from_native` |
| `docs/roadmap.md` | NEW-17 mark complete |

## Invariants preserved

- `process_references_after_gc` already relocates ref processor addresses
  after pointer-map relocation, so cleaner `reference_obj` addresses
  remain correct across GC (NEW-12 compaction integration).
- Cleanables are pinned alive by the `Cleaner`'s field-0 array, so the
  phantom-cleaner entry in the ref processor does not get removed by
  `remove_collected` until the owning `Cleaner` itself dies.
- Explicit `Cleaner$Cleanable.clean()` and GC-triggered cleanup both
  guard on the `cleaned` flag (field 1), making double-run impossible.
