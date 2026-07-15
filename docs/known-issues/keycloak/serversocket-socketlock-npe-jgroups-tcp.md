# `java.net.ServerSocket.setReceiveBufferSize()` throws NPE on null internal `socketLock` — JGroups TCP transport startup

Status: open — genuine CratonVM-specific bug candidate

Date observed: 2026-07-14 (4-shard `nonpassed-v3` rerun against `others.tsv`, branch fix/keycloak-nonpassed-rerun-v2-20260710, binary `cratonvm-nonpassed-v3-refresh-20260714.exe`)

## Summary

`model/infinispan :: org.keycloak.jgroups.protocol.JdbcPing2Test::testClearing` fails:

```
=> java.lang.NullPointerException: Cannot enter synchronized block because "this.socketLock" is null
   java.net.ServerSocket.getImpl(ServerSocket.java:249)
   java.net.ServerSocket.setReceiveBufferSize(ServerSocket.java:886)
   org.jgroups.util.Util.createServerSocket(Util.java:4294)
   org.jgroups.blocks.cs.TcpServer.<init>(TcpServer.java:68)
   org.jgroups.blocks.cs.TcpServer.<init>(TcpServer.java:43)
   org.jgroups.protocols.TCP.start(TCP.java:128)
   org.jgroups.stack.ProtocolStack.startStack(ProtocolStack.java:907)
```

JGroups' `Util.createServerSocket()` constructs a `java.net.ServerSocket` and immediately calls
`setReceiveBufferSize(...)` on it (before binding — a legal, common pattern for tuning socket options ahead of
`bind()`). This internally calls the package-private `getImpl()`, which synchronizes on `this.socketLock` — an
internal field the real JDK's `ServerSocket` constructor is supposed to initialize unconditionally. Under
CratonVM, `socketLock` is null at this point, causing the NPE.

## Root cause hypothesis

This points at CratonVM's handling of `java.net.ServerSocket`'s construction/field-initialization for whichever
constructor overload `Util.createServerSocket()` uses (JGroups typically uses the no-arg `ServerSocket()`
constructor, then configures options, then binds separately — as opposed to the more common
`new ServerSocket(port)` one-shot constructor most other code uses). If CratonVM has special-cased or
synthetic handling for `ServerSocket` construction that doesn't fully run the real constructor's field-init
logic for this specific (no-arg, pre-bind, options-then-bind) usage pattern, `socketLock` would be left null —
consistent with this project's established pattern of synthetic/native classes not faithfully replicating every
real-JDK constructor's side effects (see `docs/known-issues/keycloak/reference_overlay_real_class_corruption`-style
findings elsewhere in this project's memory).

## Next steps

1. Find CratonVM's `ServerSocket` native/synthetic constructor handling and confirm which code path
   `new ServerSocket()` (no-arg) takes versus `new ServerSocket(port)` — check whether `socketLock`
   initialization is conditioned on a specific constructor overload.
2. Minimal repro: `ServerSocket s = new ServerSocket(); s.setReceiveBufferSize(1024);` (no bind) — confirm this
   NPEs under CratonVM and succeeds under real HotSpot.
3. Re-verify `JdbcPing2Test` and any other JGroups-based tests (search for other `org.keycloak.jgroups.*` classes
   in `model/infinispan`) once fixed, since this blocks JGroups TCP transport startup broadly, not just this one
   test.

## Repro

```
cd C:\craton\CratonVM-keycloak-nonpassed-v2-20260710
$jdk = '"C:\Program Files\Java\jdk-25"'
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-serversocket-socketlock -ClassList <(printf 'module\tclass\nmodel/infinispan\torg.keycloak.jgroups.protocol.JdbcPing2Test\n') -KeycloakRoot apps\keycloak -Exe target\release\cratonvm-nonpassed-v3-refresh-20260714.exe -JdkHome $jdk
```

## Evidence

`apps/keycloak-suite-runner/.suite/results/nonpassed-v3-shard1/all-jit/logs/model_infinispan.org.keycloak.jgroups.protocol.JdbcPing2Test.out.log`,
2026-07-14 rerun with binary `cratonvm-nonpassed-v3-refresh-20260714.exe` built from `dev` at commit `e85f76d00`.
