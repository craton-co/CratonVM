# KC26 Boot-Blocker Map (Session 93 / WP0.3, 2026-04-24)

Diagnostic map of what happens when Keycloak 26.2.4 (`quarkus-run.jar`) starts
under `target/release/cratonvm.exe` built from T18 + Session 92/93 fixes.

**WP0.3 now closed** (Session 93 verification):
  * All 9 known MISSING natives registered with real implementations.
    `s10_native_coverage_100_percent` reports `191/191 Covered, 0 Missing,
    100.0%`. Anchored grep `MISSING: .+-> Native\.` and
    `MISSING: .+\.(<anything>)` both return 0.
  * Blocker #4 below is therefore RESOLVED.

Binary: `C:\craton\cratonvm\target/release/cratonvm.exe` (T18)
Workload: `C:\craton\keycloak-26.2.4\lib\quarkus-run.jar`
JDK: `C:\craton\jdk-21.0.6`

## 1. Phase-by-phase timeline

All timestamps from `RUST_LOG=cratonvm_vm=trace` run (`/tmp/kc26_d14_stderr.txt`,
1,836,353 lines). Session 91 K1 fix is confirmed holding — no `lcmp` tag crash.

| Phase                                       | First line | Last successful event                                                      | Duration |
|---------------------------------------------|------------|-----------------------------------------------------------------------------|----------|
| **VM startup / real-JDK boot**              | 02:58:52   | `Auto-discovered 70 boot classpath entries (70 jmods)`                     | ~50 ms   |
| **Native registry**                         | 02:58:52   | `Coverage: 192/201 (95%) — 192 covered, 9 missing`                          | <10 ms   |
| **System.initPhase1() attempt**             | 02:58:52   | `System.initPhase1() fell back to synthetic streams (j/l/IllegalStateExc)` | ~600 ms  |
| **Bootstrap resume (post-fallback)**        | 02:58:53   | `<clinit> java/util/concurrent/atomic/AtomicInteger` (class 389 of 390)    | ~1.6 s   |
| **Quarkus entry enters**                    | 02:58:54   | Load `io/quarkus/bootstrap/runner/{QuarkusEntryPoint,Timing,SerializedApplication,ClassLoadingResource,JarResource}` | <50 ms |
| **`Timing.staticInitStarted` runs clean**   | 02:58:54   | `running <clinit> class=io/quarkus/bootstrap/runner/Timing` → OK           | <1 ms    |
| **`ConcurrentHashMap.initTable()` enters**  | 02:58:54.875 (line 26511) | `Ifeq(-41)` jump to PC 0                                   | livelock |
| **Rest of 120-second window**               | 02:58:54.876 → 02:59:12.65 (kill) | 106,462 loop iterations (5.3 k/s)                 | ~18 s    |

**Final state at kill:** process is alive, single-threaded, interpreted loop at
PC 0–41 of `java/util/concurrent/ConcurrentHashMap.initTable()`. No panic, no
exception, no B6 swallow, no stderr output beyond the loop trace.

## 2. Top 5 blockers

| # | Phase              | Symptom                                                                 | Likely root cause                                                                                                                                                                                                       | Fix scope                                                          | Complexity |
|---|--------------------|--------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|--------------------------------------------------------------------|------------|
| 1 | Quarkus core       | **Infinite livelock** in `ConcurrentHashMap.initTable()` CAS retry on `sizeCtl`.  Bytecode: `Getfield sizeCtl → Ifge → Unsafe.compareAndSetInt(this, SIZECTL, oldVal, -1) → Ifeq(-41)`. CAS returns 0 forever (~5 k/s). | `Unsafe.compareAndSetInt` → `compare_and_swap_field` compares `values_equal_for_cas(current, expected)` with `_ => false` fallback. `gc::heap::read_slot` is `std::ptr::read(ptr as *const Value)` on zero-initialized bytes (from `alloc_zeroed`), so freshly-allocated *primitive* instance fields come back as the zero-discriminant `Value` variant (likely `Value::Object(None)`) rather than `Value::Int(0)`.  Bytecode `Getfield` coerces to `Int(0)`, but raw CAS reads `Object(None)` → `values_equal_for_cas(Object(None), Int(0)) = false` forever. Same symptom would hit a quick `ConcurrentHashMap::put` unit test. | Ensure primitive instance fields round-trip as `Value::Int(0)` after zero allocation — either tag slots at object-allocation time, or type-aware `read_slot`. | **Hard**   |
| 2 | Bootstrap          | `System.initPhase1()` throws `java/lang/IllegalStateException`, falls back to synthetic streams.        | T14 known-issue: initPhase1 touches Unsafe accessors against partially initialized reference slots.  Not blocking KC26 *today* (fallback path restores streams), but masks the real reason subsystems later misbehave — and the cache-clear after fallback discards resolution data that may be valid. | Investigate which class/method throws ISE in initPhase1; fix the underlying Unsafe slot. | Medium     |
| 3 | Bootstrap          | ~7 JDK inner-classes fall back to **synthetic stub** because array-type name `[L...;` not in classpath. Examples: `[Ljava/util/concurrent/ConcurrentHashMap$Segment;`, `[Ljava/util/concurrent/ConcurrentHashMap$Node;` | Array classes are being looked up by JMOD class path scan instead of being synthesized from the element type on demand.       | Classloading: resolve `[Lx;` by taking the concrete `x` and building the array class without a classpath round-trip. | Easy       |
| 4 | Missing natives    | ~~9 known unregistered natives~~ **RESOLVED (WP0.3, Session 93).** All 9 (`Class.getProtectionDomain0/getSigners/setSigners`, `Thread.sleep0`, 4× `AccessController.*`, `AtomicLong.VMSupportsCS8`) registered with real implementations (not stubs). Session 92's T19 Wave-1 agents (N1..N4) landed the native bodies; WP0.3 verified each registration survives the bootstrap scan — coverage now 191/191 = 100%. | Done.                                                                | —                                                                   | Done       |
| 5 | Monitoring gap     | VM **silently** livelocks: no `WARN`, no CPU telemetry, no stuck-thread detector at any log level.  `timeout(1)`-induced SIGTERM loses the `--dump-missing-natives` audit (only written on clean shutdown). | No stuck-thread watchdog; audit dump runs only in the happy path after `vm.invoke(main)`.                                                                                                                                | Add `--XX:StuckThreadMs=` trigger that thread-dumps every loop whose PC has repeated > N iterations. Write audit/dump on SIGTERM before exit. | Medium     |

## 3. New missing natives discovered beyond the 9 known

**None.** The original 9 MISSING entries at startup have been **closed**
(WP0.3, Session 93). The static-scan warning list is now empty. Any future
MISSING discoveries on KC26 will only surface **dynamically**, past the CHM
livelock — the boot is still gated on Blocker #1 (CHM initTable CAS livelock,
fix in progress via T19 Wave 1 `values_equal_for_cas` + typed default-slot
work landed Session 92).

## 4. VM final state at 120 s

* **Process:** alive, 1 OS thread, consuming modest CPU (~5,300 bytecode ops/s
  — nowhere near pegged).
* **JVM thread state:** `main` thread, interpreted, stack frame =
  `java/util/concurrent/ConcurrentHashMap.initTable()V`, PC cycling 0 → 41.
* **Swallow counter:** 0.
* **Exceptions thrown:** 0.
* **Classes loaded:** 389 (last = `java/util/concurrent/atomic/AtomicInteger`,
  id 389).
* **Quarkus progress:** `QuarkusEntryPoint.main` entered, `Timing.staticInitStarted(boolean)`
  executed cleanly past the PC 10 `lcmp` (K1 fix holds), `SerializedApplication`
  and `ClassLoadingResource` clinits completed; first `ConcurrentHashMap.put(...)`
  call (probably inside `SerializedApplication.run` class-loader init map)
  trips the livelock.

Contradicts Session 87/91 claim that "KC26 alive at 60s" implied forward
progress — that was measuring process liveness, not actual execution. KC26 has
been **stuck at the same PC since the second the VM fixed the K1 Timing crash**.

## 5. Recommended next-session work package

1. **Reproduce deterministically** with a `cargo test` that instantiates a real
   JDK `ConcurrentHashMap`, calls `put("a", "b")`, and asserts it returns in
   < 100 iterations.  This will fail the same way outside Keycloak.
2. **Fix #1 (hard blocker)** — CAS equality on freshly-allocated primitive fields. Three sub-tasks:
   * **1a**: unit test: allocate a `ConcurrentHashMap`, read `sizeCtl` via
     `get_field_volatile` without ever writing it, and assert it is `Value::Int(0)`
     (currently returns `Value::Object(None)` because `read_slot` uses
     `ptr::read::<Value>` over zero bytes).
   * **1b**: fix by either (i) having `alloc_object` tag each slot with the
     declared field kind's default value (writes `Value::Int(0)` / `Value::Long(0)`
     etc. into slots at init), or (ii) making `read_slot` carry the declared
     field type so it interprets zero bytes as the correct discriminant.
   * **1c**: verify `Unsafe.objectFieldOffset1(Class, fieldName)` and
     `Getfield(cpIndex)` produce the *same* slot index — if (1b) is fixed and
     this invariant holds, the CHM livelock clears.
3. **Quick win (Blocker #3)** — synthesize `[L…;` array classes on demand in
   `classloading::class_manager` instead of scanning JMODs. ~30 min fix.
4. **Observability win (Blocker #5)** — add a `StuckThreadMs` guard: when a
   bytecode loop hits the same `(method_id, pc)` > 10 k times, emit a WARN
   with the Java stack trace. This is the diagnostic we needed *today* and
   didn't have.
5. **Defer Blocker #2** until after KC26 boots past CHM: initPhase1 failure is
   tech-debt but not load-bearing while the synthetic-stream fallback works.

## Artefact paths

* Full 120-s debug log (TRACE): `/tmp/kc26_d14_stderr.txt`
* Info-level (phase markers only): `/tmp/kc26_d7_stderr.txt`
* Class-load trace without interpreter noise: `/tmp/kc26_d11_stderr.txt`
* Tooling reference: `--XX:AuditMissingNatives --dump-missing-natives PATH`
  only flushes on **clean shutdown** — useless for hang diagnosis today.
