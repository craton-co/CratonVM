# GC: rs_cache-presence-triggered GC-STW-vs-reactor-shutdown timing race (reactor worker leak)

**Status:** 🔴 **OPEN** (root cause); **reliable workaround validated** (`CRATONVM_ROOTSNAP_CACHE=0`).
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
  ([gc-moving-interpreter-lost-tag-missed-root.md](gc-moving-interpreter-lost-tag-missed-root.md)) is
  INDEPENDENT: a leak occurred with **zero** corruption, and green runs occurred with corruption.

## Root cause (as far as localized)

The leak is **GC-frequency-driven** and **rs_cache-PRESENCE-triggered**:

| Config | Result (RestClientSingleHostIntegTests, -Xmx1g) |
|---|---|
| `-Xmx6g` (few young GCs) | **10/10 green, 0 leaks** |
| `-Xmx1g`, cache ON (default) | ~15–25 % leak |
| `-Xmx1g`, **`CRATONVM_ROOTSNAP_CACHE=0`** | **0 leaks, 22/22** |

So the rs_cache (the frozen-frame root-**snapshot** optimization in `update_root_snapshot`) is correct, but
its mere presence makes per-snapshot work cheaper and thereby **shifts thread/GC timing** enough to expose a
latent **GC-stop-the-world vs. reactor-shutdown coordination race** — a reactor worker that is spawned /
parked / terminating in a narrow window around an STW pause during `restClient.close()` ends up neither
making progress nor reaching `mark_dead`. Disabling the cache changes the interleaving so the window is not
hit. The actual race (which thread state + which STW edge) is **not yet pinned**; it is Heisenbug-prone (any
`eprintln` tracing changes the timing and hides it).

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

Root-cause the STW-vs-reactor-shutdown race with timing-neutral instrumentation (deterministic scheduling,
or recording thread states + STW barrier arrivals via lock-free ring buffers rather than `eprintln`).
Focus on the thread-lifecycle edges during `restClient.close()`: a worker still in `alive_count` but not
arriving at the STW barrier, vs. one that finished `run()` but hasn't reached `mark_dead`
(`vm_exec.rs` thread-exit closure).

## Related

- Prior (superseded) investigation passes: the former `reactor-worker-thread-leak-at-shutdown.md` (removed; see git history).
- The co-occurring benign corruption: [gc-moving-interpreter-lost-tag-missed-root.md](gc-moving-interpreter-lost-tag-missed-root.md).
- `Thread.getState()` fix (predecessor): commit `16d23e7b`.
