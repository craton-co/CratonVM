# Round-7 VM audit

Scope: `vm/` crate. Combines (A) round-6 wave-1 regression audit and (B)
remaining HIGH/MED items from earlier rounds.

---

## CRIT-1 — `jit_iastore` / `jit_bastore` / `jit_aastore` null-array NPE flag leaks across JIT calls
File: `vm/src/jit/helpers.rs:691,735,777`; checked by interpreter only at
`vm/src/runtime/interpreter.rs:12414,12397` (inside `if result == i64::MIN`).
Loads return `i64::MIN` as a deopt sentinel; stores return `void`. When a
JIT'd method does `arr[i] = x` on a null array, `set_jit_pending_npe()` flips
the TLS flag but the helper just returns and the JIT continues executing.
The post-JIT NPE drain only runs after a sentinel return; a subsequent
JIT entry that *does* deopt (for a different reason) then reads the
stale flag and reports an NPE attributed to the wrong PC / method.
**Fix:** stores must also deopt — return early via a thread-local
"abort current JIT method" flag, or have the codegen emit an explicit
deopt-to-interpreter trampoline after every `jit_iastore`/`jit_bastore`/
`jit_aastore` call. Alternatively, clear the NPE flag at every JIT entry
in `execute_jit_call` (mirroring the `take_jit_pending_aioobe` reset on
`try_osr` at line 11213-11214).

## CRIT-2 — `Frame::new_from_arcs` does not pad bytecode; doc silent
File: `vm/src/runtime/frame.rs:453-488`. `Frame::new` pads via
`padded_bytecode()`; `new_from_arcs` accepts the `Arc<[u8]>` verbatim. The
hot interpreter loop reads `code[pc+1]` and `code[pc+2]` unconditionally
relying on the 2-byte zero padding. Today the single caller at
`interpreter.rs:2285` pre-pads, but the doc comment ("zero-copy for code
and strings") implies the caller may pass any `Arc<[u8]>`. A future
caller that hands in raw classfile bytes triggers an OOB read past the
allocation tail. **Fix:** `debug_assert!(code.len() >= 2 &&
code[code.len()-1] == 0 && code[code.len()-2] == 0)` and document the
invariant on the function. Better: rename to `new_from_padded_arcs`.

---

## HIGH-1 — OSR-rejection block duplicated 14× in `execute_frame`
File: `vm/src/runtime/interpreter.rs:2721,3052,3091,3127,3157,3172,3187,
3202,3300,3314,3328,3342,3356,3370`. Round-6 wave-1 added `should_try_osr` /
`record_osr_rejection` but kept the 12-line orchestration around each
back-edge inline (drop frame borrow → `should_try_osr` → `try_osr` →
recycle/pop/push value → fall-through to `record_osr_rejection`). The
last 8 sites were collapsed onto single 200-character lines so the
duplication is visually hidden but bytewise identical. **Fix:** factor
into `try_osr_at_backedge(shared, thread, frame_idx, initial_frame_idx,
entry_pc) -> ControlFlow<Option<Option<Value>>>` (Continue → safepoint;
Break(ret) → propagate). Cuts ~140 lines and removes a maintenance trap
where future PGO/JFR additions must be replicated 14 ways.

## HIGH-2 — `class_disables_interp_fast_path` runs `contains("springframework")` per `Frame::new`
File: `vm/src/runtime/frame.rs:355`. Substring scan over class name on every
frame creation, including non-Spring workloads which pay for nothing.
Round-5 flagged this; still present. **Fix:** replace with
`starts_with("org/springframework/")` (the package always begins with that
prefix in classfile internal form) — single pointer comparison after
length check. Saves a Boyer-Moore-class scan per `Frame::new`.

## HIGH-3 — `Frame` field ordering wastes cache locality
File: `vm/src/runtime/frame.rs:99-174`. Hot interpreter fields (`pc`,
`code`, `stack`, `locals`, `max_locals`, `max_stack`, `is_jdk_class`) are
interleaved with cold ones (`inner: FrameInner` ~96B, `osr_attempt_counts`
24B empty-Vec header, `monitor_on_exit` 16B, `last_instr_pc`). On 64B
cache lines, the hot fast-path touches 2-3 lines per opcode where 1
would suffice. **Fix:** reorder so `(class_id, pc, code, stack, locals,
max_stack, max_locals, is_jdk_class, backward_count)` sit at the top of
the struct (first ~88 bytes); push `inner`, `osr_attempt_counts`,
`monitor_on_exit`, `last_instr_pc` below. Add `#[repr(C)]` if Rust
otherwise reorders.

## HIGH-4 — three near-identical JIT-dispatch bail blocks in `jit_invoke_dispatch`
File: `vm/src/jit/helpers.rs:1399-1407, 1431-1438, 1458-1465`. After the
round-5/6 extraction of `try_call_compiled_entry` + `decode_dispatch_values`
+ `bail_to_interpreter`, the three sibling sites (DISPATCH_CACHE hit,
JIT cache hit, post-compile) still inline the same `try → bail → 0`
pattern. **Fix:** one helper
`dispatch_or_bail(entry, needs_ctx, vm, vm_ptr, info, args_slice) -> i64`
that encapsulates the three-line ritual. Removes 18 lines and the next
"silent 0" footgun.

---

## MED-1 — Monitor JFR gating re-checks `is_enabled()` racily
File: `vm/src/runtime/interpreter.rs:6789-6814`. `jfr_on` is sampled
before `monitors.enter()` and decides whether `Instant::now()` runs.
If JFR is *disabled* before `enter()` and *enabled* mid-`enter()`, the
threshold check on exit silently skips. This is documented intent
(events that race the enable get the next one) but the cost asymmetry
isn't: `is_enabled()` is a relaxed atomic load and is cheap enough to
re-check on the exit branch too. Currently we miss high-cost monitor
events for the entire duration of a hot enable.
**Fix:** also test `cratonvm_jfr::is_enabled()` on the `if let Some(start)
= mon_start` branch — if enabled, record the event even when the start
sample was `None`, using the current time as both start and end.

## MED-2 — JIT-route exception path allocates `Vec<Value::Uninitialized>` per throw
File: `vm/src/runtime/interpreter.rs:4980-4982`. Every JIT-thrown
exception that lands in a Java catch handler does
`(0..cached.num_params).map(|_| Value::Uninitialized).collect()` just
to feed `Frame::new_pooled`'s args path, which then re-fills the locals
to `Uninitialized` anyway. On hot try/catch loops (Spring init,
Jackson polymorphic deserializers) this is one fresh `Vec` per throw.
**Fix:** add `Frame::new_pooled_for_exception` that skips
`copy_args_to_locals` (locals already initialise to `Uninitialized` in
`init_locals_pooled` after `resize`). Avoids the `Vec` allocation and
the per-arg branch.

## MED-3 — `OSR_MAX_ATTEMPTS=5` per-method `Vec<(usize, u32)>` linear scan
File: `vm/src/runtime/frame.rs:374-407`. `should_try_osr` and
`record_osr_rejection` linear-scan the vec on every back-edge above the
threshold. Comment claims "only a handful of loops" — true for hand-
written code, false for generated bytecode (Spring AOP method
interceptors, Kotlin coroutines state machines with 20+ continuation
points). At ~20 entries the linear scan is repeatedly walked from every
back-edge once OSR fails once. **Fix:** keep the linear scan but split
the hot check: if `osr_attempt_counts.is_empty()` (the common case
before any reject) skip the lookup entirely; cache the last-seen
`(entry_pc, idx)` pair in a `Cell<usize>` for O(1) re-find on the next
back-edge of the same loop.
