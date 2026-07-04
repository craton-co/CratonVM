# JIT-triggered SIGSEGV/hang in `core.annotation.*` — default-reachable on `dev`, root cause NOT fully identified

Status: **open, urgent** — reproduces under COMPLETELY DEFAULT settings (no
feature flags), on `dev`'s tip as of 2026-07-04, unrelated to any of the
annotation-proxy work in this session. One narrow, verified-effective
partial mitigation landed (generated-proxy-class JIT ban); the crash is NOT
fully contained. Reproducible, well-characterized, but the exact faulting
Rust function has not been pinned down.

Date observed: 2026-07-04

## Summary

**This is a standing regression on `dev`'s current tip, reachable with zero
special configuration** — plain `--jdk real --jit on` (the suite runner's
own default mode), no `CRATONVM_REAL_ANNOTATIONS`, no other flags. It was
first noticed while investigating something else (annotation-proxy work
under `CRATONVM_REAL_ANNOTATIONS=1`, which appeared to make it *more*
frequent — more JIT-eligible call volume, more chances to hit it — but
`CRATONVM_REAL_ANNOTATIONS` is emphatically **not required**; a full,
default-settings `run-suite.sh --jit on` batch of `core.annotation.*`
crashed 5 of 29 classes with `rc=139`).

Affects (at least — this is 5 classes hit in ~15 minutes of testing, not
necessarily an exhaustive list):

- `AnnotatedElementUtilsTests`
- `AnnotationsScannerTests`
- `MissingMergedAnnotationTests`
- `AnnotationTypeMappingsTests`
- `AnnotationUtilsTests`

**This is NOT a bug in the annotation-proxy fixes** documented in
`mergedannotationstests-proxy-class-identity-reflection-vs-synthesize.md`
(that doc's own "verified effect" numbers carry a caveat pointing here).
Confirmed:

- `--nojit` (or `CRATONVM_DISABLE_JIT=1`): all 5 classes pass cleanly, every
  time. The Rust-level dispatch logic (hashCode/equals/toString/getClass/
  annotationType/getType direct dispatch, the loader-faithful proxy linking
  fix) is correct — this is purely a JIT-compiled-machine-code bug.
- The crash is reachable regardless of whether `CRATONVM_REAL_ANNOTATIONS`'s
  own dispatch fixes are present — it's about the JIT compiling *some*
  combination of code that becomes JIT-eligible once
  `CRATONVM_REAL_ANNOTATIONS=1` reshapes every annotation into a real
  `$ProxyN`, exercising code paths (repeated `hashCode`/`equals`/
  `annotationType()` calls across many distinct `$ProxyN` receiver classes,
  driven by Spring's own meta-annotation introspection) that otherwise don't
  get hot enough to tier up.

## Crash signature

```
EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005)
Faulting access: read at address 0x0000000000000001 (or similar near-null)
Registers: r10=0x18  r14=0x1  r15=0x18   (consistently, across every capture)

Native frames (most recent call first) [raw]:
   0: exe+0x20Cxxx
   1: external/jit
   2: external/jit
   3: external/jit
   4: exe+0xCF8xxx   <- matches the header's faulting `pc` exactly, every time
   5: exe+0xDC2xxx
   6: exe+0xCB311
   7: exe+0x1083F2
   8: exe+0x1013E4
   9: external/jit
```

**The critical finding:** frames 4 through 8 (the real crash site at frame 4,
per `pc`, and its caller chain) are at *the same* RVAs (mod small shifts from
unrelated code changes between builds) **regardless of which of the 5 test
classes is run**, and regardless of which combination of JIT skip-list bans
is active (see "What was tried" below — none of them changed this call
chain at all). This means the bug is **not specific to any particular
Spring/annotation Java method** — it's in some fixed, generic, heavily-used
internal call path (almost certainly a JIT→native-call trampoline used
whenever JIT-compiled code invokes a registered native method), triggered
whenever *some* precondition (probably tier-up count, or a specific
argument-marshaling shape) is met. Package/class-level JIT skip-list bans —
the standard, previously-effective containment tool for this general bug
family (see `docs/internal/jit-regalloc-callee-saved-clobber-family.md`) —
are the wrong instrument for a bug that isn't tied to specific classes.

## What was tried

1. **Bisection** (`CRATONVM_JIT_BISECT_ONLY`, an existing dev tool in
   `vm/src/jit/skip_list.rs` — comma-separated class-name-prefix allowlist,
   everything else forced to skip JIT): `jdk/proxy` alone is clean;
   `org/springframework/core/annotation` alone is clean; **both together**
   reproduces the crash/hang. This is what motivated the containment
   attempts below — but see the "critical finding" above: the fact that two
   narrow allowlists interact doesn't necessarily mean the crash is *in*
   either of them; it may simply mean enough total JIT-compiled call volume
   accumulates once both are eligible.
2. **Ban generated `$ProxyN` classes from JIT** (landed —
   `vm/src/jit/skip_list.rs`, `is_generated_proxy_class`, checked
   unconditionally): fixes the *specific* bisected repro
   (`CRATONVM_JIT_BISECT_ONLY=jdk/proxy,org/springframework/core/annotation`
   running `AnnotationsScannerTests` alone now passes cleanly), but does
   **not** fix the full 5-class repro with no bisect restriction — same
   crash, identical call chain.
3. **Also ban `org/springframework/core/annotation/`** (tried, reverted —
   did not change the outcome; the crash reproduced identically, same call
   chain).
4. **Also widen to the full original `org/springframework/core/` +
   `org/springframework/util/` scope** (mirroring the SPB.1/SPB.2 bans that
   are now inert — see below; tried, reverted — also did not change the
   outcome, same call chain, same registers).

None of the three containment attempts (2 kept, 3 and 4 reverted) changed
the crash's call chain at all, which is the tell that package/class-based
containment is the wrong tool here.

### Why the *existing* `is_known_miscompile` bans don't already cover this

`vm/src/jit/skip_list.rs`'s large family of targeted per-method bans (the
BouncyCastle/Spring-Boot/WildFly/etc. entries, including the pre-existing
`org/springframework/core/` "SPB.2" ban) are gated behind
`callee_saved_gpr_local_homes_enabled()`:

```rust
if policy == SkipPolicy::Conservative {
    if callee_saved_gpr_local_homes_enabled()
        && is_known_miscompile(class_name, method_name)
        ...
```

Another session flipped that flag to default-OFF earlier today
(2026-07-04, "disable callee-saved GPR local homes",
`docs/internal/jit-regalloc-callee-saved-clobber-family.md`), validated via a
4548-class Hibernate ORM soak (94.4% pass, no regressions found). That fix is
real and independently validated — but it silently deactivated the ENTIRE
`is_known_miscompile` ban list as a side effect (since the whole block is
conditioned on that flag), removing whatever safety net it might have
incidentally provided here. This crash is either a residual case the
register-allocator fix doesn't cover, or an unrelated bug that happened to
share the same containment mechanism. The two unconditional checks added in
this investigation (`is_antlr_prediction_context_miscompile`, my new
`is_generated_proxy_class`) are the only entries in this file NOT subject to
that gate.

## Diagnostic tooling fixed along the way (kept — independently correct)

`CRATONVM_SYMBOLIZE` (the offline crash-address symbolizer,
`vm/src/runtime/crash_handler.rs`) was completely non-functional before this
investigation: `SymLoadModuleExW` was called with `DllSize=0`, which lets
`SymInitializeW`/`SymLoadModuleExW` report success but makes every subsequent
`SymFromAddr` fail with `ERROR_INVALID_ADDRESS` (487) — dbghelp has no
registered address range for the module. Fixed by passing a generous
over-estimate (128 MiB) instead of 0. Verified working against a `profsym`
profile build (`[profile.profsym]` in the workspace `Cargo.toml` — release
codegen + full debug info, for exactly this kind of offline symbolization).
**This fix did not end up pinpointing the crash** — the `profsym` profile's
slightly different codegen consistently turned the crash into a hang instead
(same bug family, "hang" flavor — see
`docs/internal/jit-regalloc-callee-saved-clobber-family.md`'s two-symptom
description), so a symbolized *hang* stack was never obtained either (the
`CRATONVM_DBG_HANGWALK` watchdog's conservative stack scan found the thread
genuinely parked in `ZwWaitForAlertByThreadId`, with no usable frame data
beyond that).

## Suggested next steps

1. **Get a real symbol for the crash site.** The blocker was reproducing a
   *crash* (not a hang) in a binary built with debug info — the `profsym`
   profile's codegen differences (despite `inherits = "release"`) were
   enough to flip this specific bug from crash to hang every time it was
   tried (3 attempts). Try: (a) more repetitions — the flip may be
   probabilistic, not deterministic per-profile; (b) a debug-info profile
   closer to plain `release` (try adding `debug = "line-tables-only"` +
   `strip = "none"` as a profile override directly on top of `release`
   rather than routing through the separate `profsym`/`release-with-debug`
   profiles, in case something else in those profiles' settings — not just
   `debug`/`strip` — differs); (c) a live debugger attach (`cdb`/WinDbg) at
   the moment of the fault instead of the process's own crash handler, if
   tooling can be made available.
2. **Question the "two-ingredient" bisection framing.** The fact that no
   combination of package bans changed the (invariant) crash call chain
   suggests the crash may not actually require *specific* Spring/proxy code
   at all — it may just need enough *total* JIT compilation volume/thread
   time to reach a tier-up threshold or hit a timing window. Worth testing
   bisection against a *totally unrelated* hot-JIT workload (no Spring, no
   proxies) that does a comparable volume of native-method calls from
   JIT'd code, to see if the SAME crash reproduces there — if so, this is a
   general JIT/native-call-dispatch bug with no connection to annotations
   whatsoever, and should be retitled/refiled accordingly.
3. Given `is_generated_proxy_class`'s ban IS independently verified to close
   one specific narrow repro (bisected in isolation) even though it's
   insufficient for the full picture, it's being kept — it's a real,
   low-risk containment for at least one interaction, even if it doesn't
   fully solve this doc's headline crash.
4. **Urgent: until this is resolved, `--jit on` against the full
   `core.annotation.*` package is not reliably safe under DEFAULT settings**
   — no `CRATONVM_REAL_ANNOTATIONS` or any other flag is required. Anyone
   running this package's suite on `dev`'s current tip (post the 2026-07-04
   "disable callee-saved GPR local homes" fix) may hit this. The "Verified
   effect" table in
   `mergedannotationstests-proxy-class-identity-reflection-vs-synthesize.md`
   (727/728 clean) reflects one specific run's outcome, not a reliable
   guarantee — the crash is timing/batch-composition-sensitive (observed to
   flip between crash/hang/pass across otherwise-identical reruns). This
   should be flagged to whoever is actively working `jit/src/*` /
   `vm/src/jit/*` today, since it's independent of any of this session's
   changes and was only stumbled into while testing something unrelated.
