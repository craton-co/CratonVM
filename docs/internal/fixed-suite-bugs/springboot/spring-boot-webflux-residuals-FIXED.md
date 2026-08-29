# `module/spring-boot-webflux`: two unrelated residuals (NPE + HANG) — FIXED

**Status: FIXED — 2026-07-19** (both primary defects; see "New residual
discovered" below for a separate, unrelated issue found during
verification)

Original doc: `docs/known-issues/springboot/spring-boot-webflux-residuals.md`
(found 2026-07-17). Worktree
`CratonVM-springboot-webflux-residuals-20260718-019f7681`, branch
`codex/fix-springboot-webflux-residuals-20260718-019f7681`.

## Issue A — `RecordableServerHttpRequestTests.getRemoteAddress()`: NPE, `InetSocketAddress.getAddress()` returns null

### Root cause

`new InetSocketAddress(port)` (the single-int constructor) delegates, per
the real JDK, to the `(InetAddress, int)` constructor with the resolved
wildcard address (`0.0.0.0`/`::`) — NOT a null/unresolved address.
`native-builtins/src/phases_early.rs`'s `register_phase52_inet_socket_address`
registered this constructor with `Value::Object(None)` for the address,
so `getAddress()` returned `null` and `RecordableServerHttpRequestTests
.getRemoteAddress()` NPE'd on `InetSocketAddress.getAddress().toString()`.

### Fix

`native-builtins/src/phases_early.rs`, `<init>(I)V` for `InetSocketAddress`:
synthesize the resolved wildcard `InetAddress` (via
`net_phase_e::alloc_inet_address_external(ctx, "0.0.0.0", "0.0.0.0")`)
instead of leaving the address null.

### Verification

- New regression test `vm/tests/inet_socket_address_port_only.rs` +
  fixture `vm/tests/resources/cratonvm/InetSocketAddressPortOnly.java`:
  constructs `new InetSocketAddress(0)`, asserts `getAddress()` is
  non-null, `isUnresolved()` is false, and the address `isAnyLocalAddress()`.
  Runs the fixture in both JIT and `--nojit` modes. **PASS** (both modes).
- Direct binary invocation of the fixture: `INET_SOCKET_ADDRESS_PORT_ONLY_OK
  0.0.0.0`, exit 0, in both modes — reconfirmed after every rebuild in this
  session (pre-merge, post-merge with `origin/dev`, and on final `dev` tip).
- `module/spring-boot-webflux`'s `RecordableServerHttpRequestTests`: full
  class run via `spring-boot-suite-runner`, **5/5 PASS** (up from 4/5).

## Issue B — `WebFluxManagementChildContextConfigurationIntegrationTests`: HANG

### Root cause

Two contributing gaps in loader-aware symbolic resolution, both in
`vm/src/runtime/interpreter.rs`:

1. **`getfield`/`putfield` never used loader-aware field resolution.**
   `resolve_field_ref_loader_aware` (the JVMS §5.4.3-faithful resolver,
   already wired to `getstatic`/`putstatic` via an earlier fix,
   `19d7f4170`) was never used for the instance-field opcodes. Wired
   `Instruction::Getfield`/`Instruction::Putfield` to call it instead of
   the loader-blind `resolve_field_ref`. Also hardened the per-callsite
   field-resolution cache: a cache entry seeded before a custom loader
   defined its own copy of the field-owning class is no longer trusted
   merely because the loader has since resolved that name — the cached
   `declaring_class_id` must match (or be an ancestor of, matching
   `dev`'s independently-landed validation logic for the getstatic/
   putstatic path, `resolve_class_loader_aware`/`is_subclass_of`) the
   freshly-resolved owner.
2. **`MergedAnnotation$Adapt.isIn()` cross-loader identity split.** Spring's
   `@CompileWithForkedClassLoader` test machinery can produce two loader-local
   copies of this private nested enum used within one logical annotation
   operation. Real `Adapt.isIn(Adapt... adaptations)` bytecode does a
   reference-identity scan (`c == this`), which fails when the receiver and
   an array element are equal-by-value but distinct objects from two
   different loader copies. Added a narrowly-scoped identity bridge in the
   invoke-dispatch hot paths (`execute_invoke_kind`,
   `execute_invokevirtual_vtable_fast`, `execute_invokevirtual_cached`) that
   compares by declaring-class name + enum constant name/ordinal instead of
   raw reference identity for this exact `(class, method, descriptor)`
   triple, and forces the cache-miss slow path so a call-site cache
   populated before the loader fork can't paper over the identity mismatch.

   **Not redundant with `dev`'s own fix for the same bug** (a `MergedAnnotation
   $Adapt.isIn` native override + `force_native_over_real_jdk_bytecode` gate,
   landed independently via a separate session's "isolated-loader-*" cluster
   while this fix was in progress) — empirically verified: removing this
   inline bridge and relying solely on `dev`'s native override reintroduced
   the hang (stuck within seconds of the first sub-test; confirmed under a
   *low* host-load window, ruling out contention). Kept both mechanisms.

### Verification

- Direct suite-runner isolation (`-Parallel 1`), pre-merge (dev base
  `40678d0f5` + this fix only): **3 clean full completions** — 130s,
  120.9s, 144.1s wall time (host-load-dependent), each **4/5 tests
  passing**. The remaining failure (`No qualifying bean of type
  ObjectProvider<TomcatConnectorCustomizer>`) is an unrelated, pre-existing
  Spring bean-autowiring gap, not part of this bug's scope.
- Previously: 0-byte stdout, silently wedged ~35s into JUnit discovery, no
  exception, no summary (see original doc). Now: full JUnit summary, real
  bean-creation exceptions on the one genuinely-failing sub-test.
- `cargo test -p cratonvm-vm --lib --release`: regression-clean at every
  checkpoint (pre-merge: 2211 passed/19 failed; post-`origin/dev`-merge:
  2219 passed/19 failed; final `dev` tip after this branch merged: 2219
  passed/19 failed) — all 19 failures are the same pre-existing
  `jit::skip_list::*`/`native::jni::*`/`runtime::lock_order::*`/
  `buffered_input_stream_real_jdk_uses_its_own_bytecode` release-build
  artifacts in every run; zero new failures at any checkpoint.
- A production-panic regression was introduced and caught mid-session: an
  `.expect("checked is_some")` in the field-cache validation added above
  tripped this repo's own `hot_files_have_no_production_panics`/`b3_gate_
  scans_full_production_body_of_interpreter` tests (which enforce zero
  panics in `interpreter.rs`'s production code paths). Fixed by rewriting
  the check as `loader_local_id.map_or(true, |id| ...)` (no panic
  possible); reconfirmed clean afterward.

## New residual discovered — NOT part of this fix, NOT fixed here

While reverifying Issue B after merging 122 new `origin/dev` commits into
this branch, `WebFluxManagementChildContextConfigurationIntegrationTests`
started hanging again — but at a **different, later** point than the
original bug: after Tomcat actually starts, stalled inside Hibernate
Validator's classloader resource lookup for `../../../../apps/META-INF/validation.xml`
(`ResourceLoaderHelper: Trying to load ... via user class loader` → `via
TCCL` → `via Hibernate Validator's class loader`, then nothing further).

**Root-caused to a pre-existing `dev` regression, unrelated to this fix**:
confirmed via a from-scratch worktree built at pure `origin/dev` tip
(no trace of this branch's changes at all) — the identical class hung at
the identical stall point, under a verified-low-host-load window (39-44GB
free RAM, 3-12 concurrent build processes on this shared box). Likely
introduced by one of the classloader-related fixes in the 122-commit
window (candidates: the `URLClassLoader.getResourceAsStream`/`ClassUtils
.forName`/"isolated-loader-*" cluster), not bisected further — out of
scope for this task. Filed as a new, separate open issue:
`docs/known-issues/springboot/webfluxmanagementchildcontext-hibernatevalidator-classloader-hang.md`.

## Affected classes

| Module | Class | Outcome |
|---|---|---|
| `module/spring-boot-webflux` | `org.springframework.boot.webflux.actuate.web.exchanges.RecordableServerHttpRequestTests` | FIXED, 5/5 |
| `module/spring-boot-webflux` | `org.springframework.boot.webflux.autoconfigure.actuate.web.WebFluxManagementChildContextConfigurationIntegrationTests` | Original hang FIXED (4/5, verified pre-merge); a NEW, unrelated `dev` regression now blocks a clean run post-merge — see above |
