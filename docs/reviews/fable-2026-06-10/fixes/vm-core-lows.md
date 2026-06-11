# Fix note — vm-core-lows

**Agent:** vm-core-lows
**Report:** `docs/reviews/fable-2026-06-10/vm-core.md` (findings B3, B4, B5, V2)
**Owned files edited:** `vm/src/vm/vm_exec.rs`, `vm/src/vm/vm_init.rs`
**Build configs intended to stay green:** default (real-jdk), app-stubs, synthetic-jdk. No feature-gated branches added; all edits are unconditional and only use symbols already in scope.

---

## B3 — RAII guard for the per-class loading flag (`vm_init.rs`)

`load_class_*` path (now ~3004-3040, drop site ~3107). Previously `*loading` was set
`true` (line 3005), `drop(loading)`, then `load_class` ran, then the flag was reset
`false` + `notify_all()` only on the *normal* return. If `load_class` (or any of the
post-load JIT/JVMTI work) unwound via panic, the flag stayed `true` forever: every
later loader of that class double-checks (not loaded), enters `while *loading`, and
spins on the 30s `wait_timeout` that no notifier ever satisfies — one load panic
turns into a cascade of stuck/aborting loaders.

**Fix:** introduced a function-local `LoadingFlagGuard { lock: Arc<(Mutex<bool>, Condvar)> }`
whose `Drop` re-acquires the per-class lock, sets `*loading = false`, and
`notify_all()`s. It holds a cloned `Arc` of `class_lock`, so it works on both the
success path (explicit `drop(loading_guard)` replacing the old manual reset block)
and the unwind path. The `Drop` tolerates a poisoned lock (`poisoned.into_inner()`)
so the notify still fires defensively. The success-path `remove(name)` cleanup runs
*after* the guard drop, preserving the original notify-then-cleanup ordering.

Note: because the guard is dropped *before* the fallible `load_class` work in the
success case and the std `Mutex` was already unlocked (`drop(loading)`) before
`load_class`, the per-class mutex itself is not actually poisoned by a load panic;
the real hazard the report describes is the never-reset flag + never-notified
waiters, which this guard contains.

## B4 — no pointer truncation in `coerce_value_for_return` (`vm_exec.rs` ~111-136)

The `b'I'|b'B'|b'C'|b'S'|b'Z'` arm did `Value::Int(p.as_ptr() as usize as i32)` and
the `b'J'` arm `Value::Long(... as i64)` for a type-confused `Object(Some(_))` return
— silently truncating / leaking a 64-bit heap pointer into an integer slot.

**Fix:** both `Object(Some(_))` arms now `debug_assert!(false, …)` (loud in debug)
and yield the zero default (`Value::Int(0)` / `Value::Long(0)`) in release, matching
the philosophy of the `b'F'`/`b'D'` arms (which never fabricate a corrupted value
from a mismatched variant). `Object(None)`, the legitimate `Long->I` narrowing, and
the `L`/`[` jobject path are unchanged.

## B5 — deleted the dead, hazardous `value_as_object_ref` (`vm_exec.rs` ~304-311)

The *unvalidated* `value_as_object_ref` reinterpreted any aligned `Value::Long` bits
as an `ObjectRef` via `ObjectRef::from_raw` with no heap-membership check — the exact
GC-mark SEGV (0xC0000005) pattern the surrounding comments warn about. A repo-wide
grep confirmed **zero callers** anywhere in the tree (all sites use
`value_as_validated_object_ref`).

**Fix:** removed the function entirely, leaving a `// NOTE (B5)` breadcrumb pointing
future callers at the validated variant so the hazard can't be silently
reintroduced. The validated variant's doc-comment no longer intra-doc-links the
deleted symbol (avoids a rustdoc broken-link warning).

## V2 — descriptor-arity sanity check before unsafe JNI dispatch (`vm_exec.rs`)

Two call sites (RegisterNatives fast path ~9928, dlsym auto-resolve path ~9990)
entered `unsafe { dispatch_jni_native(fn_ptr, env, receiver, call_args, descriptor) }`
trusting the (untrusted-classfile-derived) `descriptor`. `dispatch_jni_native`
`zip`s `call_args.iter()` with `parse_param_types_cached(descriptor)`, so a descriptor
whose parameter count disagrees with `call_args.len()` silently builds a malformed C
call frame (missing/extra register args) → UB.

**Fix:** before each unsafe dispatch, compute
`crate::runtime::proxy::count_descriptor_params(descriptor)` (JVM-spec param count:
long/double = 1, matching the one-`Value`-per-param shape of `call_args`) and compare
to `call_args.len()`. On mismatch, clear the JNI TLS context/thread, emit a
`tracing::warn!`, and `return Err(UnsatisfiedLinkError{..})` (per the report's
recommendation and JVMS §5.3.5) instead of entering the unsafe call. Valid calls are
unaffected.

---

## Test added

`#[cfg(test)] mod tests` in `vm_exec.rs` gained four focused unit tests (pure
functions, no `SharedVm`, so they don't trip the new B4 `debug_assert`):
- `coerce_null_object_to_int_is_zero`, `coerce_null_object_to_long_is_zero` — B4 null
  coercion stays zero.
- `coerce_long_to_int_truncates_value_not_pointer`, `coerce_int_to_long_widens` —
  legitimate primitive narrowing/widening arms unchanged.
- `jni_arity_helper_matches_jvm_spec_param_count` — pins the V2 arity helper's
  long/double = 1 semantics (`(JD)V` → 2, `(Ljava/lang/String;[IJ)Z` → 3).

## Compile confidence

High. All edits use symbols already imported/in-scope in each file
(`MethodCallFailed`, `VmError`, `RuntimeError`, `Arc`, `Value`,
`crate::runtime::proxy::count_descriptor_params`, `crate::native::jni::*`). No new
imports, no feature-gated code, no changes to public signatures used elsewhere. The
deleted `value_as_object_ref` had no callers (verified by repo-wide grep). Did not
run cargo (per agent rules).
