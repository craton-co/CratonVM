# STW cross-thread takeover hangs during Netty EventLoopGroup shutdown (--nojit)

Status: open

Date observed: 2026-07-07 — surfaced while running `RealmModelTest` with
`--nojit` to work around
[keycloak-model-infinispan-jit-adjacent-decode-error-fullname](../internal/fixed-suite-bugs/keycloak-model-infinispan-jit-adjacent-decode-error-fullname-FIXED.md).

## Summary

With `--nojit`, `RealmModelTest` runs all the way through Infinispan
cache-manager bootstrap (protostream schema registration, JGroups cluster
topology recovery — "Cluster recovery found %d caches, members are 0") and
into test teardown: `GlobalComponentRegistry`/`EmbeddedCacheManager` and its
many components (`CacheManagerJmxRegistration`, `QueryCache`,
`CertificateReloadManagerFactory`, `ClusterTopologyManagerFactory`,
`LocalTopologyManagerFactory`, `TelemetryServiceFactory`,
`MarshallerFactory`, ...) all correctly transition `STOPPING` → `STOPPED` in
order — but then hangs indefinitely while stopping
`io.netty.channel.EventLoopGroup`, repeatedly logging:

```text
WARN cratonvm_vm::runtime::interpreter: STW cross-thread JIT takeover is still waiting for cooperative mutators rounds=64 pending=1 taken=0
```

`rounds` keeps incrementing (confirmed still logging at rounds=64+ after
several minutes); `pending=1` the whole time — one thread never reaches the
requested safepoint. Despite the log message's "JIT takeover" wording, this
reproduces under `--nojit`, so either the message is a generic/reused label
for a broader stop-the-world safepoint mechanism (not JIT-compilation-
specific), or something about `--nojit` doesn't fully disable whatever
triggers this STW request.

## Hypothesis (not yet confirmed)

Most likely a real Netty `EventLoopGroup` worker thread is blocked in a
native/blocking call (e.g. `epoll_wait`/`select`-equivalent with no timeout,
or a blocking socket read) during its shutdown sequence, and doesn't poll a
safepoint-check flag while blocked there — so the VM-wide STW request never
gets acknowledged by that one thread. This would be a real concurrency gap
in whatever safepoint/STW mechanism the interpreter uses for cross-thread
JIT takeover (`vm/src/runtime/interpreter.rs`), triggered here by something
during shutdown (finalization, a GC, or another JIT-adjacent event) rather
than by `EventLoopGroup` itself.

## Not yet root-caused / fixed

Whoever picks this up should:
1. Grep `vm/src/runtime/interpreter.rs` for "STW cross-thread" / "takeover"
   to find the safepoint-request/acknowledgment mechanism and understand
   what triggers a request during shutdown with `--nojit` active.
2. Identify which native/blocking call the stuck Netty `EventLoopGroup`
   worker thread is parked in (interpreter PC/entry tracing or a debugger
   attach while hung, matching the methodology used for the
   `JGroupsTransport.start()` investigation — see
   `docs/internal/fixed-suite-bugs/keycloak-model-jgroupstransport-start-never-invoked-FIXED.md`).
3. Determine whether the fix belongs in the safepoint mechanism (make it
   robust to a thread blocked in native I/O) or in whatever native socket/
   epoll wrapper that thread is blocked in (make it poll the safepoint flag
   periodically, e.g. via a bounded timeout).

## Repro

Same harness as the two docs above; add `--nojit` to the CratonVM args.
Currently: hangs indefinitely (observed 3+ minutes stuck at the same log
line with `rounds` still climbing) during
`io.netty.channel.EventLoopGroup` shutdown, right after the ENTIRE
Infinispan cache-manager lifecycle (start → bootstrap → use → stop)
otherwise completed cleanly. Needs to be killed by an external timeout; no
JUnit `KCRUNNER_RESULT` summary is produced since the process never reaches
`System.exit`.
