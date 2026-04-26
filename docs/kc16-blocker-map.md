# KC16 Boot-Blocker Map (Session 93 / WP0.3, 2026-04-24)

Keycloak 16.1.1 (WildFly / JBoss-Modules) under Session 92/93 `rustjvm.exe`.
**WP0.3 now closed** (N1..N4 + WP0.3 verification):
  * All 9 known MISSING natives registered. `s10_native_coverage_100_percent`
    runs: `Total ACC_NATIVE=191, Covered=191, Missing=0, Coverage=100.0%`.
  * Anchored grep `MISSING: .+-> Native\.` (roadmap pattern, legacy format)
    returns 0 — the real emitter at `vm/src/vm/vm_object.rs:719` emits
    `MISSING: <class>.<method><desc>` and the inventory scan returns empty.
  * Blocker #4 below is therefore RESOLVED.

Workload: `jboss-modules.jar -jaxpmodule
javax.xml.jaxp-provider org.jboss.as.standalone -b 0.0.0.0`. JDK 21.0.6.
Reference: HotSpot `standalone\log\server.log` hit `WFLYSRV0025` in ~47 s.

## 1. WildFly phase timeline

`RUST_LOG=rustjvm_vm=trace` → `/tmp/kc16_d2_stderr.txt` (4.0 GB, 20.1 M lines, killed at 206 s).

| Phase | Event | Time |
|-------|-------|------|
| VM startup, 70 jmods, 267 classes | `NativeBridge Coverage 192/201`, 9 MISSING | 03:10:17.267 |
| JDK core clinits | `<clinit> java/util/Collections` | 03:10:17.380 |
| **`org.jboss.modules.Main.<clinit>`** | + `StartTimeHolder`, `Properties` | 03:10:17.383 |
| `ConcurrentHashMap.<clinit>` + `version.properties` | bytes=732 | 03:10:17.386 |
| StringUTF16 compress, Preconditions clinit | — | 03:10:17.444 |
| **LIVELOCK — `Ifeq(-41)` in `CHM.initTable()` CAS** | — | 03:10:17.449 |
| 206 s, 1,184,080 iterations (~5,740 /s) | — | kill |

**Phase reached: P0 Module-loader bootstrap.** Never parses `module.xml`,
never resolves `org.jboss.as.standalone`, never reaches MSC, subsystems,
Keycloak deployment, or HTTP listener.

## 2. Top 5 blockers

| # | Phase | Symptom | Root cause | Fix | Cmplx |
|---|-------|---------|------------|-----|-------|
| 1 | Mod-loader P0 | **CHM `initTable()` CAS livelock** on `sizeCtl`: PC 0..41 spin 5,740 /s. Identical bytecode to KC26 #1. | `Unsafe.compareAndSetInt` reads zero-alloc primitive slot as `Value::Object(None)` not `Int(0)`; CAS equality never holds. | Typed default at `alloc_object` or typed `read_slot`. | **Hard** |
| 2 | Mod-loader P0 | Never opens any `module.xml`. | Downstream of #1. | Gated on #1. | Hard |
| 3 | Bootstrap diff | **No `initPhase1` synthetic-stream fallback** (KC26 emits it). | Different `-jar` launch paths. | Align with KC26 after #1. | Medium |
| 4 | Natives | ~~9 static MISSING~~ **RESOLVED (WP0.3, Session 93)** — all 9 (`Class.getProtectionDomain0/getSigners/setSigners`, `Thread.sleep0`, 4× `AccessController.*`, `AtomicLong.VMSupportsCS8`) registered with real implementations (not stubs). Coverage now 191/191 = 100 %. | — | Done. | Done |
| 5 | Observability | Silent livelock: no WARN, no stuck-thread detector, `kill -9` loses audit. | No heartbeat, no SIGTERM flush. | `--XX:StuckThreadMs=N` + audit flush. | Medium |

## 3. New missing natives beyond the 9 known

**None.** 0 dynamic `UnsatisfiedLinkError`, 0 swallows in 4.0 GB trace —
only 10 startup-banner lines. Livelock precedes any further native dispatch.
Post-WP0.3: the original 9 banner lines are gone — the static scan now
reports 0 MISSING. Next time KC16 is re-run for a fresh census, any new
misses will be **dynamic** (reached only after the CHM livelock clears via
T19-wave-1).

## 4. Final state at 120 s (ran 206 s)

Process alive, 1 thread, ~5,740 ops/s. Interpreter looping PC 0..41 of
`CHM.initTable`. 26 classes fully clinit'd (vs KC26's 389 — KC16 trips
earlier, during `jboss-modules/Main`'s first CHM use, before JDK finishes
`j.u.concurrent`). 0 swallows, 0 exceptions, `server.log` not created,
stdout empty.

## 5. Shared-with-KC26 vs KC16-specific

| Blocker | KC26 | KC16 |
|---------|------|------|
| **CHM `initTable` CAS livelock (#1)** | YES | **YES — identical fix** |
| 9 static-scan MISSING natives | ~~YES~~ **RESOLVED** | ~~YES~~ **RESOLVED** (WP0.3) |
| `initPhase1` synthetic-stream fallback | YES | **NO — KC16-differential** |
| `[L…;` array synthesis (KC26 #3) | YES | unreachable |
| Silent livelock / no watchdog (#5) | YES | YES |
| WildFly MSC/Undertow/XNIO (T19.1/2/7) | N/A | deferred, gated on #1 |
| Quarkus ArC/static-init (T19.3/4) | YES | N/A |

**Both share the same primary blocker in the same JDK class.** Fixing #1
unblocks both; all version-specific work is behind that choke point.

## 6. Recommended next-session work

1. **Fix #1** per `docs/kc26-blocker-map.md` §5.2. Unblocks KC16 + KC26 together.
2. **#5** — `--XX:StuckThreadMs=N` + SIGTERM flush. Otherwise every future hang needs a 4 GB trace.
3. Re-run KC16 after #1 → expect P1 (`module.xml`) and fresh natives (JBoss-VFS, Log-Manager, Elytron).
4. **Defer T19.1 / T19.2 / T19.7** — unreachable today.
5. Re-run census; expect KC16 natives (JGroups, JBoss-Threads, Elytron) beyond the shared 9.

## Artefacts

* `/tmp/kc16_d2_stderr.txt` — 4.0 GB trace
* `/tmp/kc16_d2_stdout.txt` — 0 bytes
* `C:\craton\keycloak-16.1.1\standalone\log\server.log` — HotSpot reference
* `docs\kc26-blocker-map.md` — D1 peer
* `docs\kc-missing-natives-census.md` — static baseline

#1 gates T19.1-4; #5 is a new T19 observability sub-task. WP0.3 closed:
9 MISSING natives all registered — no residual T10 work on this list.
