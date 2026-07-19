# `module/spring-boot-cloudfoundry` 2026-07-17 rerun: skip-SSL-verification not honored (1 FAIL, confirmed) + `$Proxy` layout-probe livelock (2 HANGs)

**Status: OPEN — found 2026-07-17**

This module contributed 3 non-passing classes to this triage batch,
splitting into 2 unrelated root causes.

## Issue A — `SkipSslVerificationHttpRequestFactoryTests`: a custom permissive `X509TrustManager` has no effect, so the "should skip verification" client still rejects the peer certificate

```
JUnit Jupiter:SkipSslVerificationHttpRequestFactoryTests:restCallToSelfSignedServerShouldNotThrowSslException()
    => org.springframework.web.client.ResourceAccessException: I/O error on GET request for "https://localhost:52907/hello": handshake process: invalid peer certificate: certificate expired: verification time 1784319908 (UNIX), but certificate is not valid after 1413976735 (370343173 seconds ago)
       org.springframework.web.client.RestTemplate.doExecute(RestTemplate.java:760)
     Caused by: javax.net.ssl.SSLHandshakeException: handshake process: invalid peer certificate: certificate expired: verification time 1784319908 (UNIX), but certificate is not valid after 1413976735 (370343173 seconds ago)
       org.springframework.http.client.SimpleClientHttpRequest.executeInternal(SimpleClientHttpRequest.java:89)
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-cloudfoundry.org.springframework.boot.cloudfoundry.autoconfigure.actuate.en-a5d1434fffe5.out.log`
(and matching `.err.log`).

### Root cause (CONFIRMED at file:line precision)

The test (`SkipSslVerificationHttpRequestFactoryTests.java`, this worktree)
starts a real embedded Tomcat with an intentionally self-signed/expired
`test.jks` keystore, then asserts that a `RestTemplate` built with
`SkipSslVerificationHttpRequestFactory` succeeds (HTTP 200) against it, while
a plain `RestTemplate` throws `SSLHandshakeException` — i.e. the test's
entire point is to verify the skip-verification factory actually skips
verification. `SkipSslVerificationHttpRequestFactory`
(`org.springframework.boot.cloudfoundry.autoconfigure.actuate.endpoint.servlet.SkipSslVerificationHttpRequestFactory`,
this worktree) does this the standard JSSE way: builds an `SSLContext`
initialized with a custom, always-trusting `X509TrustManager`
(`checkServerTrusted` is a no-op, never throws) and installs it via
`HttpsURLConnection.setSSLSocketFactory(context.getSocketFactory())`.

CratonVM's native bridge for this (`native-builtins/src/t27_tls.rs`) *does*
have machinery to consult a custom Java `TrustManager`
(`ctx_trust_managers_table`, `engine_run_trust_check`,
`t27_tls.rs:5556-5610`) — it looks up any `TrustManager[]` captured off the
`SSLContext` and calls the real Java `checkServerTrusted` on it. But this
consultation is wired in as a **post-handshake, secondary check**:
`engine_take_pending_trust_check` (`t27_tls.rs:5516-5554`) only fires *after*
`state.conn` reports the handshake `!is_handshaking()`, i.e. after rustls's
own internal certificate-chain validation has already completed
successfully. rustls performs its own default certificate validation as
part of establishing the connection — it has no way to be told "skip your
own validation, a permissive Java `TrustManager` will decide instead" from
this code path — so for a certificate rustls itself refuses (expired, in
this case), the handshake fails *before* `engine_run_trust_check` (and
therefore before the custom `SkipX509TrustManager.checkServerTrusted`) ever
runs. The custom `TrustManager` can only make an already-rustls-approved
connection *stricter* (reject something rustls accepted); it structurally
cannot make a connection rustls has already rejected *more permissive*. This
precisely matches the observed error text (`"handshake process: invalid peer
certificate: certificate expired"` — a rustls-native error message shape,
not a Java `CertificateException` a thrown-from-`checkServerTrusted` would
produce) and the fact that the assertion fails on the *first* line
(`restTemplate.getForEntity(...)`, the one that's supposed to succeed via the
skip-verification factory) rather than the second (which correctly still
throws for the plain `RestTemplate`).

This was confirmed by reading `t27_tls.rs`'s actual trust-check wiring, not
by attaching a live debugger — the architectural gap (rustls's own
verifier runs unconditionally before any custom `TrustManager` is
consulted) is clear from the code, but the exact rustls `ServerCertVerifier`
construction site that would need to become skip-aware was not located this
session.

## Issue B — 2 classes HANG in a tight, zero-progress `gc::guard` retry loop against a Spring `$Proxy` object

| Class | Receiver object |
|---|---|
| `CloudFoundryReactiveActuatorAutoConfigurationTests` | `org/springframework/core/$Proxy51`, `class_id=2454`, `num_slots=1`, `index=1` |
| `CloudFoundryActuatorAutoConfigurationTests` | `org/springframework/core/$Proxy50`, `class_id=2288`, `num_slots=1`, `index=1` |

Both `.out.log`s are completely empty (no JUnit banner, no
`SBRUNNER_RESULT`). Both `.err.log`s show, after normal startup, a burst of
the same `gc::guard` out-of-bounds-field-read warning repeating at
**sub-millisecond** intervals (e.g. `20:19:29.439137` → `.439460` →
`.439783`, i.e. ~300µs apart — much tighter than the ~150ms-2s cadence seen
in the sibling `InterceptingExecutableInvoker` livelock cluster, see below)
against the exact same `$Proxy5{0,1}` object, then (for
`CloudFoundryReactiveActuatorAutoConfigurationTests`) a `WARN
cratonvm_vm::vm::vm_exec: Missing native method in real-JDK mode
method=io/netty/handler/codec/quic/Quiche.quiche_version()Ljava/lang/String;`
line, after which the log ends (killed at shard timeout).

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-cloudfoundry.org.springframework.boot.cloudfoundry.autoconfigure.actuate.en-7decd5b54db9.err.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-cloudfoundry.org.springframework.boot.cloudfoundry.autoconfigure.actuate.en-7729cc9d2f53.err.log`

### Root cause

**Not root-caused; related to, but a distinct receiver shape from, the
already-filed
[`junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster.md`](junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster.md).**
That cluster's own "Also checked and NOT added" note (added independently
the same day by a parallel triage batch) explicitly excludes a
`spring-boot-pulsar` class with this same `org/springframework/core/$Proxy*`,
`num_slots=1` shape from its `InterceptingExecutableInvoker`/`num_slots=0`
cluster on the grounds that the receiver class and cadence both differ —
these 2 CloudFoundry classes are further, independent instances of that same
excluded `$Proxy` shape (not the `InterceptingExecutableInvoker` one), now
seen in a 3rd module. The guard's own diagnostic text ("speculative
collection-layout probe dispatched on a non-matching receiver type") applies
identically: `index=1` against a `num_slots=1` object means slot 1 is being
read on an object that only has slot 0 — some caller is treating a
1-field Spring `$Proxy` (a JDK dynamic proxy backing an
`InvocationHandler`-style interface, `real_field_count=Some(1)`) as if it
had at least 2 fields, in a hot retry loop that never observes a usable
value and so never proceeds to whatever it's trying to reach (both `.out.log`s
being completely empty means this happens before any JUnit test method
runs). Not identified: which specific caller issues the probe, nor whether
this `$Proxy` livelock family (this doc + the excluded `spring-boot-pulsar`
class) shares a root cause with the `InterceptingExecutableInvoker` family —
plausible given the identical guard/mechanism, but the receiver classes
differ enough that this session does not merge them. Needs a live repro
(`CRATONVM_DBG_JIT_DISASM` or `--nojit` bisection) to identify the actual
caller, same as the sibling cluster's own recommended next step.

## Affected classes

| Module | Class | Issue |
|---|---|---|
| `module/spring-boot-cloudfoundry` | `org.springframework.boot.cloudfoundry.autoconfigure.actuate.endpoint.servlet.SkipSslVerificationHttpRequestFactoryTests` | A |
| `module/spring-boot-cloudfoundry` | `org.springframework.boot.cloudfoundry.autoconfigure.actuate.endpoint.reactive.CloudFoundryReactiveActuatorAutoConfigurationTests` | B |
| `module/spring-boot-cloudfoundry` | `org.springframework.boot.cloudfoundry.autoconfigure.actuate.endpoint.servlet.CloudFoundryActuatorAutoConfigurationTests` | B |
