# ES FAIL - RestClientBuilder SSL UnknownIssuer reopened

Status: FIXED

Date observed: 2026-07-10

## Fix applied (2026-07-10)

Root cause: `javax.net.ssl.SSLContext.getDefault()` had **no way to observe**
a prior `SSLContext.setDefault(ctx)` call and always allocated a fresh,
unconfigured `SSLContext` (no trust/key managers, no rustls state) on every
invocation.

There are (were) four separate native registrations for `SSLContext.
getDefault`/`init` in this codebase, registered in this call order inside
`register_essential_natives` (`native-builtins/src/lib.rs`):

1. `phases_late.rs::register_p68_ssl`, called directly at
   `native-builtins/src/lib.rs:33722`.
2. `net_phase_e.rs::register_re6_ssl_context`, called via
   `net_phase_e::register_phase_e_networking(registry)` at
   `native-builtins/src/lib.rs:33745` (AFTER #1).
3. `tls.rs::register_ssl_context`, called via
   `register_tls_natives(registry)` at `native-builtins/src/lib.rs:39809`.
4. `phases_late.rs::register_p68_ssl` again, called via
   `register_phase68_natives(registry)` at
   `native-builtins/src/lib.rs:39812`.

The native method registry is last-registered-wins (plain `HashMap::insert`,
`native-api/src/registry.rs`), so by call order alone #4 looks like the
winner. **It is not.** Calls #3 and #4 both live inside
`register_synthetic_overrides`, which `vm/src/native/builtins.rs` gates
behind `#[cfg(feature = "synthetic-jdk")]` — a no-op shim when that feature
is off, which it is by default (`vm/Cargo.toml`: `synthetic-jdk` is not in
the default feature set). This is the exact trap already documented in
`reference_synthetic_jdk_feature_gate_trap` for the same class of code
(SSLContext/SSLSocketFactory/SSLSocket via `phases_late.rs`). So in the
default real-JDK build the only two LIVE registrations are #1 and #2, and
#2 (`net_phase_e.rs`) wins.

Confirmed empirically (not just by static call-graph reading, per the same
memory's warning about this exact registration-precedence machinery having
burned past sessions): added a temporary `eprintln!` gated on
`CRATONVM_DBG_TLS_AUTH` inside all three candidate `getDefault()`/`init()`
closures (`phases_late.rs`, `net_phase_e.rs`, `tls.rs`), rebuilt, and reran
`RestClientBuilderIntegTests` with that env var set. Only the
`net_phase_e.rs` markers fired (5x `getDefault`, 3x `init`); the other two
files' markers never printed. The debug markers were removed before the
final fix landed (`phases_late.rs`/`tls.rs` were reverted to their
pre-investigation state; the temporary markers were also stripped from
`net_phase_e.rs`).

`net_phase_e.rs`'s `getDefault()` always allocated a fresh context — and
critically, **no native for `SSLContext.setDefault` was registered
anywhere** in any of the three files (confirmed by grep) — so
`testBuilderUsesDefaultSSLContext`'s second phase (`SSLContext.setDefault
(getSslContext())` followed by a `RestClient` build that internally calls
`SSLContext.getDefault()` at `RestClientBuilder.java:330`) always got a
brand-new, unconfigured context with no trust roots, and the handshake
failed with `UnknownIssuer` exactly as if no truststore had ever been
configured. `testBuilderSetsThreadName` and the `idle-timeout-task`
`ThreadLeakError` were downstream fallout from the same hung/failed
connection.

### Fix

- Added `SSLContext.setDefault(SSLContext)` (previously missing) in
  `native-builtins/src/net_phase_e.rs`, alongside `getDefault`/`init` in
  `register_re6_ssl_context`. It stores the passed `ObjectRef` into a new
  GC-safe global slot.
- `getDefault()` now checks that slot first and, if set, returns the SAME
  object `setDefault` was given — not a copy — so whatever trust/key-manager
  state `SSLContext.init()` already attached to it (via `t27_tls.rs`'s
  GC-stable identity-hash-keyed `ctx_trust_managers_table`/
  `ctx_key_managers_table`) is naturally still found on the next lookup. No
  identity data needed to be duplicated.
- New storage in `native-builtins/src/t27_tls.rs`, mirroring the existing
  `ctx_trust_managers_table`/`gc_scan_tls_ctx_trust_manager_roots` pattern
  exactly: a `Mutex<Option<ObjectRef>>` behind a `OnceLock`
  (`default_ssl_context_slot`), with `set_runtime_default_ssl_context`/
  `get_runtime_default_ssl_context` accessors and
  `gc_scan_default_ssl_context_root`/`gc_update_default_ssl_context_ref`
  root-scan/remap companions (a raw `ObjectRef` held outside the Java heap
  is invisible to a moving GC otherwise).
- Wired into `vm/src/memory/roots.rs` (scan side) and `vm/src/memory/gc.rs`
  (update side), alongside the existing trust/key-manager table wiring.
- Left `phases_late.rs`'s and `tls.rs`'s dead `getDefault`/`init`
  registrations untouched — they are unreachable in the default build and
  out of scope for this fix.

### Verification

`RestClientBuilderIntegTests` — before: FAIL, 2 tests, 2 failures
(`SSLHandshakeException: rustls: invalid peer certificate: UnknownIssuer`,
plus a downstream `AssertionError`/`ThreadLeakError`). After: PASS, 2/2.

`RestClientBuilderTests` (plain unit-test sibling, no HTTPS) reconfirmed
unaffected.

Date fixed: 2026-07-10

## Original report (superseded by the fix above)

Observed in:
- Run: `esfull-20260710-083851`
- Host: local Windows box
- CratonVM binary: `C:\craton\cratonvm-targets\es-full-local-20260710-083851\release\cratonvm-es-full-local-20260710-083851.exe`
- Suite mode: `craton` / JIT on
- Stopped partial run totals: 367 recorded classes, 69 PASS, 283 FAIL, 15 HANG, 0 CRASH

Affected class:
- `client/rest org.elasticsearch.client.RestClientBuilderIntegTests`

CratonVM result:
- FAIL, 27.310s, 2 tests, 4 failures.

Primary signal:
```text
javax.net.ssl.SSLHandshakeException: rustls: invalid peer certificate: UnknownIssuer
```

Additional fallout:
```text
java.lang.AssertionError
com.carrotsearch.randomizedtesting.ThreadLeakError: 1 thread leaked from SUITE scope
Thread[id=3, name=idle-timeout-task, state=RUNNABLE, group=TGRP-RestClientBuilderIntegTests]
```

HotSpot control:
- Run: `esprobe-hotspot-restbuilder-20260710`
- Same class: PASS, 1.5s.

Evidence:
- Craton stdout: `C:\craton\esfull-20260710-083851\results\esfull-20260710-083851\jit-shard1\logs\client_rest.org.elasticsearch.client.RestClientBuilderIntegTests.out.log`
- Craton stderr: `C:\craton\esfull-20260710-083851\results\esfull-20260710-083851\jit-shard1\logs\client_rest.org.elasticsearch.client.RestClientBuilderIntegTests.err.log`
- HotSpot result: `C:\craton\esfull-20260710-083851\results\esprobe-hotspot-restbuilder-20260710\hotspot-restbuilder\results.tsv`

Relationship to older fixed note:
- `docs/internal/fixed-suite-bugs/elasticsearch-restclient-builder-ssl-handshake-residual.md` recorded an older RestClientBuilder TLS residual as fixed on 2026-07-04.
- This current-dev result reopened the class as an active known issue with a DIFFERENT root cause (missing `SSLContext.setDefault`, not the identity-key GC-stability issue the 2026-07-04 fix addressed).
