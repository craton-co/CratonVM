# Round-8 VM audit

Scope: `vm/` crate. (A) round-7 regression audit, (B) carryovers, (C) new
angles.

---

## CRIT-1 — `jit_{iastore,bastore,aastore}` `process::abort()` rests on a non-existent SIGSEGV→NPE handler
`vm/src/jit/helpers.rs:749, 794, 840`. Comments justify the abort by
the JIT inlining stores and relying on a page-fault NPE through
`emit_bounds_check`. But `emit_bounds_check` (`jit/src/x64.rs:7307`)
only does the bounds check; null RAX page-faults. The sole SIGSEGV/SEH
handler (`crash_handler.rs:287-326`) writes `hs_err_pid.log` and
re-raises `SIG_DFL`. Inline-store-on-null already crashes; the abort
adds nothing and the comment is wrong. **Fix:** drop the abort, set
`JIT_PENDING_NPE` + return (drained at `interpreter.rs:2249/12618`);
file follow-up for a real SEGV→NPE handler.

## CRIT-2 — `JIT_PENDING_NPE` leaks across normal-return JIT path
`vm/src/runtime/interpreter.rs:2224-2253, 12598-12624`. The drain runs
*only when result == i64::MIN*. If a JIT'd method dispatches into a
callee that hits `jit_iaload(null)` setting the flag, but the outer
JIT returns normally, the flag leaks and the next unrelated JIT entry's
deopt mis-attributes the NPE — the bug round-7 CRIT-1 claimed to fix.
**Fix:** drain `take_jit_pending_npe/_aioobe/_exception` unconditionally
at JIT entry (after `set_jit_thread`) AND at the top of the normal
return arm.

## CRIT-3 — `ensure_class_initialized_shared` "AtomicU8 fast path" takes two RwLocks per call
`vm/src/vm/vm_util.rs:79-88`. Code does `class_manager.read()` just to
call `class_init_state_handle`, which itself takes `init_states.read()`
and clones an `Arc<AtomicU8>`
(`classloading/src/class_manager.rs:2969-2979`). Two reader-locks +
atomic refcount per invoke. **Fix:** cache the `Arc<AtomicU8>` on
`CachedBytecodeMethod` so steady state is one bare `load(Acquire)`.

---

## HIGH-1 — Monitor JFR plumbing does two extra mutex acquires per enter/exit
`vm/src/threading/monitor.rs:407-422`, `interpreter.rs:7003, 7033`.
`set_jfr_enter_recorded`/`jfr_enter_recorded` each call
`self.state.lock()` separately from `enter`/`exit`. The exit-side peek
is `let _`-discarded (no exit event emitted yet — line 7026). Pure
cost. **Fix:** fold the flag into `Monitor::exit`'s return; pass
`enter_recorded` into `enter()` so the inner lock is shared. Or
`#[cfg(feature = "jfr-monitor-pairs")]` until emission lands.

## HIGH-2 — `find_exception_handler_impl` O(N catch entries), no per-method index
`vm/src/runtime/interpreter.rs:4983-5042`. Search is still linear with
HashMap probe + `is_subclass_of` per typed entry. Spring nested
try/catch chains scan all 3-5 per uncaught throw. **Fix:** precompute
`Vec<(catch_class_id, start_pc, end_pc, handler_pc)>` at
`CachedBytecodeMethod` build; binary-search start_pc + direct id compare.

## HIGH-3 — `try_call_compiled_entry` bails on 5+ register args (round-8 stack-arg TODO)
`vm/src/jit/helpers.rs:278-282, 306-309`. Any callee with >4 (no-ctx)
or >3 (with-ctx) Java args takes `bail_to_interpreter`. Hits
`HashMap.putVal(int,K,V,boolean,boolean)` and most JDK NIO/Reflection
ctors. **Fix:** emit stack-arg setup per sysv64/win64 ABI; N=8
register tables close ~99% of JDK arities.

## HIGH-4 — Three near-identical `try_call_compiled_entry + bail` blocks in `jit_invoke_dispatch`
`vm/src/jit/helpers.rs:1514-1531, 1538-1563, 1572-1590`. Carried from
round-7 HIGH-4 verbatim — helpers were extracted but the three sibling
sites still inline `try → bail → 0`. **Fix:** one
`dispatch_or_bail(entry, needs_ctx, vm, vm_ptr, info, args_slice) ->
i64`. -30 LOC.

---

## MED-1 — `class_disables_interp_fast_path` runs 7 `starts_with` per `Frame::new`
`vm/src/runtime/frame.rs:367-388`. Up to 7 prefix compares per frame,
including user code that matches none. **Fix:** compute once at
`CachedBytecodeMethod` install; `Frame::new_from_arcs` copies the bit.

## MED-2 — `Frame::new_from_arcs` padding check is `debug_assert`
`vm/src/runtime/frame.rs:521-526`. Release UB still possible.
**Fix:** promote to `assert!`, or wrap in `PaddedBytecode(Arc<[u8]>)`
newtype constructible only via `padded_bytecode()`.

## MED-3 — `lookup_slot` is `#[inline]`, not `#[inline(always)]`
`vm/src/runtime/vtable.rs:240`. Advisory; JIT-helper callers cross CU
boundaries. **Fix:** promote to `#[inline(always)]` for `lookup_slot`
and `resolve_virtual_method`; verify with `cargo asm`.

## MED-4 — `Monitor::wait` zeroes `entry_count` without snapshotting `jfr_enter_recorded`
`vm/src/threading/monitor.rs:447-450, 533-534`. Safe today via condvar
protocol; future ABI loosening lets a sibling observe stale `true`.
**Fix:** snapshot the flag before release, restore at line 534.

## MED-5 — VM startup re-installs panic hook + 5 signal handlers per process spawn
`vm/src/runtime/crash_handler.rs:269-333`. Visible across `cargo test`
JVM spawn cycles. **Fix:** `OnceLock` gate; expose
`disable_crash_handler` for embedders.

---

12 findings (3 CRIT regression + 4 HIGH + 5 MED).
