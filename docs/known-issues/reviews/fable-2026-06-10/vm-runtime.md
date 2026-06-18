# CratonVM Code Review — `vm` runtime layer

Scope: `vm/src/runtime/` (interpreter, frame, value_stack, invokedynamic, vtable,
exceptions, gc_integration, jit_integration, signals, crash_handler, hprof,
serviceability, tck, jvmti, offload, instrument, unified_logging, lockfree_resolve,
soak_test, + supporting modules), `vm/src/jit/` (helpers.rs, skip_list.rs,
conservative_roots.rs), and `vm/src/native/jni.rs`.

Reviewer: Fable (Opus 4.8). Date: 2026-06-10. Static review only — no build/test run.

---

## Summary

The runtime layer is unusually disciplined for its size. The interpreter, frame,
and value_stack code consistently route recoverable errors through
`VmError`/`RuntimeError` instead of panicking, classfile-derived indices are
accessed via bounds-checked `.get()`/`.get_utf8()` with explicit error returns,
and the conservative GC stack scanner and async-signal-safe crash handler are
carefully engineered (stack-only buffers, alignment + `MAX_SCAN_BYTES` caps,
`heap.is_object_address` validation, re-entry latches). The JIT helper raw-pointer
boundary is heavily commented and mostly hardened against NaN-box/operand-stack
miscompiles.

The most material findings are correctness/soundness asymmetries in the JIT runtime
helpers (`vm/src/jit/helpers.rs`): the array load/store helpers handle the *null*
array case correctly (pending NPE) but silently swallow *out-of-bounds*, and
`jit_getfield` lacks the slot bounds-check that its `jit_putfield_*` siblings have —
on a live inlining codegen path. Plus one infrastructure bug: the CI gate that is
supposed to forbid `unwrap/panic/unreachable` in `interpreter.rs` is neutered by a
doc-comment containing the literal `#[cfg(test)]`, so it scans only the first 33
lines and the invariant is unenforced.

There are essentially **zero** `unimplemented!()`/`todo!()` macros in production code
in this scope; the apparent "NotImplemented" hits are all the `RuntimeError::NotImplemented`
enum variant. That variant, however, is a real stub-signalling hazard: it is mapped to an
**uncatchable** VM-internal error (`exceptions.rs:474`), so any native that returns it
hard-unwinds rather than throwing a Java exception.

---

## Bugs

### B1 (HIGH-as-latent / MEDIUM live) — JIT array load/store helpers silently swallow out-of-bounds instead of throwing AIOOBE
`vm/src/jit/helpers.rs`
- `jit_baload` (1138-1140), `jit_iaload` (1209-1211), `jit_aaload` (1252-1254):
  on `index < 0 || index >= length` they `return 0;` (a fabricated zero / null),
  WITHOUT setting the pending-AIOOBE flag (`set_jit_pending_aioobe`, line 1944) and
  WITHOUT returning the `i64::MIN` deopt sentinel.
- `jit_bastore` (1187-1189), `jit_iastore` (1232-1234), `jit_aastore` (1277-1279),
  `jit_dastore`/`lastore` analogues: silently `return;` (drop the store) on OOB.
- The interpreter only drains a pending AIOOBE inside the `if result == i64::MIN`
  branch (`interpreter.rs:15112-15138`); a normal `0` return never triggers it.
  So a JIT'd OOB array access yields 0/null instead of `ArrayIndexOutOfBoundsException`,
  violating JVMS and masking real user bugs.
- This is the *same* rationale the authors used to FIX the null case in these very
  functions ("Previously returned 0, which silently fabricated a zero byte and masked
  real null-deref bugs"). The OOB arm was not given the same treatment — a clear
  internal inconsistency.
- Reachability nuance: current `jit/src/x64.rs` emits an *inline* `emit_bounds_check`
  before array element access (calls `jit_throw_aioobe`, the correct pending-AIOOBE
  path), and the load helpers appear unused by x64 codegen — so the silent-OOB arm is
  presently a **latent/dead fallback** (any future codegen path, a non-x64 backend, or a
  refactor that routes through these helpers reintroduces the bug). `jit_bastore` *is*
  reached for the null-check stub path, so its OOB arm is one codegen change away from live.
- Fix: make the OOB arm call `set_jit_pending_aioobe(index, length)` and return
  `i64::MIN` (loads) / set the flag (stores), mirroring `jit_throw_aioobe`.

### B2 (MEDIUM) — `jit_getfield` performs an unchecked OOB heap read on a live inlining path
`vm/src/jit/helpers.rs:1378-1394`
- `jit_getfield(obj_ptr, field_index)` reads `*(obj + HEADER_SIZE + field_index*SLOT_SIZE)`
  with **no** bounds check against the object's `num_slots` header field, unlike all the
  `jit_putfield_*` helpers which were hardened via `jit_putfield_slot_in_bounds`
  (1409-1416). The putfield doc-comment (1396-1407) explicitly describes the
  exact failure mode (stale `field_index` under synthetic/real-JDK layout drift reads/
  writes the neighbouring heap object) — the read side was simply missed.
- This helper is actively emitted by codegen on the inlining path
  (`jit/src/x64.rs:10321` and `:14790`, callee getfield during caller compilation),
  so a `field_index` miscompile or layout drift yields an out-of-object heap read whose
  bytes are returned to JIT'd Java as an `i64`/object pointer (info leak + potential
  follow-on UAF if interpreted as a ref).
- `jit_getfield` also returns `0` on `obj_ptr == 0` (no NPE) — same silent-null class as B1.
- Fix: add the symmetric `jit_putfield_slot_in_bounds(obj_ptr, field_index)` guard
  (and ideally set pending NPE on null) to `jit_getfield`.

### B3 (MEDIUM) — `interpreter.rs` panic-free CI gate is neutered (scans only 33 lines)
`vm/src/runtime/interpreter.rs:17242-17248` (`scan_production_section`) + `:17280` (`hot_files_have_no_production_panics`)
- The "production section" boundary is computed as `src.find("#[cfg(test)]")`. The
  FIRST occurrence of that literal in `interpreter.rs` is inside a **doc comment** at
  line 33 (`//! Tests inside \`#[cfg(test)] mod tests { ... }\` are exempt`).
- Result: `production = &src[..byte_offset_of_line_33]`, so the gate scans only the
  file header (all doc comments, which it then skips) and treats lines 34-17114 — the
  entire 17k-line production body — as "test section". The
  `hot_files_have_no_production_panics` assertion therefore passes vacuously.
- Concrete proof it's broken: `interpreter.rs:5505` contains a production
  `_ => unreachable!(),` that the gate is explicitly meant to forbid (it's in the
  `needles` list at 17295), yet the test is green.
- Fix: anchor on a real boundary, e.g. `find("\n#[cfg(test)]")` plus a non-comment
  check, or split the gate file so the test module lives in a separate file.

### B4 (MEDIUM) — Operand-stack overflow surfaces as uncatchable `NotImplemented`, not `StackOverflowError`
`vm/src/runtime/value_stack.rs:314-319` + `exceptions.rs:474-477` + `interpreter.rs:5402-5405`
- `ValueStack::push` returns `RuntimeError::NotImplemented { feature: "operand stack overflow" }`
  on a full stack. `NotImplemented` is mapped to `MethodCallFailed::InternalError`
  (uncatchable; `exceptions.rs:474`) AND is explicitly excluded from the runtime-error→
  Java-exception conversion (`interpreter.rs:5402-5405`), so it unwinds the whole call
  stack (`interpreter.rs:5498-5503`) and cannot be caught by Java `catch (StackOverflowError)`
  / `catch (Throwable)`.
- A JVM operand-stack overflow on unverified bytecode (or a verifier gap) should be a
  catchable `StackOverflowError`. Using the wrong error variant turns a recoverable
  condition into a hard VM-internal abort. (Note `push_checked` at 360 correctly uses
  `IllegalStateException`; `push` uses the wrong variant.)
- Fix: return `RuntimeError::StackOverflowError` (which the interpreter already routes
  to a real Java exception at 5449) from `push`.

### B5 (LOW) — Malformed `ldc`/constant-pool entries throw uncatchable `VmError::Internal`
`vm/src/runtime/interpreter.rs:8877-8933` (`execute_ldc`), similar in `execute_ldc2w`
- A bad CP index or wrong-type entry produces `VmError::Internal{..}` which is an
  uncatchable internal error, not a Java `ClassFormatError`/`LinkageError`. The verifier
  normally prevents this, so it is defense-in-depth, but with `skip_verification` and an
  untrusted classfile it hard-aborts instead of throwing a catchable linkage error.
  (No memory-safety issue — access is bounds-checked.)

---

## Vulnerabilities

Note: JNI entry points (`jni.rs`) are by definition called from *trusted* native code,
not from untrusted classfiles/network data — the JNI contract makes the caller
responsible for valid pointers. The items below are robustness/hardening notes within
that trust model, not classfile-reachable vulnerabilities.

### V1 (LOW) — `GetStringUTFRegion` writes a NUL terminator the JNI spec does not promise (1-byte overflow)
`vm/src/native/jni.rs:2667-2671`
- After copying the modified-UTF8 region into `buf`, the code writes
  `*buf.add(bytes.len()) = 0;`. The JNI spec for `GetStringUTFRegion` does NOT
  null-terminate (unlike `GetStringUTFChars`), so a conforming caller that sizes `buf`
  to exactly the region length gets a 1-byte out-of-bounds write. HotSpot does not write
  this terminator. Recommend dropping the terminator (or documenting the deviation).
- `start + len` overflow is not reachable here: both are `JSize` (i32) `>= 0`-checked,
  so the sum is `< 2^32 << usize::MAX` on 64-bit.

### V2 (LOW) — JNI array-region/critical helpers trust caller-supplied `start`/`len`
`vm/src/native/jni.rs:2329-2407` (get/set region macros), `:2645` (string region)
- Bounds are enforced indirectly: `start as usize + i` is passed to
  `heap.get/set_array_element`, which bounds-checks and returns `Err`/`None` (swallowed
  by `.ok()?` / `let _ =`). So an OOB region request reads/writes nothing past the array
  — memory-safe. The residual risk is purely the trusted-native-caller contract. Fine to
  leave; flagged for completeness.

### V3 (informational) — `push_unchecked`/`set_local_unchecked` rely on the classfile verifier
`vm/src/runtime/value_stack.rs:348-396`, `vm/src/runtime/frame.rs:940-962`
- The fast-path interpreter uses `*_unchecked` variants gated only by `debug_assert!`.
  Under `skip_verification` on untrusted bytecode these degrade to a Rust bounds-checked
  index **panic** (controlled DoS, never UB) — the code documents this precisely. Memory
  safety holds; the only exposure is a panic on adversarial input when verification is off.

---

## Stubs and Unimplemented

No `unimplemented!()` / `todo!()` / no-op-fake-value natives were found in this scope's
production code. The relevant tech-debt surface is the `RuntimeError::NotImplemented`
variant, which is the project's signal for "native not registered / feature absent":

- `RuntimeError::NotImplemented` → uncatchable `InternalError` (`exceptions.rs:474-477`).
  Any native returning it hard-stops execution rather than throwing a Java-catchable
  exception. This matches the project memory's known SecureRandom/SunEC class of
  "uncatchable hard-stop" hazards. Consider mapping unimplemented natives to a catchable
  `UnsupportedOperationException`/`InternalError`-subclass so suites can continue.
- `value_stack.rs:316` reuses `NotImplemented` for "operand stack overflow" — see B4
  (a mislabel, not a stub, but it inherits the uncatchable behavior).
- `offload.rs:294` `.expect("ctx presence checked above")` — a production `.expect()` on
  a documented invariant (GPU offload path). Low risk; prefer returning a
  `LookupOutcome::Blacklisted` over a panic.
- `crash_handler.rs:1638` "placeholder that documents this limitation",
  `offload.rs:942` "placeholder cuda_bridge::Stream equivalent" — documented partial
  implementations on non-core paths, not fake-app-behavior stubs.
- `interpreter.rs:2688` `let code_size = 0usize; // TODO: expose compiled code size` —
  a serviceability metric hardcoded to 0 (reported via management interface). Cosmetic.

---

## Performance

### P1 — `std::env::var(...)` / `var_os(...)` called on hot opcode paths
`vm/src/runtime/interpreter.rs` (e.g. `:6177`, `:6242`, `:6269`, `:15115`) and array-store diag closures
- Several array load/store error closures and diagnostic gates call `std::env::var("CRATONVM_DBG_AIOOBE")`
  /`var_os(...)` on each invocation. `std::env::var` locks the process environment and
  allocates a `String`. These are only hit on the error/exception path (AIOOBE), so impact
  is bounded, but the project already has `env_cache.rs` for cached gates — route these
  through it (one-time `OnceLock<bool>`), matching `jit_putfield_diag()`/`disable_jit()`.

### P2 — `ec_is_watched_class` and per-store `arrstore_check` use `class_name.contains(...)` substring scans
`vm/src/runtime/interpreter.rs:202-213`, `:143-188`
- `ec_is_watched_class` does up-to-4 `String::contains` substring scans per class (memoized,
  so amortized OK). `arrstore_check` formats and reads thread frames; it is gated by
  `arrstore_enabled()` (cached) so it is off by default — fine. Keep these debug-only paths
  behind the cached gate (they are) and ensure they never run in release-default.

### P3 — `format!`/`to_string()` for diagnostic context built eagerly before the gate check
`vm/src/runtime/interpreter.rs:6230-6232`, `:6258-6260`, `:6285-6287`, `:6307-6309`
- The `_diag_pc`/`_diag_method`/`_diag_class` triple (with `.to_string()` on class/method
  names) is materialized on EVERY `aastore`/`iastore`/`lastore`/`dastore`, then only used
  inside the `pop_object_ref_ctx_with` closure / the `CRATONVM_DBG_AIOOBE` branch. The
  `.to_string()` allocations happen unconditionally on a hot store path. Defer them into the
  error closure (they are already closures for `pop_object_ref_ctx_with`, but the bare
  `let _diag_method = ...to_string()` lines execute eagerly). Move the `to_string()` inside
  the `format!` closure so the common (non-error) path allocates nothing.

### P4 — `find_exception_handler_impl` re-`get_class` per catch entry
`vm/src/runtime/interpreter.rs:5897`
- Inside the per-entry loop, `cm_guard.get_class(frame.class_id)` is called again for each
  exception-table entry to fetch the constant pool. The owning class is invariant across the
  loop; hoist the `&Class`/`&ConstantPool` borrow out of the loop (only the lazy-load slow
  path needs to drop+reacquire the guard). Minor; only matters for methods with many handlers
  during exception storms.

---

## Tests

Estimated coverage for this scope: **~55-60%** (best estimate; does not plausibly reach 85%).

Basis (read, not run):
- Strong unit coverage in: `value_stack.rs` (60 tests — push/pop/tag/cat-2/overflow),
  `signals.rs` (61), `tck.rs` (99), `serviceability.rs` (90), `jvmti.rs` (66),
  `interpreter.rs` (114 — but mostly arithmetic helpers, descriptor parsing, float
  conversions, NOT full opcode dispatch), `vtable.rs` (38), `gc_integration.rs` (51),
  `jit_integration.rs` (50). Plus a large `vm/tests/` integration suite (93 files:
  differential, exception edge, jck conformance, jit arity, monitor stress, etc.).
- `jit/helpers.rs` (20 tests): thoroughly covers the **null** array/field cases
  (`jit_baload_null_sets_pending_npe`, `jit_iastore_null_sets_pending_npe`, etc.),
  NaN-box length stripping, negative-length, and the SATB pre-barrier. **No test exercises
  the out-of-bounds load/store path** — the exact gap behind B1. A test like
  `jit_iaload(valid_array, oob_index)` would pass today with the silent-0 behavior, which
  is why the bug survives.
- `conservative_roots.rs` (15) and `crash_handler.rs` (20): cover chain push/pop/prune and
  signal-handler scaffolding.

Coverage gaps / most important missing tests:
1. **JIT helper OOB array access** (B1) — assert AIOOBE is raised (pending-flag set /
   sentinel returned) for `jit_iaload`/`jit_baload`/`jit_aaload`/`*store` on out-of-bounds.
2. **`jit_getfield` slot bounds** (B2) — assert an out-of-range `field_index` does NOT read
   past the object; add the symmetric test the `jit_putfield_*` helpers have.
3. **Operand-stack overflow → StackOverflowError** (B4) — assert a deeply-nested
   push-heavy method yields a *catchable* `StackOverflowError`, not an uncatchable abort.
4. **The CI gate itself** (B3) — a meta-test asserting `scan_production_section` actually
   covers the full production body (e.g. by checking `total_lines` is ~17k, not ~33).
5. Exception-table walking under lazy catch-type loading and missing catch types
   (`find_exception_handler_impl`).
6. JNI region/string edge cases (start+len at boundary, `GetStringUTFRegion` terminator
   behavior — V1).

The aggregate test count is high, but it skews toward management/diagnostics surfaces
(tck/serviceability/jvmti) and helper-function unit tests; the core 17k-line opcode
dispatch in `execute_instruction` is mostly covered indirectly by the integration suite
rather than by targeted unit tests, and the JIT-helper error contracts have a real,
demonstrated coverage hole on the OOB paths.

---

## Feature Suggestions

1. **Centralize the "JIT helper exceptional-return" contract.** B1/B2 stem from each
   helper hand-rolling its null/OOB handling. Introduce a small typed result
   (`enum JitFault { Npe, Aioobe{index,length}, NegativeArraySize, None }`) and a single
   `surface_jit_fault` drain so every helper sets the flag uniformly and no arm can
   silently fabricate a value. This makes the silent-0 class of bug structurally
   impossible.
2. **Make `RuntimeError::NotImplemented` catchable.** Map it to a real Java throwable
   (e.g. `java/lang/InternalError` or `UnsupportedOperationException`) so an unregistered
   native degrades gracefully (suite continues) instead of hard-unwinding — directly
   addresses the recurring "uncatchable hard-stop" class noted in project memory.
3. **Fix and strengthen the panic-free CI gate (B3) and extend it** to `frame.rs`,
   `value_stack.rs`, and `jit/helpers.rs`, with a self-test that the scanned region is the
   real production body.
4. **A debug "strict OOB" mode** that routes JIT array access exclusively through the
   helpers (bypassing inline `emit_bounds_check`) under an env gate, so the helper OOB
   contract is exercised in CI and can't silently rot.
5. **Cache the remaining `std::env::var` opcode-path gates through `env_cache.rs`** (P1)
   and add a clippy lint / grep CI check forbidding `std::env::var(` outside `env_cache.rs`.
6. **JNI hardening pass**: align `GetStringUTFRegion` (no terminator) with the spec and add
   an optional `CRATONVM_JNI_CHECK` mode (like HotSpot `-Xcheck:jni`) that validates
   region start/len and pointer canonicality, surfacing native-caller bugs early.

---

## Files sampled vs fully read

Fully read (key regions, sometimes the whole logical unit):
- `vm/src/jit/helpers.rs` — array/field/static helpers (738-1700), invoke dispatch (2278+),
  uncommon_trap (3227+), `build_helpers` (3736+), and the entire test module (3285-end).
- `vm/src/jit/conservative_roots.rs` — stack scan (820-889), remap (705-749), precise
  scan (767+), structure of the rest.
- `vm/src/runtime/interpreter.rs` — array load/store opcodes (6155-6315), exception
  conversion/handler routing (5390-5520, 5870-5929), `execute_ldc` (8851-8937),
  `count_method_params` (16905-16944), JIT-return drain (15080-15155), the CI-gate
  self-test (17220-17308), and structural grep of all `fn` signatures (104-16984).
- `vm/src/runtime/crash_handler.rs` — Unix signal handler (1177-1281), Windows VEH
  (525-614), platform Command usage (1424-1435).
- `vm/src/runtime/value_stack.rs` — push family + NotImplemented sites (300-400).
- `vm/src/runtime/frame.rs` — local get/set (926-1010), GC remap loop (1270-1309).
- `vm/src/runtime/exceptions.rs` — runtime-error→exception mapping (460-488).
- `vm/src/native/jni.rs` — region macros (2329-2407), string region (2645-2674),
  critical encode/decode (2676-2810), object-array (2110-2211), structural grep of all
  entry points.
- `vm/src/runtime/gpu_marshal.rs` — host_view/write_back copy helpers (58-130) (out of
  priority scope; spot-checked for raw-copy soundness — found sound).

Sampled (grep + targeted reads, not exhaustive):
- `vm/src/runtime/invokedynamic.rs` (BSM resolution 115-149; no production panics),
  `vtable.rs`, `gc_integration.rs`, `jit_integration.rs`, `signals.rs` (NPE-message gen),
  `serviceability.rs`, `jvmti.rs`, `tck.rs`, `instrument.rs`, `unified_logging.rs`,
  `lockfree_resolve.rs`, `soak_test.rs`, `offload.rs`, `lock_order.rs`,
  `jit/skip_list.rs` — surveyed for `unimplemented!/todo!/NotImplemented`,
  `unsafe`, command-exec, and unwrap/expect; no additional high-severity findings.
- `jit/src/x64.rs` — read only to characterize B1/B2 reachability (inline
  `emit_bounds_check` at 11012/12545+, getfield helper-call at 10321/14790). Out of scope
  for findings.
