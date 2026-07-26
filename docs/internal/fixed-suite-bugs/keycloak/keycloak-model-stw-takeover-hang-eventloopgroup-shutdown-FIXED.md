# STW cross-thread takeover hang during Netty EventLoopGroup shutdown (--nojit) - FIXED

Status: FIXED (2026-07-08). Moved out of `../../../known-issues` per the
known-issues triage rule.

Date observed: 2026-07-07, while running `RealmModelTest` with `--nojit` to
work around
[`keycloak-model-infinispan-jit-adjacent-decode-error-fullname`](../../known-issues/keycloak-model-infinispan-jit-adjacent-decode-error-fullname.md).

## Symptom

With `--nojit`, `RealmModelTest` got through Infinispan cache-manager
bootstrap, use, and teardown, then hung indefinitely while stopping
`io.netty.channel.EventLoopGroup`. The log repeated:

```text
WARN cratonvm_vm::runtime::interpreter: STW cross-thread JIT takeover is still waiting for cooperative mutators rounds=64 pending=1 taken=0
```

The important detail was `pending=1`: one event-loop worker was parked in a
blocking native wait and never reached the cooperative STW acknowledgement
point.

## Root cause

The real `native-io` NIO selector implementation already bracketed blocking
selector waits with the VM's blocked-thread protocol. The older synthetic S2
selector path in `../../../../native-builtins/src/servlet.rs` did not.

That S2 path still registers `java/nio/channels/Selector.select()` and
`select(long)` for the Keycloak/Netty path. For positive or infinite waits it
called `selector_poll(...)` directly, and the empty-selector path either
blocked in the wakeup poll or slept. None of those waits called
`NativeContext::begin_blocking_region()` / `end_blocking_region()`. A thread
parked in `poll`/`WSAPoll` was therefore still counted as a cooperative mutator
for STW, but could not execute Java bytecode or poll the safepoint request.

The same area also had a moving-GC hazard: the per-selector wakeup channel was
keyed by raw `ObjectRef` pointer bits. If a moving collection relocated the
selector while one thread was blocked in `select()`, a shutdown-thread
`Selector.wakeup()` could compute a different key and miss the channel that the
blocked thread was polling.

## Fix

`../../../../native-builtins/src/servlet.rs`:

- Key S2 selector wakeup channels by `(ctx.vm_identity(), ctx.identity_hash_code(sel))`
  instead of raw `ObjectRef` address bits.
- Thread the `NativeContext` through `ensure_wakeup_channel`,
  `release_wakeup_channel`, `signal_wakeup`, and `drain_wakeup`.
- Wrap every nonzero-timeout S2 selector wait in
  `begin_blocking_region()` / `end_blocking_region()`, including the
  empty-selector fallback.
- Pin selector-local object references across the blocked wait and read them
  back after wakeup so a moving GC can remap them safely.

`../../../../native-builtins/src/test_utils.rs`:

- Add blocked-region counters to `MockNativeContext` so unit tests can assert
  that blocking selector waits enter and leave the blocked-thread protocol.

## Verification

Real suite-runner repro, after the fix:

```powershell
$suite = 'C:\craton\CratonVM\apps\keycloak-suite-runner\.suite'
$list = Join-Path $suite 'keycloak-model-realm-stw-20260708-001.tsv'
"module`tclass" | Set-Content -Path $list -Encoding ascii
"testsuite/model`torg.keycloak.testsuite.model.RealmModelTest" | Add-Content -Path $list -Encoding ascii

& 'C:\craton\CratonVM\apps\keycloak-suite-runner\run-keycloak-suite.ps1' `
  -ClassList $list -Category others -Vm craton -Jit off -Parallel 1 `
  -TimeoutSec 900 -RunName 'keycloak-stw-eventloopgroup-verify-20260708-001' `
  -KeycloakRoot 'C:\craton\CratonVM\apps\keycloak' -WorkDir $suite `
  -Exe 'C:\craton\cargo-targets\keycloak-stw-eventloopgroup-20260708-001\release\cratonvm-keycloak-stw-eventloopgroup-20260708-001.exe'
```

Result: no hang. The runner returned in 108.644s with status `FAIL`, not
`HANG`/timeout. The log reaches:

```text
Changed status of io.netty.channel.EventLoopGroup to STOPPING
Changed status of io.netty.channel.EventLoopGroup to STOPPED
```

and contains no `STW cross-thread JIT takeover is still waiting` signature.
This confirms the original EventLoopGroup shutdown hang is closed.

The class then failed later and cleanly with a separate CratonVM-specific
residual:

```text
java.lang.ExceptionInInitializerError
Caused by: org.infinispan.commons.CacheConfigurationException:
ISPN000436: Cache '___protobuf_metadata' has been requested, but no matching cache configuration exists
```

HotSpot `-Xint` control for the same class/list passes in 142.6s:

```powershell
& 'C:\craton\CratonVM\apps\keycloak-suite-runner\run-keycloak-suite.ps1' `
  -ClassList $list -Category others -Vm hotspot -Jit off -Parallel 1 `
  -TimeoutSec 900 -RunName 'keycloak-stw-eventloopgroup-hotspot-control-20260708-001' `
  -KeycloakRoot 'C:\craton\CratonVM\apps\keycloak' -WorkDir $suite
```

That protobuf metadata residual is now fixed and archived in
[`keycloak-model-protobuf-metadata-cache-config-missing-FIXED.md`](keycloak-model-protobuf-metadata-cache-config-missing-FIXED.md).
The remaining deeper no-JIT timeout is tracked separately in
[`docs/known-issues/keycloak-model-liquibase-xerces-xml-parse-nojit-timeout.md`](../../known-issues/keycloak-model-liquibase-xerces-xml-parse-nojit-timeout.md).

Narrow native selector regression group covering the root mechanism:

```powershell
$env:CARGO_TARGET_DIR='C:\craton\cargo-targets\keycloak-stw-eventloopgroup-20260708-001'
cargo test -p cratonvm-native-builtins selector -- --nocapture
```

Result: `21 passed; 0 failed`, including:

- `servlet::tests::s2_empty_selector_wait_enters_gc_blocked_region`
- `servlet::tests::s2_registered_selector_wait_enters_gc_blocked_region`
- existing S2 selector poll and wakeup tests

Also built a release binary in the same unique target directory and copied it
to:

```text
C:\craton\cargo-targets\keycloak-stw-eventloopgroup-20260708-001\release\cratonvm-keycloak-stw-eventloopgroup-20260708-001.exe
```

That binary was used for the real CratonVM suite-runner verification above.

## Cross-reference

Retired from
`docs/known-issues/keycloak-model-stw-takeover-hang-eventloopgroup-shutdown.md`.
