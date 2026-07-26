# GC: rs_cache-presence-triggered GC-STW-vs-reactor-shutdown timing race (reactor worker leak)

**Status:** ✅ **FIXED on dev, ARCHIVED to `..` 2026-07-07** (`323a3ba6`, 2026-07-01 —
"fix(gc): serialize thread exit against stw"; the "Fix candidate" section below is what landed, incl. the
`blocked_dead_transition_*` / `inflated_handle_survives_object_remap_*` unit tests, verified present on dev
2026-07-06). **Residual validation item (not a code gap):** this doc's own acceptance soak — ES
`RestClientSingleHostIntegTests` at `-Xmx1g` with the default rootsnap cache — was not rerun before
archiving (the ES test fixture is not present on the Linux probe host); rerun opportunistically next time an
ES-capable environment exists. The `CRATONVM_ROOTSNAP_CACHE=0` workaround is expected unnecessary post-fix.
Found/characterized 2026-06-20 (branch `fix/es-restclient-gc-safety`). Supersedes an earlier
investigation pass (the former `reactor-worker-thread-leak-at-shutdown.md`, now removed — see git history).
`--nojit` (moving young collector), GC-pressure-dependent.

## Symptom

`RestClientSingleHostIntegTests` at `-Xmx1g` intermittently (~15–25 %) ends with a SUITE-scope
`ThreadLeakError`: one Apache httpcore-nio I/O reactor worker
(`elasticsearch-rest-client-N-thread-M`, `state=RUNNABLE`) does not terminate on `restClient.close()`, so
randomizedtesting reports a zombie thread and fails the suite. (Captured stacks vary: sometimes an
"empty stack", sometimes busy in `DefaultNHttpClientConnection.produceOutput`; both are the same leaked
worker.)

## What it is NOT

- **Not a socket-write / OP_WRITE bug.** A real `std::net::TcpStream` write to a closed/reset peer returns
  an error (`sc_write` maps `ConnectionReset` → `SocketException`), so the reactor would close the session,
  not spin. The earlier "non-blocking write returns 0 forever" hypothesis is refuted.
- **Not an rs_cache *correctness* bug.** A freshness hardening (also keying frozen-frame reuse on the
  callee frame's `seq`) did NOT reduce the leak (~5/20 with the cache on) and is in fact **redundant** —
  a frame's `seq` is stable, so the existing seq-prefix-closure already implies the callee matched.
- **Not the separate teardown corruption.** The lost-tag "all-zero header" miss
  ([gc-moving-interpreter-lost-tag-missed-root.md](gc-moving-interpreter-lost-tag-missed-root.md),
  since FIXED `0abb64ba` and archived) is
  INDEPENDENT: a leak occurred with **zero** corruption, and green runs occurred with corruption.

## Root cause (pinned 2026-07-01)

The leak is **GC-frequency-driven** and **rs_cache-PRESENCE-triggered**:

| Config | Result (RestClientSingleHostIntegTests, -Xmx1g) |
|---|---|
| `-Xmx6g` (few young GCs) | **10/10 green, 0 leaks** |
| `-Xmx1g`, cache ON (default) | ~15–25 % leak |
| `-Xmx1g`, **`CRATONVM_ROOTSNAP_CACHE=0`** | **0 leaks, 22/22** |

So the rs_cache (the frozen-frame root-**snapshot** optimization in `update_root_snapshot`) is correct, but
its mere presence makes per-snapshot work cheaper and thereby **shifts thread/GC timing** enough to expose a
latent **GC-stop-the-world vs. reactor-shutdown coordination race**.

The pinned race is the thread-exit edge:

- `request_stw` previously accepted a precomputed `alive_count` sampled outside the GC barrier transition
  lock, then subtracted `threads_blocked` under the lock.
- A terminating thread entered a GC-blocked region, could be marked dead, and only later decremented
  `threads_blocked`.
- A GC initiator in that window could observe `alive_count` without the terminating thread but still subtract
  the terminating thread from `threads_blocked`, under-counting `expected` by one. That lets STW proceed
  while a live mutator/reactor worker has not arrived.
- The old thread-exit path also kept a raw `Thread` object address across the final blocked/mark-dead/notify
  sequence; a moving GC in that window could remap the object and make the final `Thread.join()` wakeup use a
  stale monitor-table lookup.

Disabling the cache changes the interleaving so this window is not hit; it does not fix the underlying STW
accounting race.

## Fix candidate

The 2026-07-01 fix candidate makes the transition atomic and removes the stale monitor lookup:

- `GcBarrier::request_stw_counted` computes `alive_count` while holding the barrier transition lock.
- `BlockedGuard::finish_after` and `mark_blocked_region_leave_after` run liveness changes while holding that
  same transition lock and decrement `threads_blocked` before the state becomes observable to the next STW.
- Java thread termination now acquires a stable inflated `Arc<Monitor>` for the `Thread` object before
  `mark_dead`, then uses that handle for the final `notifyAll`/`exit` after liveness teardown.
- Foreign-thread/AIO detach paths use the same atomic blocked-dead leave helper, so the fix is not limited to
  `java.lang.Thread` teardown.

Verification so far:

- `cargo test -p cratonvm-vm blocked_dead_transition --lib`
- `cargo test -p cratonvm-vm inflated_handle_survives_object_remap_for_thread_exit_notify --lib`
- `cargo test -p cratonvm-vm foreign_ --lib`

The ES `RestClientSingleHostIntegTests` soak has **not** been rerun yet, so this doc stays in
`../../known-issues` rather than moving to `..`.

## Reliable workaround (validated)

Run the ES RestClient suites with **`CRATONVM_ROOTSNAP_CACHE=0`** (plus the residual-2 GC fixes already on
the branch). Validated at `-Xmx1g`: single-host **22/22 green** (≥16 consecutive, 0 `ThreadLeakError`),
multi-host **4/4 green** (no regression).

```bash
CRATONVM_ROOTSNAP_CACHE=0 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
  cratonvm.exe --nojit -Xmx1g org.junit.runner.JUnitCore \
  org.elasticsearch.client.RestClientSingleHostIntegTests
```

**Do NOT flip the global default** (`rootsnap_cache()` defaults on): the cache is a real per-snapshot
optimization for deep-stack native-heavy app-gauntlet workloads (snapshots run on every object-returning
native call over deep stacks), so a global off risks regressing their throughput / timing-bound passes.
Keep it a suite-level env until the race itself is fixed.

## Next step

Rerun the ES RestClient soak at `-Xmx1g` with the default rootsnap cache enabled. If the single-host suite is
green across the prior failure envelope, move this doc to `..`.

## Related

- Prior (superseded) investigation passes: the former `reactor-worker-thread-leak-at-shutdown.md` (removed; see git history).
- The co-occurring benign corruption: [gc-moving-interpreter-lost-tag-missed-root.md](gc-moving-interpreter-lost-tag-missed-root.md) (FIXED `0abb64ba`, archived).
- `Thread.getState()` fix (predecessor): commit `16d23e7b`.
