# Fix note — nb-appserver-wildfly (B2 / B6 / B7)

Source review: `docs/reviews/fable-2026-06-10/nb-appserver.md`
Owned files: `native-builtins/src/wildfly_core.rs`, `native-builtins/src/jboss_msc.rs`

## Finding

Three WildFly/MSC compat-layer issues that fake or hide real VM gaps in the
**default** build:

* **B2 (critical, synthetic)** — `wildfly_core.rs::native_async_future_task_await`
  force-completed the bootstrap future. When the future was still `WAITING`
  and its `result` field was `null` (i.e. the MSC service container never
  reached STABLE), the native flipped status `WAITING → COMPLETE` in place,
  FAKING a successful WildFly/Keycloak boot. Live by default.
* **B6 (medium, synthetic)** — the `ControlledProcessState` state-transition
  setters (`setStarting`/`setRunning`/`setStopping`/`setStopped` +
  `setRestartRequired`/`setReloadRequired`/`revert*`/`checkRestartRequired`)
  bypassed the real bytecode and mutated only the Rust-side
  `global_model_controller()`, masking an `AtomicStampedReference` VarHandle
  modeling gap (the ASR-backed `state` field reads back `null` on the
  `setStarting()` boot path → NPE → WFLYSRV0239). Live by default.
* **B7 (medium, fault-masking)** — `jboss_msc.rs::drive_starts` recorded a
  failing service `start()` and kept draining, but the failure was only ever
  surfaced under the `CRATONVM_MSC_DBG` env. With debug off the failure was
  silently swallowed, so a half-started container could present as healthy —
  and an uncatchable internal VM error was discarded the same way as a normal
  Java `StartException`.

## Root cause

* B2/B6 are deliberate synthetic shims that paper over two real, still-open VM
  gaps (MSC service-graph not driven to RUNNING in real-JDK mode; ASR VarHandle
  write path not fully modeled). Per the no-synthetic-stub policy, the *default*
  build must not fake — the fakes must be gated off and tagged so the audit and
  `--dump-native-registry` census flag them.
* B7 conflated two failure kinds and gated all visibility on a debug env.

## Exact change

### `native-builtins/src/wildfly_core.rs`

* **B2** — `native_async_future_task_await`: kept the honest paths unchanged
  (already-terminal status; the `has_result` → `COMPLETE` nudge, which is a
  genuine result and stays always-on; the queued-Runnable drain). The
  null-result `WAITING → COMPLETE` flip — the actual fake — is now split by
  `cfg`:
  * `#[cfg(feature = "app-stubs")]`: preserves the prior Keycloak-compat flip
    (still honoring the `CRATONVM_AWAIT_NO_SHORTCIRCUIT` opt-out).
  * `#[cfg(not(feature = "app-stubs"))]` (DEFAULT): returns the real `WAITING`
    status untouched (yielding once), so the unmet MSC-startup gap surfaces
    instead of being masked.
  The `await` registration is now wrapped in
  `r.with_category(NativeKind::SyntheticStub, ...)` so the audit flags it.
* **B6** — the `ControlledProcessState` state-transition setter registrations
  are now `#[cfg(feature = "app-stubs")]` + wrapped in
  `with_category(NativeKind::SyntheticStub, ...)`. In the default build they are
  not installed, so the real bytecode runs and the VarHandle gap surfaces.
  `getState()` (an honest bridge returning the real enum-constant singleton)
  stays registered unconditionally. The six now-conditionally-used setter
  functions got `#[cfg_attr(not(feature = "app-stubs"), allow(dead_code))]` to
  keep the default build warning-clean.
* Added test `b2_b6_wildfly_stub_gating_contract` asserting: `getState` always
  registered; `setStarting` present iff `app-stubs` (and tagged SyntheticStub
  when present); `await` always tagged SyntheticStub. Uses
  `set_drop_synthetic_stubs(false)` for determinism under `CRATONVM_NO_STUBS`.

### `native-builtins/src/jboss_msc.rs`

* **B7** — `drive_starts` now returns `Result<(), MethodCallFailed>` and never
  silently swallows a start failure:
  * The failing service is always recorded as `Failed` (as before) **and** its
    name + failure are logged via `tracing::error!` (no longer gated on
    `CRATONVM_MSC_DBG`).
  * `MethodCallFailed::ExceptionThrown` (a catchable Java exception, e.g.
    `StartException`): MSC-faithful — record + continue draining (now logged),
    matching real MSC, which marks the service `Failed` without aborting the
    install.
  * `MethodCallFailed::InternalError` (an uncatchable Rust/VM error): no longer
    swallowed — propagated out via `return Err(e)`.
  * Re-entrancy-guard trip is now a `tracing::warn!` instead of a debug-only
    `eprintln!`.
* `native_service_builder_install` propagates the new result: after resetting
  the `DRIVING` re-entrancy flag it does `drive_res?;`, so an uncatchable
  start failure surfaces through `install()`'s result path instead of being
  reported as a successful boot.

## Files touched

* `native-builtins/src/wildfly_core.rs`
* `native-builtins/src/jboss_msc.rs`
* `docs/reviews/fable-2026-06-10/fixes/nb-appserver-wildfly.md` (this note)

## Tests added

* `wildfly_core.rs::tests::b2_b6_wildfly_stub_gating_contract` — verifies the
  B2/B6 registration gating + SyntheticStub tagging contract, branching on
  `cfg!(feature = "app-stubs")`.

## Follow-up & risk

* **Behavioral change (intended) in the default build:** WildFly/Keycloak boot
  via `org/jboss/modules/Main.main` that previously rode the B2 force-complete
  will no longer fake-complete in the default build — it returns `WAITING`,
  exposing the real MSC-not-driven-to-RUNNING gap (already a known open gap in
  MEMORY: "MSC service-graph drive to RUNNING"). The `app-stubs` build retains
  the prior Keycloak-compat behavior. Likewise B6 transitions now run real
  `ControlledProcessState` bytecode by default, which will hit the ASR VarHandle
  NPE until that VM gap is fixed — this is the honest surfacing the policy asks
  for, not a regression of a previously-working real path.
* **Real fixes that would let these shims be deleted entirely** (out of scope
  here, both outside my owned files): (1) drive the MSC container to RUNNING in
  real-JDK mode (`jboss_msc.rs` worker/dispatcher wiring — partially in scope
  but the cross-thread `NativeContext` dispatch gap is not); (2) model the
  `AtomicStampedReference` VarHandle write path so `ControlledProcessState`'s
  `state` field is non-null on `setStarting()`.
* **B7 propagation scope:** only `InternalError` propagates; `ExceptionThrown`
  stays record-and-continue to remain MSC-faithful. If a future caller wants
  the thrown Java exception to abort the install too, that is a separate policy
  decision. Low risk: the change only adds logging + an error return on a path
  that previously discarded the error.
