# Parallel Roadmap Handoff — fan-out work items

Decomposition of the feature-designs roadmap into **independently actionable,
parallel-safe** work items, plus the coupled spine that must stay serial. Each
item names its design doc, current status, the **first landable increment**, an
acceptance check, and the subsystem boundary (so parallel agents don't collide).

> Correction to the prior roadmap snapshot: **activate-ir-optimizer's φ/branch
> SIGSEGV blocker is FIXED** (missing SETcc ModRM byte + loop-carried-phi
> back-patch; un-dormanted on `dev` `d641ca49`). It is now unblocked.

## Per-agent contract (the handoff prompt)

> You own ONE roadmap item. Work on a fresh worktree off `dev`. Read the item's
> design doc under `docs/internal/feature-designs/`. Implement the **first
> landable increment** below (code + a focused test). Build/test only the
> narrowest relevant crate; if a full VM build would thrash the box, deliver the
> complete diff + the exact test command instead of blocking on it. Stay inside
> your item's subsystem boundary — do not edit files another item owns. Return:
> what landed, files changed, test/build status, the next 2–3 steps, and any
> blocker. Do not commit to `dev`; leave the work on your worktree branch.

## A. Parallel-safe items (independent of the deopt/GC spine)

1. **jep358-helpful-npe** (M, partial) — doc `jep358-helpful-npe.md`.
   - First: un-stub `getExtendedNPEMessage` (native-builtins `lib.rs:~5065`
     returns null) + the action half ("Cannot invoke … on null",
     `interpreter.rs:~11573`); bci-context backward analysis for the
     `getfield`/`aload`/`invoke` cases. Steps 1–5 are deopt-independent (the bci
     is always known in the interpreter); only step 6 (JIT-NPE parity) is gated.
   - Accept: NPE message matches HotSpot `getExtendedNPEMessage` on a probe.
   - Boundary: `native-builtins/`, `vm/src/runtime/exceptions.rs`, NPE message paths.

2. **proxy-real-classfile** (M, partially built) — doc `proxy-real-classfile.md`.
   - First: audit `define_or_get_proxy_class` (native-builtins `lib.rs:~35365`),
     which falls back to the synthetic `Proxy$Instance` on ANY failure
     (`:~35375`); enumerate the failure modes real apps hit and fix the top one
     so the real `$ProxyN` classfile path is canonical.
   - Accept: real generated proxy used (not synthetic) on a reflection/proxy probe.
   - Boundary: `proxy_gen.rs`, the `define_or_get_proxy_class` path.

3. **keystore-mldsa-mlkem** (M, in progress) — doc `keystore-mldsa-mlkem.md`.
   - First: wire the ML-DSA `Signature` SPI + ML-KEM KEM SPI routing (currently
     stubs in `jca/signature.rs`; only `route_ec_to_real` exists); then the
     `KeyStore` store/write path (`engine_store` absent) and correct the
     over-claimed TCK rows (`tck.rs:~2040`).
   - Accept: ML-DSA sign/verify + ML-KEM encaps/decaps through the JCA SPIs.
   - Boundary: `jca/`, `keystore.rs`, PQC provider routing.

4. **embedding-api** (L, not started) — doc `embedding-api.md`.
   - First: Layer 1 C-ABI — `JNI_CreateJavaVM` / `GetDefaultJavaVMInitArgs` /
     `GetCreatedJavaVMs` (none exist today) + a `cdylib`/`staticlib` crate-type;
     reuse the per-`Vm` extern-C table in `jni.rs`.
   - Accept: a small C harness creates a VM and calls a static method.
   - Boundary: a new `libcratonvm` crate + `jni.rs` invocation entry points.

5. **real-cdi-bean-container** (XL, design done) — doc `real-cdi-bean-container.md`.
   - First scalp (lowest risk): the `ApplicationStartup.DEFAULT` `<clinit>` NPE
     (`spring_startup_bootstrap.rs:20-26`) — retire that pure-observability shim
     by fixing the underlying general VM gap (static-final field on an interface
     not initialised). Do NOT attempt the whole shim cluster.
   - Accept: Spring startup runs the targeted path without the shim.
   - Boundary: `spring_startup_bootstrap.rs`, the `<clinit>`/interface-static fix.

6. **wire-tiered-manager** (L, dormant) — doc `wire-tiered-manager.md`.
   - First: steps 1–2 — stop dropping the recommended tier
     (`interpreter.rs:~14181` binds `_recommended_tier` and discards it); add a
     real background compile thread + enqueue (`on_backedge`/`dequeue_compilation`
     in `tiered.rs` have only test callers today). Steps 1–4 are deopt-independent;
     only step 5 (precise OSR) is gated.
   - Accept: methods enqueue + compile off-thread; no mutator first-call stall.
   - Boundary: `jit/src/tiered.rs`, the invocation/back-edge hook in `interpreter.rs`.

## B. Now-unblocked (was φ/branch-gated)

7. **activate-ir-optimizer** (L, scoped) — doc `activate-ir-optimizer.md`.
   - The φ/branch blocker is FIXED, so the branchy IR path is live. First:
     broaden the passes — add a DSE pass (`ir_optimize.rs` currently has only
     DCE) and/or widen escape→scalar-replacement past the narrow accepted shape.
   - Accept: DSE removes a dead store on a probe; escape analysis fires on more shapes.
   - Boundary: `jit/src/ir_optimize.rs`, `jit/src/escape_analysis.rs`.

## C. Coupled spine — NOT for parallel agents

- **real-frame-deopt-x64-backport** (L, scoped) — gated on the primitive/width
  StackMapTable type source ("single most under-scoped").
- **default-moving-young-gen** (XL) — Route A operand-stack-only first; gated on
  precise rewritable roots + the SAME primitive/width subsystem as the backport.

These two share the under-scoped primitive/width subsystem and the deopt
machinery — keep them serial/coupled (owned by the deopt track), not fanned out.
