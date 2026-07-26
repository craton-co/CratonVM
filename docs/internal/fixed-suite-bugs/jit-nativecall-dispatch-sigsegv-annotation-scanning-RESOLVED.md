# JIT SIGSEGV/hang in `core.annotation.*` — RESOLVED (was the `a11025aa2` invokedynamic/`needs_heap` regression)

Status: **RESOLVED** — this was independently discovered while investigating
annotation-proxy soak testing, then found to be the exact same bug already
root-caused and fixed by another session in
`docs/known-issues/jit-sigsegv-regression-20260704.md` (fix merged to `dev`
via `fix/jit-sigsegv-regression-a11025aa`, commit `042a064c`). See that doc
for the full root-cause writeup and fix details. This doc records the
independent confirmation and the tooling/mitigation found along the way.

Date observed: 2026-07-04
Date resolved: 2026-07-04

## What this doc originally reported

A SIGSEGV/hang in 5 `core.annotation.*` classes (`AnnotatedElementUtilsTests`,
`AnnotationsScannerTests`, `MissingMergedAnnotationTests`,
`AnnotationTypeMappingsTests`, `AnnotationUtilsTests`) under `--jit on`,
reproducible under completely default settings (no
`CRATONVM_REAL_ANNOTATIONS`, discovered while soak-testing that flag but not
actually caused by it). Confirmed via `--nojit` (always clean) that it was
pure JIT-codegen, not application logic. Three JIT-skip-list containment
attempts (ban generated `$ProxyN` classes; also ban
`org/springframework/core/annotation/`; widen to all of
`org/springframework/core/` + `org/springframework/util/`) left the crash's
call chain and register state completely unchanged, which was the tell that
package-level containment was the wrong tool — this was never actually
about *which* Java code was running, only about *how much* JIT-compiled
call volume accumulated.

## Confirmation this was the `a11025aa2` bug

The other session's root cause (`jit-sigsegv-regression-20260704.md`):
`a11025aa2` ("stop blacklisting whole methods for a dead invokedynamic")
made methods containing `invokedynamic` JIT-eligible, lowering it to an
unconditional jump to the shared uncommon-trap deopt stub — but the new scan
arm never set `needs_heap`, so the stub's `vm_ptr` load reads an unreserved
frame slot. For a method whose only heap-requiring-looking construct is a
dead-branch `assert` (the common case the original commit targeted), the
resulting garbage `vm_ptr` crashes deep inside whatever the deopt path
touches first (in the other session's gdb trace, a `Mutex<DeoptimizationLog>`
CAS).

This exactly matches what was found here independently:

- **Register/crash signature invariance** — every capture showed the same
  `r10=0x18`/`r14=0x1`/`r15=0x18`-shaped registers and the same 5-native-frame
  call chain (`exe native → 3× external/jit → exe native → exe native ×4`)
  regardless of which Java class or method triggered it. Consistent with a
  *shared* deopt stub reading a *fixed, wrong* slot, rather than anything
  method-specific.
- **Verified fixed**: rebuilding after merging the official fix
  (`fix/jit-sigsegv-regression-a11025aa`, `042a064c`) made the exact 5-class
  repro that had crashed on every single prior attempt pass cleanly, with no
  further changes needed on this branch. Re-ran the full `core.annotation.*`
  package at both `CRATONVM_REAL_ANNOTATIONS=1` (727/728, `classes:OK=29`,
  zero crashes) and default gate-off (726/728, `classes:OK=28 FAIL=1`, zero
  crashes, byte-identical to the pre-regression baseline) — both clean.

## What was kept from this investigation (independently useful regardless of the above)

1. **`CRATONVM_SYMBOLIZE` (`../../../vm/src/runtime/crash_handler.rs`) was
   non-functional** — `SymLoadModuleExW` was called with `DllSize=0`, which
   lets `SymInitializeW`/`SymLoadModuleExW` report success while every
   subsequent `SymFromAddr` fails with `ERROR_INVALID_ADDRESS`. Fixed by
   passing a generous size estimate. Independent of the crash above — a
   genuine, previously-unnoticed bug in this diagnostic tool.
2. **Generated `$ProxyN` dynamic-proxy classes are now unconditionally
   excluded from JIT compilation** (`../../../vm/src/jit/skip_list.rs`,
   `is_generated_proxy_class`) — proxy method bodies are a handful of
   bytecodes (marshal args, box, call, unbox, return), so this costs
   essentially nothing, and it closes off proxy dispatch as a contributor to
   *any* future variant of "JIT-compiled code calling a native trampoline"
   bugs in this family, independent of whether this specific one recurs.
   Kept as a low-risk, low-cost defense-in-depth measure even though it
   turned out not to be the actual fix needed here.
