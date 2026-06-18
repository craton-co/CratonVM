# Known-issues triage — complete remaining-gaps table (2026-06-18)

Re-triaged against `dev` @ `8879f1d2` after the large batch of fixes/features that landed.
Binary-verified items used a freshly built `dev` binary (`cvtri.exe`). **7 docs whose error is
confirmed fixed were moved to `docs/internal/`** (see *Relocated* at the bottom).

Legend — **Category**:
- **ACTIONABLE** — localized fix is clear; can be attempted directly.
- **INSTRUMENTATION** — root-caused or reproducible, but pinning the fix needs deep tracing / a debug build / suite re-run.
- **FEATURE** — needs a missing VM subsystem or capability, not a point fix.
- **INDEX** — not a bug (handoff / redirect / summary).

## Remaining open gaps

| # | Doc | Gap (1-line) | Status on current `dev` | Category | Notes / what it needs |
|---|-----|--------------|--------------------------|----------|-----------------------|
| 1 | `reflrepro-register-resident-jit-root-handoff` | **A2** — reflection oop live only in `rax`/native-return at a young-GC safepoint → reclaimed → UAF SIGSEGV | 🔴 OPEN — **verified rc=139** (`ReflRepro 8000`, `GC_STRESS=65536`) | INSTRUMENTATION→FEATURE | precise-maps (default-on) does **not** cover it; needs spill-every-live-oop-reg before GC-capable calls **+** non-moving-sweep walker hardening |
| 2 | `fork6-fjp-multithread-jit-root-reclamation` | **A4** — worker-thread `ForkJoinTask`s reclaimed under FJP workers (gated `CRATONVM_REAL_FORKJOINPOOL`) | 🟡 OPEN/partial — lost-tag manifestation mitigated; ~15% worker-subtask residual; a real-FJP CAS failure masks it | INSTRUMENTATION | needs worker-thread full-native-stack scan / per-thread SP capture at STW |
| 3 | `jit-regalloc-callee-saved-clobber-family` | **Family J** umbrella — callee-saved-reg clobber / allocate-then-putfield miscompile (the ~30 `is_known_miscompile` bans) | 🔴 OPEN (general) — managed by per-method bans; many individual members lifted | FEATURE | general regalloc/calling-convention fix deferred (Stage B/C); **underlies kafka-bug-C `dup_x1`, keycloak-credentialmodel, Groovy** |
| 4 | `spring-bug-04-junit-timeout-interceptor-double-proceed` | JUnit `@Timeout` interceptor `proceed()` invoked twice (cross-thread / MethodHandle re-entry) | 🔴 OPEN — no fix commit found | ACTIONABLE | verify body runs on both timeout-worker and caller; threading/MH-invoke dispatch |
| 5 | `spring-bug-06-mergedannotations-hang` | `MergedAnnotationsTests` hangs in JUnit discovery (annotation-proxy identity → cycle-detect loop) | 🟡 OPEN — gated on `spring-bug-01` (annotation-proxy `equals`/`hashCode`, partly landed) | INSTRUMENTATION | re-run after bug-01; confirm `findAnnotation` `visited` set terminates |
| 6 | `spring-bug-08-serializable-proxy-roundtrip` | Serializable JDK-proxy round-trip fails on deserialize | 🔴 OPEN — **verified**: `UnsatisfiedLinkError: Module.defineModule0` (proxy-real-classfile inc.3 did **not** fix it) | FEATURE | needs JPMS natives (`Module.defineModule0`/`addReads0`/`addExportsToAll0`/`addOpens0` + `Proxy.defineClass0`) **or** synthetic-proxy-aware OIS proxy deser |
| 7 | `spring-bug-10-junit-platform-execution-loaderr` | Family-A GC root-undercount race → LOADERR/CCE in heavy multithreaded JUnit batch | 🟡 partial — single-thread/register-invisibility closed by precise-maps default-on; heavy-batch/cross-thread residual unconfirmed | INSTRUMENTATION | suite re-run on current `dev`; the cross-thread/moving-remap residual is the open part |
| 8 | `springsuite-bug-04-string-constant-corrupted-under-load` | Interned String constant → bare `Object` under batch load (Family-A manifestation) | 🟡 partial — same race as #7 | INSTRUMENTATION | re-run batched suite on current `dev` to confirm precise-maps coverage |
| 9 | `spring-bug-11-groovy-and-scheduler-crashes` | Groovy crash cluster | 🟡 crash **FIXED** (HashMap-layout `87091dec`); **residual: Groovy hangs at BEGIN** | INSTRUMENTATION | residual is a Groovy compile/indy/MethodHandle hang (JIT + `--nojit`); needs trace |
| 10 | `bug06-fam5-reflection-getdeclaredmethod-null` | Reflection native returns `null` vs a `Class`/`Method` (×28) | 🔴 OPEN — common paths clean (`Refl5`==HotSpot); narrow generic/proxy path unattributed | INSTRUMENTATION→ACTIONABLE | per-test attribution to capture the exact receiver type, then a localized native fix |
| 11 | `bug06-fam6-annotation-synthesis-mergedannotation` | Spring `@AliasFor`/`MirrorSets` synthesis mismatches **+ a ~2 GB OOM** in `AnnotationUtilsTests` | 🔴 OPEN — raw annotation reading conformant; synthesis layer + bounded-wrong-size alloc | ACTIONABLE | OOM first (small `-Xmx`, find the wrong length/count read) — likely localized |
| 12 | `hibernate-jta-narayana-xa-completion-and-socket-loopback` | JTA: Layer-1 Narayana XA completion never drives `XAResourceWrapper.commit()`; Layer-2 synthetic socket loopback | 🟡 Layer-0 **FIXED**; Layers 1/2 OPEN | FEATURE | deep Narayana XA-completion + accept↔connect loopback; live repro blocked under peer CPU contention |
| 13 | `hibernate-jaxb-classloading-bytebuddy-bootstrap-slow` | XML/JAXB + ByteBuddy `MethodGraph` bootstrap times out (rc=124) | 🔴 OPEN — class-loading/interpreter throughput, not a localized loop | INSTRUMENTATION | infinite-vs-slow on `MethodGraph.doAnalyze`; throughput levers (see new JIT features) |
| 14 | `hibernate-deserialization-sessionfactory-reconnect-null` | Deserialized `EntityManager`/`SessionFactory` is null (`SessionFactoryRegistry.findSessionFactory` returns null) | 🔴 OPEN — generic deser works; Hibernate UUID/name registry reconnect gap | INSTRUMENTATION→ACTIONABLE | trace registry population vs lookup key; likely localized |
| 15 | `kafka-bug-B-mockstatic-capturing-lambda-jit` | After class redefine, a capturing-lambda call dispatches a stale JIT target (NPE); `--nojit` OK | 🔴 OPEN — dispatch half fixed; this JIT residual remains | INSTRUMENTATION | caller-side inline-cache/bound-call invalidation on redefine |
| 16 | `keycloak-credentialmodel-jit-lazy-init-field-null` | JIT escape-analysis elides `new→putfield` lazy-init; getter returns null; `--nojit` OK | 🔴 OPEN — context-sensitive (bare repro doesn't trip) | INSTRUMENTATION | JIT disasm of the lazy-init getter; escape-analysis `new`-to-field-then-return |
| 17 | `reactor-worker-thread-leak-at-shutdown` | Intermittent httpcore-nio worker stuck in `Selector.select()` after shutdown | 🟡 OPEN (intermittent) — `Thread.getState()` reporting fixed (`16d23e7b`); selector wakeup race open | INSTRUMENTATION | selector-wakeup-at-shutdown audit; low priority |
| 18 | `keycloak-16-stream-onclose-and-laziness` | (A) `Stream.onClose`/`close` are no-ops; (B) intermediate ops eager-materialize (break short-circuit) | 🔴 OPEN | A: ACTIONABLE / B: FEATURE | A is a self-contained native fix; B is the lazy-stream-pipeline rework |
| 19 | `keycloak-15-windows-path-root-parsing` | `Path.getRoot()` null for Windows drive paths; `getNameCount()` counts `C:` as a name | 🔴 OPEN (deferred — pervasive) | FEATURE | explicit Windows drive/UNC prefix parsing in the nio Path natives |
| 20 | `ES-HANG-02-residuals-handoff` / `ES-HANG-02-restclient-integ-http-server` | RestClient integ tests | ✅ **HANG FIXED** (`a4dc1402`/`f720f071`); residual-1 connect FIXED (`1cf9c965`); **residual-2 throughput** open | INSTRUMENTATION | residual-2 = per-request overhead / connection-churn; **HTTP keep-alive** in the synthetic server is the lever |
| 21 | `bc-jit-ban-investigation` | BouncyCastle blanket JIT ban; 34–64× slow | 🟡 6 latent codegen defects **FIXED**; ban kept as policy; interp↔JIT boundary cost ~4.7× | ACTIONABLE (perf) | reduce boundary/transition cost; not correctness |
| 22 | `springrepos-extension-hang-jit-throughput-and-deep-recursion` | (Test passes on dev.) Latent: deep JIT→JIT recursion overruns native stack | ⚪ LATENT — only with an unmerged cold-path experiment | FEATURE | stack-banging + a stack-overflow-aware fault handler (design in doc) |
| — | `hibernate-hang-clusters-summary`, `hibernate-jta-txcontrol-…` (redirect), `roadmap-fanout-handoff`, `README` | summaries / index / handoff | n/a | INDEX | not bugs |

## New JIT/GC features that landed — and where they help

| Feature (commit / gate) | What it does | Relevant remaining bugs |
|---|---|---|
| **Precise JIT oop maps, default-on** (`32649b56`, `CRATONVM_PRECISE_JIT_MAPS`) | precise root scan across every active JIT frame (RBP-chain + per-safepoint maps) | **Closed A3 (SB-CRASH-04)**; partial A2/A4; should reduce/close the *single-thread* part of #7/#8 — **re-run the Spring batch to confirm** |
| **Full loop unrolling** (`6cb169ce`, `CRATONVM_JIT_UNROLL`, default-off) + **IR optimizer** (LICM/DSE) | compile-side throughput | throughput-bound: #13 (JAXB/ByteBuddy), #20 res-2 (ES throughput), BC (#21) |
| **Tiered manager / bg-compile** (`CRATONVM_BG_COMPILE`, default-off, GC-STW-safe) | background compilation | throughput-bound items above |
| **Real AQS default-on** (`42ab4f11`) | real `j.u.c` lock/condition semantics | concurrency-correctness adjacent (#17 reactor, #2 FJP) |
| **PC-annotated JIT disasm** (`CRATONVM_DBG_JIT_DISASM`, local→reg map + bc@PC) | pin codegen miscompiles | #16 (escape-analysis), #3 family members, #15 |

## Relocated to `docs/internal/` (error confirmed fixed on `dev`)

- `SB-SUITE-CRASH-04-jit-inline-new-heap-corruption.md` — A3, precise-maps default-on (`32649b56`)
- `jit-junit-discovery-reflection-corruption.md` — A1, mirror-array pinning (`fda29dcf`)
- `hibernate-jaxb-classload-synthetic-stub-rescan-storm.md` — HIB-DEV-03 memoized absent
- `hibernate-json-function-sigsegv-al_state-foreign-receiver.md` — `al_state` layout guard
- `hibernate-throwable-stacktrace-order-reversed-FIXED.md` — `getStackTrace()` order
- `kafka-bug-C-weakhashmap-stream-infinite-hang.md` — hang fixed (`1cd0ab26`); **verified** `stream().count()=5` (underlying `dup_x1` defect tracked in `jit-regalloc-callee-saved-clobber-family`)
- `kafka-bug-B-mockito-mockstatic-mock-dispatch.md` — dispatch fixed (JIT residual tracked in `kafka-bug-B-mockstatic-capturing-lambda-jit`)
