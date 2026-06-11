# Fix note — vm-runtime-lows

Owned file: `vm/src/runtime/interpreter.rs` (only file edited).
Report: `docs/reviews/fable-2026-06-10/vm-runtime.md` (B5, B3, P1, P2, P4).

---

## B5 (LOW) — FIXED: malformed `ldc`/`ldc2_w` constant-pool entries are now catchable

**Problem.** A bad CP index or wrong-type entry in `execute_ldc` /
`execute_ldc2w` produced `VmError::Internal{..}`, which maps to an
**uncatchable** `MethodCallFailed::InternalError`. Under `skip_verification`
with an untrusted classfile this hard-aborts the VM instead of throwing a
Java-catchable linkage error.

**Fix.**
1. The five malformed-classfile sites in `execute_ldc` (bad CP index, bad
   `string_index`, bad class `name_index`, bad condy `name_and_type`,
   unsupported entry type) and the two in `execute_ldc2w` (CP index out of
   range, non-Long/Double entry) now return
   `VmError::Linkage(LinkageError::ClassFormatError { class_name, message })`
   instead of `VmError::Internal`. The "current class not found" sites are
   left as `Internal` on purpose — that is a genuine VM invariant, not a
   classfile defect.
2. New `#[cold] fn convert_ldc_class_format_error(shared, thread, err)`
   (next to `execute_ldc`) converts a
   `MethodCallFailed::InternalError(VmError::Linkage(ClassFormatError{..}))`
   into a real `ExceptionThrown(java/lang/ClassFormatError)` via the existing
   `exceptions::create_exception_object`, falling back to the original error
   if the exception object can't be built (heap-exhausted / rt.jar absent) —
   never panics, never loses the diagnostic. Mirrors the existing
   `convert_class_not_found` / AbstractMethodError pattern.
3. The `Ldc` / `LdcW` / `Ldc2W` dispatch arms in `execute_instruction` apply
   it via `.map_err(|e| convert_ldc_class_format_error(shared, thread, e))?`.
   Same `(shared, thread)` borrow shape already proven by the three
   `convert_class_not_found` call sites, so it compiles under NLL.

`catch (ClassFormatError)` / `catch (LinkageError)` / `catch (Throwable)` in
Java can now observe these instead of the VM hard-unwinding.

## B3 (MEDIUM) — FIXED: panic-free CI gate now scans the real production body

**Problem.** `scan_production_section` anchored its boundary on
`src.find("#[cfg(test)]")`. The FIRST literal occurrence of that string is a
`//!` doc comment near the top of each hot file (interpreter.rs:33,
x64.rs:22), so the gate scanned only the ~33-line header and treated the
entire 17k-line dispatch body as "test code". `hot_files_have_no_production_panics`
passed **vacuously** while a production `unreachable!()` (and an `.expect()`)
survived in `interpreter.rs`.

**Fix.**
- Rewrote `scan_production_section` to walk line-by-line and skip only the
  bodies of genuine `#[cfg(test)]`-gated items (a real attribute line, not a
  comment), tracking brace depth. This correctly excludes the trailing
  `mod tests { ... }` AND any mid-file `#[cfg(test)] fn helper {...}`
  (vm_exec.rs has one in its production region) while scanning everything
  else. Now returns `(hits, scanned_lines)`. Verified by an independent
  simulation: interpreter.rs scans **17395 lines, 0 production hits**
  (previously ~33 lines).
- Fixed the two real production panic sites the widened gate exposed in
  interpreter.rs (my owned file):
  - The `code_attr_opt.expect("has_code true implies code present")` after
    the AbstractMethodError block → `match { Some(c) => c, None => return
    Err(VmError::Internal{..}) }`.
  - The dead `_ => unreachable!()` arm in the `match exc_result` (the only
    two `MethodCallFailed` variants are already handled, so the match is
    exhaustive) → removed.
- Rewrote `hot_files_have_no_production_panics`:
  - `interpreter.rs` is now strict **zero** (clean after the two fixes).
  - `vm_exec.rs` (1 site: thread-spawn `.expect`) and `jit/src/x64.rs` (16
    sites: JIT codegen-invariant `unreachable!`/layout `.unwrap`) are
    **ratcheted** at their current owned-elsewhere baselines — the count may
    only shrink, never grow, so a *new* `.unwrap()` in those files still
    fails the gate. (I do not own those files, so their pre-existing sites
    are documented as a baseline rather than asserted to zero.)
  - Added a `scanned > 1000` self-check so a future regression of the
    boundary logic back to the doc-comment anchor fails loudly instead of
    silently passing.

### Note on files I do not own
`vm_exec.rs` and `jit/src/x64.rs` carry legitimate-looking production panic
sites (thread-spawn failure; JIT codegen invariants). They are now *scanned
and ratcheted* (regressions blocked) but their existing sites are tolerated
via the baseline. A follow-up owned by the vm_exec / jit agents could drive
those baselines to 0 (vm_exec.rs:3203 thread-spawn `.expect`; the 16 x64.rs
`unreachable!`/`.unwrap` codegen-invariant sites).

## P1 (perf) — FIXED: cache `CRATONVM_DBG_AIOOBE`/`AIOOBE2` gates

Added cached `aioobe_dbg()` / `aioobe2_dbg()` (`OnceLock<bool>`) next to the
existing `arrstore_enabled()` / `no_cleaners()` gates, and routed the five
`std::env::var("CRATONVM_DBG_AIOOBE")` / `var_os(...AIOOBE2)` calls on the
array load/store error paths through them. No more per-invocation env lock +
`String` alloc.

## P2 (perf) — NO CHANGE NEEDED (confirmed gated)

`ec_is_watched_class` is per-ClassId memoized AND only called inside
`if crate::runtime::ec_watch::enabled()` (cached gate). `arrstore_check` is
only called behind the cached `arrstore_enabled()` gate. Both are off in
release-default exactly as the report concluded; converting the memoized
substring scan to a precomputed set adds risk for no measurable gain, so per
"only if clearly safe and local" I left them as-is.

## P4 (perf) — NO CHANGE (hoist not safely achievable)

`find_exception_handler_impl` already hoists the class-existence check out of
the loop (line ~5992) and fetches `get_class` only for entries that pass the
PC check and are typed (a cheap held-guard `HashMap` get). A full hoist of
the `&Class`/`&ConstantPool` borrow across the loop is blocked by the
lazy-load slow path, which `drop`s and re-acquires the read guard — holding
the borrow across that `drop` is a borrow-checker violation, and pre-scanning
catch-type names into owned `String`s would allocate on the common path
(contradicting P3). The report rates this "Minor"; left unchanged.

---

## Tests added (interpreter.rs `mod tests`)
- `ldc_class_format_error_is_catchable_or_falls_back` — `ClassFormatError`
  → `ExceptionThrown` (or preserved error without rt.jar), never panics.
- `ldc_converter_passes_through_unrelated_errors` — a non-CFE `Internal`
  error passes through `convert_ldc_class_format_error` untouched.
- `b3_gate_scans_full_production_body_of_interpreter` — meta-test asserting
  the scan covers >10k lines and 0 production panic sites (guards against
  re-breaking the boundary).

## Compile confidence: HIGH
- All edits mirror existing in-file idioms (`OnceLock<bool>` gates;
  `.map_err(|e| conv(shared, thread, e))?`; `create_exception_object`
  fallback). `LinkageError`/`VmError`/`MethodCallFailed`/`JvmThread` already
  imported (line 65/71). New tests use the established
  `Vm::new(VmConfig::new())` + `vm.shared`/`&mut vm.main_thread` pattern.
- Scanner behavior validated by an independent brace-tracking simulation over
  the edited file (0 production hits / 17395 lines).
- No `#[cfg]`-gated paths touched, so default / app-stubs / synthetic-jdk all
  build identically. No `clippy::{unwrap,expect,panic,unimplemented,todo}_used`
  patterns added to production code (test-only `.unwrap()/panic!` are exempt
  under the `not(test)` deny header).
