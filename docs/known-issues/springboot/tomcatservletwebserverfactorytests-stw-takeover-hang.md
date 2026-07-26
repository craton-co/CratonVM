# `TomcatServletWebServerFactoryTests` (SSL-heavy runs): intermittent STW cross-thread JIT takeover hang — OPEN

**Status: OPEN — found 2026-07-26**, while verifying
[`tomcatservletwebserverfactorytests-ssl-clientauth-peercert-residuals.md`](tomcatservletwebserverfactorytests-ssl-clientauth-peercert-residuals.md).
Not a regression from that doc's fix — reproduced identically on both a
pre-fix and a post-fix binary, in different full-class reruns of the same
test class. Filed separately since it's a distinct, general VM-concurrency
bug category (see the many prior `STW cross-thread JIT takeover` fixes under
`docs/internal/fixed-suite-bugs/{wildfly,keycloak}/*stw-takeover*`), not
specific to TLS/peer-cert handling.

## Symptom

A full run of `TomcatServletWebServerFactoryTests` (132 test methods, most of
which start and stop a fresh embedded Tomcat) occasionally — roughly 1 run in
3-5 in this session's testing — hangs indefinitely partway through, always
right after a fresh HTTPS connector for one of the `ssl*` test methods starts
(observed directly after the `NioEndpoint.certificate` "configured from
keystore ... with trust store [null]" log line, i.e. right as a
`ClientAuth.WANT`-configured connector begins accepting). The only log output
is the recurring VM warning:

```
WARN cratonvm_vm::runtime::interpreter: STW cross-thread JIT takeover is still waiting for cooperative mutators rounds=64 pending=1 taken=0
```

`rounds=64 pending=1 taken=0` repeats without ever resolving — one thread
never reaches the cooperative STW acknowledgement point. No stack dump was
captured for this specific occurrence (`--stack-dump-on-timeout` was not
armed on the runs that hit it); only a plain `timeout(1)`-triggered kill was
used to unstick the host.

## Not yet root-caused, but a strong existing-pattern match

Every prior `STW cross-thread JIT takeover` hang fixed in this codebase
(`docs/internal/fixed-suite-bugs/wildfly/wildfly-standalone-boot-stw-jit-takeover-hang-FIXED.md`,
`docs/internal/fixed-suite-bugs/keycloak/keycloak-model-stw-takeover-hang-eventloopgroup-shutdown-FIXED.md`,
several more) traced to the SAME shape of bug: some native blocking call (a
selector wait, an accept loop, a socket read) was not bracketed with
`NativeContext::begin_blocking_region()`/`end_blocking_region()`, so a thread
parked inside it is still counted as a "cooperative mutator" by the STW
takeover protocol but can never actually reach a safepoint poll — the
takeover then waits forever for a thread that will never arrive. Given this
hang appears specifically around fresh HTTPS connector startup/accept in
these tests, the most likely candidate is a TLS-adjacent blocking call in
`native-io`/`native-builtins/src/servlet.rs` (the NIO2 accept loop, or the
rustls handshake's own blocking read/write path) missing the same bracketing
its sibling call sites already have — but this is an unconfirmed hypothesis,
not verified against a live thread/stack dump for this specific trigger.

## Suggested next step

Reproduce with `--stack-dump-on-timeout <N>` armed (`N` below the observed
hang point, e.g. 20-30s) to capture live thread states at the moment of the
stall — the `pending=1` count means exactly one thread is the holdout; its
captured stack should point directly at the missing blocking-region bracket,
the same way the two linked prior fixes were root-caused. `CRATONVM_DBG_TLS_AUTH=1`
combined with the stack dump would additionally show whether the stuck
thread is inside the rustls handshake, the native accept loop, or an
unrelated component that merely happens to run concurrently during these
tests.

## Reproduction

```
cd module/spring-boot-tomcat
export CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_ROOTSNAP_CACHE=1
export TMPDIR=/data/tmp/<scratch>   # keep off a full root fs on the Azure host
<cratonvm-exe> --java-home <jdk25> --Xmx 2g --stack-dump-on-timeout 25 \
  -Dfile.encoding=UTF-8 -Djava.awt.headless=true -Djava.io.tmpdir=/data/tmp/<scratch> \
  -cp <module classpath> SbRunner org.springframework.boot.tomcat.servlet.TomcatServletWebServerFactoryTests
```

Does not reproduce on every run — rerun a handful of times if the first
attempt completes cleanly. Not observed at all on plain HotSpot (132/132 PASS
in every HotSpot rerun this session).

## Affected classes

| Module | Class | Note |
|---|---|---|
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.servlet.TomcatServletWebServerFactoryTests` | Intermittent full-suite hang, ~1 in 3-5 runs, always near SSL connector startup |
