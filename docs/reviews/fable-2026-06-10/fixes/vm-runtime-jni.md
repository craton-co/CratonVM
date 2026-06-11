# Fix note: vm-runtime-jni

Owner: agent `vm-runtime-jni`
File touched: `vm/src/native/jni.rs` (only)
Findings addressed: V1 (low), V2 (low) from `docs/reviews/fable-2026-06-10/vm-runtime.md`.

## V1 — `GetStringUTFRegion` 1-byte out-of-bounds NUL write

`jni_get_string_utf_region` copied the region bytes into `buf` and then wrote
`*buf.add(bytes.len()) = 0;`. The JNI spec for `GetStringUTFRegion` does **not**
null-terminate the destination (only `GetStringUTFChars` does), so a conforming
caller that sizes `buf` to exactly the region length got a 1-byte OOB write.
HotSpot writes no terminator here.

Fix: dropped the trailing-NUL write entirely. Now only `bytes.len()` bytes are
copied, matching the spec and HotSpot.

## V2 — region helpers trusted caller `start`/`len` (no explicit length check / no AIOOBE)

`Get/Set<Type>ArrayRegion` (the `get_array_region!` / `set_array_region!`
macros) and the two string-region accessors only checked `start >= 0 && len >= 0`
and then relied on the per-element heap accessor's *internal* bounds check, which
silently swallowed an out-of-range request (`.ok()?` / `let _ =`). Memory-safe,
but it violates the JNI contract, which requires throwing
`ArrayIndexOutOfBoundsException` (string regions: `StringIndexOutOfBounds`,
surfaced here as AIOOBE) for a window outside the array/string.

(Note: `GetPrimitiveArrayCritical` has **no** `start`/`len` parameters — it hands
out the whole array, so it has no region to bounds-check; the report's V2 mention
of it is moot. The actionable surface is the region/string-region helpers.)

Fix: added two helpers just above the region macros:

- `region_bounds_ok(start: JSize, len: JSize, length: usize) -> bool` —
  validates `start >= 0 && len >= 0 && start + len <= length` using
  `checked_add` (so `start + len` cannot wrap into a spuriously-valid range).
  Returns `true` for in-bounds / empty windows; on `false` it has already raised
  a pending AIOOBE.
- `raise_jni_aioobe(index, length)` — when a `JvmThread` context is available
  (via `with_jni_context`) it materialises a real
  `java/lang/ArrayIndexOutOfBoundsException` via
  `runtime::exceptions::create_exception_object` and stores its handle in
  `JNI_PENDING_EXCEPTION` (the same slot `Throw`/`ThrowNew` use), so `vm_exec`
  rethrows it as a catchable Java exception on native return. Without a thread
  context (e.g. a direct unit-test call) it falls back to the `ThrowNew`
  sentinel (`u64::MAX`) so the condition is flagged, never silently dropped.

Applied the check, **before** any read/write, in:
- `get_array_region!` macro (all 8 `Get<Type>ArrayRegion` natives)
- `set_array_region!` macro (all 8 `Set<Type>ArrayRegion` natives)
- `jni_get_string_region`
- `jni_get_string_utf_region`

The in-bounds fast path is unchanged (the bounds check is a couple of integer
comparisons gating the existing loop/copy). On OOB, the helper returns early
(`None`) before any element is touched, so there is no partial copy/store.

### Reentrancy / borrow safety
`region_bounds_ok` is invoked inside the macros' `with_shared_vm` closure, which
holds an *immutable* `borrow()` on the `JNI_SHARED_VM` RefCell.
`raise_jni_aioobe` → `with_jni_context` takes only another *immutable* borrow,
clones the `Arc` out, and drops the borrow before calling
`create_exception_object`. No `borrow_mut()` of `JNI_SHARED_VM` happens on this
path (only set/clear-context do that, which aren't reached during exception
creation), so there is no RefCell double-borrow panic.

## Tests added (`#[cfg(test)] mod tests`)

- `region_bounds_ok_rejects_out_of_range` — accepts in-bounds/empty windows,
  rejects `start+len > length` and `start past end`, and confirms
  `i32::MAX, i32::MAX` does not wrap into a valid range. Drains the pending
  sentinel afterward so it doesn't leak into the shared thread-local.
- `jni_get_array_region_out_of_bounds_no_partial_write` — an OOB
  `Get<Type>ArrayRegion` leaves the destination buffer fully untouched (no
  partial write) and flags a pending exception; an OOB `Set<Type>ArrayRegion`
  does not mutate the array.

## Configs / compatibility

- No new `cfg` gates; helpers are plain `fn`s. The file already has
  `#![allow(dead_code)]`, and both helpers are used by the macros/string
  regions, so no unused warnings. Compiles identically across default,
  app-stubs, and synthetic-jdk (no feature-gated code paths added).
- Existing `jni_array_region_roundtrip` test is unaffected
  (`region_bounds_ok(0, 4, 4)` → `4 <= 4` → in-bounds).
- No `NativeKind`/stub policy interaction — this is a real correctness/contract
  fix in the JNI region path, not a stub.
