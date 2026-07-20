# Embedded-Tomcat integration tests hanging indefinitely on self-connect — FIXED

**Status: FIXED — verified 2026-07-20**

## Symptom (recap)

`RemappedErrorViewIntegrationTests` (and by the same shape,
`BasicErrorControllerIntegrationTests`, `WebMvcAutoConfigurationTests`) start
a real embedded Tomcat via `@SpringBootTest(webEnvironment = RANDOM_PORT)`,
then issue an HTTP request back to `localhost:<ephemeral port>` from the
same process. On `dev`, this hung **indefinitely with zero further log
output** right after `Root WebApplicationContext: initialization
completed` — no exception, no timeout, no JUnit progress — reproduced
consistently in parallel, serial, and fully isolated single-class runs.

## Root cause — CONFIRMED via a live CratonVM stack dump

### The reproduction gap that initially masked the cause

The first attempt to reproduce the hang by launching the release binary
directly (bypassing `run-spring-boot-suite.ps1`) did **not** hang — it
completed normally in 15.7s. The suite runner sets four environment
variables before launching CratonVM that a bare manual invocation omits:

```
CRATONVM_REAL_NET_SOCKETS=1
CRATONVM_REAL_AQS=1
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
CRATONVM_ROOTSNAP_CACHE=1
```

`CRATONVM_REAL_NET_SOCKETS=1` in particular switches `java.net.ServerSocket`
to the real-JDK-matching field layout/native path (see
`apps/spring-boot-suite-runner/run-spring-boot-suite.ps1`'s comment on this
flag). Without it, a materially different code path runs and the bug does
not manifest. Reproducing this class of hang on this codebase requires
replicating the suite runner's env vars exactly, not just its command line.

### The actual mechanism

With the correct environment, CratonVM's own `--stack-dump-on-timeout <N>`
flag captured a live 210-thread dump at the hang point:

- The **main** thread was parked inside
  `BackgroundPreinitializingApplicationListener`'s application-ready event
  handling — Spring Boot's mechanism for kicking off a background thread to
  warm up slow-to-initialize infrastructure (notably Hibernate Validator)
  concurrently with the main startup thread, then synchronizing on it.
- The **`background-preinit`** thread was stuck deep inside Hibernate
  Validator's `ConstraintHelper`/`TypeHelper` reflection-heavy constraint
  enumeration (`ConstraintHelper$StaticConstraintHelper.<init>` ->
  `TypeHelper.extractConstraintValidatorTypeArgumentType`), never returning.

Both threads were doing native-dispatched virtual-method resolution
concurrently, for the first time simultaneously, through a made-newly-hot
code path: commit `3293da963` ("fix(spring): honor anonymous toString
overrides in native formatting") broadened `NativeContextImpl::invoke_virtual`
(`vm/src/vm/vm_exec.rs`) from routing through the heavier, lock-acquiring
`invoke_on_class_shared` **only** for a rare loader-identity-divergence case,
to routing through it for **every** `resolved_from_receiver` call — i.e.
essentially all ordinary virtual dispatch reaching this native entry point,
from any thread. `invoke_on_class_shared_inner` takes `class_manager.read()`
in several places and has additional special-cased native lookups. Making
that the default path for concurrent, first-time class/reflection-heavy
work on two threads simultaneously (exactly what
`BackgroundPreinitializingApplicationListener` is designed to create)
deadlocked them.

Confirmed via bisection across the 91 `dev` commits pulled into
`codex/fix-webmvc-error-timeout-20260718-019f768e`: `3293da963`'s direct
parent (`a7e8b1572`) completed cleanly; `3293da963` itself hung — an exact,
reproducible boundary.

### Why this wasn't the two co-merged fixes

Neither `mapping_match_static` (`native-builtins/src/lib.rs`, only affects
request routing *after* a request has already arrived) nor `http_parse_url`
(`native-builtins/src/net_phase_e.rs`, only changes behavior for
query-only-suffix URLs) touches connect-time or class-dispatch behavior —
both were correctly ruled out early, and the bisection confirms the actual
cause is unrelated to either.

## Fix

Narrowed `invoke_virtual`'s dispatch-selection condition back down:
`invoke_on_class_shared` is now used only for the original loader-identity
divergence case this mechanism was built for, **or** when the call is
specifically an anonymous-object-shaped `toString()` (`method_name ==
"toString"`, descriptor `()Ljava/lang/String;`) — the exact shape
`3293da963`'s own regression test covers. Every other ordinary virtual call
goes back through the lighter `invoke_or_native(&class_name, ...)` path, so
the lock-heavy path is no longer the hot default for arbitrary concurrent
class/reflection work.

```rust
let needs_exact_class_dispatch = resolved_from_receiver
    && (method_name == "toString" && descriptor == "()Ljava/lang/String;"
        || self.shared.class_manager.read().get_loaded_class_id(&class_name)
            != Some(receiver_class_id));
```

## Validation

- `RemappedErrorViewIntegrationTests` (via `run-spring-boot-suite.ps1`,
  with the real suite-runner environment): **2/2 PASS in 15.0s** (was HANG
  at every timeout tried, up to 1800s).
- `cargo test -p cratonvm-vm --test string_format_throwing_tostring`
  (the original anonymous-`toString()` regression test `3293da963` added):
  **passes** — direct call, `"%s".formatted(...)`, and `String.format("%s",
  ...)` all still correctly propagate the anonymous override's thrown
  exception.

## Relationship to `webclient-loopback-self-connect-timeout-os10060-cluster.md`

That doc (still OPEN, root cause not confirmed) describes a *different*,
milder symptom on the same general shape of test (self-connect over
loopback to a just-started embedded server) — a fast, real OS-level `os
error 10060` failure, not a silent indefinite hang. This fix does not touch
networking/socket code at all (the actual mechanism was a VM-internal
dispatch/locking issue, unrelated to sockets), so that doc's underlying
question remains open and is not resolved by this fix.

## Affected classes (confirmed fixed)

| Module | Class |
|---|---|
| `module/spring-boot-webmvc` | `org.springframework.boot.webmvc.autoconfigure.error.RemappedErrorViewIntegrationTests` |

`BasicErrorControllerIntegrationTests` and `WebMvcAutoConfigurationTests`
were identified as sharing the same hang shape but not independently
re-verified against this specific fix this session (they were already
confirmed passing, pre-dating this regression's introduction, in
`webmvc-error-forward-and-multiboot-timeout-cluster-FIXED.md`).
