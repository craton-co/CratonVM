# Known issues — index & bug map

This folder collects CratonVM-only defects found while running upstream Java
suites. The docs had grown to describe the **same underlying bug from several
angles**; this index is the consolidated map. Read it first.

## How many distinct bugs are here?

After consolidation (full re-count 2026-06-18), the ~30 docs map to **one root-cause family
+ ~15 distinct standalone bugs**, of which **6 are already FIXED on `dev`**. Headline:

**~12 distinct OPEN defects + 1 latent**, grouped as:

1. **Family A — GC root coverage under JIT** (one root cause, several manifestations). Open members:
   **A2** (register-only/native-return reclaim + non-moving-sweep walk) and **A4** (Fork6 FJP
   multi-thread, gated). **A1/A3 are FIXED.** `springsuite-bug-04` and `spring-bug-10` are Family-A
   manifestations seen from the Spring suite (same root cause, different entry points), and the
   suite-scale field evidence in `jit-junit-discovery-reflection-corruption.md` is the same race.
2. **Standalone B** — JUnit `@Timeout` interceptor double-`proceed()` (open).
3. **Standalone C** — deep JIT→JIT recursion native-stack overflow (latent; only with an unmerged experiment).
4. **bug-06 F5** — reflection native returns null vs a `Class`/`Method` (open, unattributed).
5. **bug-06 F6 / `spring-bug-06` / `spring-bug-01`** — annotation **synthesis** value-mismatches.
   (The `MergedAnnotations` **hang** + the `AnnotationUtilsTests` "~2 GB OOM" in this cluster were the
   `toArray` self-recursion and are **FIXED** 2026-06-20, commit `8795b88d`; `MergedAnnotationsTests`
   now runs 174/178, `AnnotationUtilsTests` 72/72. Only the synthesis value-mismatches remain open.)
6. **`spring-bug-08`** — serializable JDK-proxy round-trip (open).
7. **`spring-bug-11` residual** — Groovy hang at `BEGIN` (the SIGSEGV half is FIXED via bug-12; open).
8. **`kafka-bug-B`** — Mockito `mockStatic` + `mock`/`mockConstruction` dispatch (open, deep).
9. **`kafka-bug-C`** — `WeakHashMap.values().stream()` infinite hang (open; pure-JDK 3-line repro).
10. **Hibernate JTA** (Narayana) — ✅ **RESOLVED 2026-06-20** (docs → [`docs/internal/`](../internal/)).
    L0 fixed on dev; **L1 was never broken (refuted)**; **L2 = an `accept()` deadlock** (global `s2_registry`
    lock held across blocking `accept()`) + `getLocalPort()==0`, both fixed on branch `fix/hib-jta-xa-loopback`
    (`e0426050`). A full Hibernate 8.1 + Narayana 7.3.4 + H2 begin/persist/commit/truncate cycle now passes in
    default (synthetic-socket) mode.
11. **Hibernate JAXB/ByteBuddy bootstrap slow** — ✅ **RESOLVED / does-not-reproduce 2026-06-20**
    (doc → [`docs/internal/`](../internal/)). Class-load storm fixed on dev; ByteBuddy `MethodGraph`
    JoinedSubclass bootstrap completes ~16s `--nojit`.
12. **Hibernate deserialized-`SessionFactory`-null** (`SessionFactoryRegistry` reconnect; open).
13. **ES-HANG-02 — ✅ RESOLVED 2026-06-20** (`fix/es-restclient-gc-safety`; docs moved to
    [`docs/internal/elasticsearch-suite/`](../internal/elasticsearch-suite/)). The RestClient
    embedded-HTTP-server hang AND both residuals are fixed: residual 1 (real non-blocking connect) +
    residual 2 — which was **NOT throughput** (the prior handoff's theory) but **three GC-correctness bugs**:
    a safepoint-resume monitor/native-root remap gap (`IllegalMonitorStateException`), an unrooted synthetic
    `com.sun.net.httpserver` handler ref (`NoSuchMethodError java/lang/Object.handle` storm), and
    per-request native read-alloc-use staleness. Both ES RestClient suites are green at `-Xmx1g` with
    `CRATONVM_ROOTSNAP_CACHE=0` (single-host 22/22, multi-host 4/4) — see the two new GC defects #15/#16.
    Former siblings also resolved: **ES-HANG-01** (`1cd0ab26`), **ES-FAIL-03**, **ES-FAIL-04**.
14. **Spring-suite sweep (2026-06-19 → re-verified 2026-06-20).** Status after running the suite
    through the JUnit-Platform `KRun` harness on a fresh dev build (`cratonvm-spring0620`, off
    `697134f8`):
    - ✅ **toArray-recursion FIXED** (commit `8795b88d`, → dev) — `ReferencePipeline.toArray(IntFunction)`
      was shadowed by a native that re-entered no-arg `toArray()` → `StackOverflowError`. This was the
      open **`MergedAnnotations` hang** AND **bug-06 fam6 "~2 GB OOM"**, and it blocked the *entire*
      JUnit launcher (no test class could run). Now: `MergedAnnotationsTests` 174/178,
      `AnnotationUtilsTests` 72/72, `AnnotatedElementUtilsTests` 82/82. Doc:
      [`docs/internal/fixed-suite-bugs/springsuite-0620-toarray-referencepipeline-recursion.md`](../internal/fixed-suite-bugs/springsuite-0620-toarray-referencepipeline-recursion.md).
    - ✅ **Unsafe off-heap DirectBuffer (bug-A + bug-A2) FULLY RESOLVED** — `PooledDataBufferTests`
      **10/10**, `LeakAwareDataBufferFactoryTests` 2/2. The bug-A2 Netty `refCnt` AIOOBE no longer
      reproduces. Archived → [`docs/internal/fixed-suite-bugs/springsuite-0619-unsafe-offheap-directbuffer.md`](../internal/fixed-suite-bugs/springsuite-0619-unsafe-offheap-directbuffer.md).
    - 🟡 **getBeanClassName bean-filter (bug-B) PARTIAL** — primary filter fix holds (no "hiding bean"
      warning), but **bug-B2 OPEN**: `LookupMethodTests` 0/7 / `LookupAnnotationTests` 0/10 still fail
      `Target object must not be null` (CGLIB **method-injection** enhancer makes a null instance).
      Kept in this folder: [getBeanClassName bean-filter / bug-B2](springsuite-0619-getbeanclassname-bean-filter.md).
    - Still untriaged from the sweep: `ReactiveAdapterRegistry$MutinyRegistrar` NCDFE, XML
      "Unexpected failure during bean definition parsing", "Unnamed bean definition", spring-jdbc
      mass-TIMEOUT, scheduler `StringIndexOutOfBounds`, and `DataBufferUtilsTests` TIMEOUT
      (heavy-reactive). (The prior `springsuite-0619-open-candidates.md` link was already dangling.)
15. **[GC: moving-collector lost-tag missed root](gc-moving-interpreter-lost-tag-missed-root.md)** — 🔴 **OPEN**
    (benign in practice). Under `-Xmx1g` GC pressure the **moving** young collector zeroes a live object
    whose only reference is a frame slot tagged non-`Object` at the marking snapshot ("all-zero header" /
    `Stale pointer … StringBuilder.flush`). Localized **deterministically** to `RandomizedRunner.invoke
    local[3]` by a new gated `CRATONVM_GC_VERIFY_STALE` per-parked-thread verifier. Same *class* as the
    Family-A "lost-tag interpreter local" (A4) but on the `--nojit` moving path.
16. **[GC: rs_cache-presence reactor-shutdown timing race](gc-rscache-reactor-shutdown-timing-race.md)** —
    🔴 **OPEN** (workaround validated). The ES RestClient reactor-worker `ThreadLeakError` at
    `restClient.close()`: GC-frequency-driven and `rs_cache`-PRESENCE-triggered (a latent
    GC-STW-vs-reactor-shutdown race exposed by snapshot timing), **NOT** a socket/OP_WRITE bug and **NOT** an
    rs_cache correctness bug. Reliably avoided by `CRATONVM_ROOTSNAP_CACHE=0` (suite-level — do NOT flip the
    global default). Supersedes [reactor-worker-thread-leak-at-shutdown.md](reactor-worker-thread-leak-at-shutdown.md).

FIXED docs kept for the trail: A1 (`jit-junit-discovery-…`), A3 (`SB-SUITE-CRASH-04`), the Hibernate
JAXB class-load rescan storm (HIB-DEV-03), the JSON-function `al_state` SIGSEGV, the reversed
stack-trace order, and the springrepos hang (3 fixes landed; `dev` passes the test). 2026-06-20:
the `ReferencePipeline.toArray(IntFunction)` recursion and the off-heap DirectBuffer (bug-A/A2) docs
moved to [`docs/internal/fixed-suite-bugs/`](../internal/fixed-suite-bugs/).

### Consolidations applied (2026-06-18)
- The two Hibernate-JTA docs (`hibernate-jta-narayana-…` + `hibernate-jta-txcontrol-getinetaddress-per-class-report`)
  described the **same** Narayana cluster → merged into `hibernate-jta-narayana-xa-completion-and-socket-loopback.md`;
  the per-class file is now a redirect stub.
- (2026-06-17) The two Fork6 precise-maps files were already merged into `fork6-fjp-multithread-jit-root-reclamation.md`.

There is also a **second umbrella family** distinct from the GC-root one:
[**JIT regalloc callee-saved-register clobber**](jit-regalloc-callee-saved-clobber-family.md)
— the single root cause behind the ~30+ targeted JIT method bans in
`vm/src/jit/skip_list.rs` (HashMap/WeakHashMap/`Integer.valueOf`/`String.toLowerCase`/j.u.c./
BouncyCastle/Spring-boot/ByteBuddy/kafka-bug-C). Manifests as either `rc=139` corruption or
`rc=124` hangs, always JIT-only. Individual members are lifted as fixed; the general fix is the
deferred precise-JIT-maps / regalloc project. **Do not conflate it with Family A** — that one is
about root-*scanning* completeness, this one about register *clobber*.

---

## Family A — GC root coverage under JIT  *(one root cause, four manifestations)*

**Root cause (shared):** when a JIT frame is active, the young collection is the
**non-moving sweep with selective-promotion evacuation** (`gc_quiescence`; the
moving Cheney collector only runs with no JIT frame). That sweep is only correct
if the **root set is complete**. CratonVM's JIT-frame root scan is *conservative*
(it reads stack memory only) and has historically had gaps; a live young object
whose only reference sits in a gap is not marked (and not added to the
selective-promotion pin set) → it is swept-zeroed or evacuated-and-zeroed → its
stale reference later reads an all-zero / garbage header → `inconsistent header`,
`Stale pointer … all-zero header`, CCE, NPE, or SIGSEGV. `--nojit` always passes
(interpreter frames are precisely scanned and the moving collector remaps every
root); `-Xmx8g` passes (no young GC).

The eventual correct fix for the whole family is **precise JIT stack roots**
(know exactly which registers/slots hold oops at each safepoint), tracked under
`project_precise_jit_stack_maps`. The `CRATONVM_SHADOW_STACK` mechanism is the
current (incomplete/buggy) implementation of that.

| # | Manifestation | Repro | Status | Doc |
|---|---|---|---|---|
| **A1** | Reflection mirror-array builders held an `ObjectRef` array in a Rust local across allocating calls (`Field[]`/`Method[]`/annotation arrays) | `wildfly-suite/repro/MinRepro` | ✅ **FIXED on dev** (`pin_native_root` sweep) | [jit-junit-discovery-reflection-corruption.md](jit-junit-discovery-reflection-corruption.md) |
| **A2** | **`implausible object size` young-sweep-walker crash** (reflection/String-array allocation churn) — a *distinct* bug, NOT the register root | `wildfly-suite/repro/ReflRepro` | 🔴 **OPEN** — precise maps do **not** fix it (still crashes; verified 2026-06-17) | [reflrepro-register-resident-jit-root-handoff.md](reflrepro-register-resident-jit-root-handoff.md) |
| **A3** | **Register-invisibility** — a live oop sits only in a CPU register at a young-GC safepoint, invisible to the stack-only scan (single thread) | `apps/spring-boot/buildSrc/runner/MinRegexProbe` | ✅ **FIXED on dev** (`32649b56`, precise maps default-on) | [SB-SUITE-CRASH-04-jit-inline-new-heap-corruption.md](SB-SUITE-CRASH-04-jit-inline-new-heap-corruption.md) |
| **A4** | Multi-thread: live `ForkJoinTask`s reclaimed under **FJP worker threads** + a **lost-tag** interpreter local | `scratch/xworker/Fork6` (needs `CRATONVM_REAL_FORKJOINPOOL=1`) | 🟡 **OPEN / inconclusive** — a separate real-FJP CAS failure now masks the reclaim test (same with/without precise) | [fork6-fjp-multithread-jit-root-reclamation.md](fork6-fjp-multithread-jit-root-reclamation.md) |

> ## ✅ FIX (2026-06-17, dev `32649b56`): **precise JIT oop maps, default-on** — closes the register-invisibility root-scan gap (A3)
> `CRATONVM_PRECISE_JIT_MAPS` is now **default-on** (opt out: `CRATONVM_NO_PRECISE_JIT_MAPS`).
> It makes the non-moving sweep's root scan **precise across every active JIT frame**
> (RBP-chain `frame_record` + per-safepoint oop maps + a conservative per-frame
> fallback), so a live oop in a callee-saved register of a **caller** frame — the
> register-invisible root the conservative deepest-band-only scan missed — is found.
> This fixes the **register-invisibility class** (A3 and the kafka bug-21/22 /
> tomcat-style register-invisibility reclaims). **Verified:** A3 (`MinRegexProbe`)
> green at `GC_STRESS` 524288 **and** 4 MB; `bintrees16/18` == golden
> `14985902`/`68332206` (no under-count); `matrix`/`sieve`/`fib` correct; opt-out
> reverts to the broken path.
>
> **Scope correction (verified 2026-06-17): A2 and A4 are NOT closed by this** — they
> have *separate* bugs. **A2** (`ReflRepro`) still crashes with `implausible object
> size` / `inconsistent header` on the young-sweep WALK (a core array/String
> allocation↔sweep bug, distinct from the register root — precise maps fix root
> *scanning*, not the sweep walker; it actually surfaces *more* of A2's corruption by
> retaining more). **A4** (`Fork6`, under the experimental `CRATONVM_REAL_FORKJOINPOOL`
> gate) now fails with a real-FJP `ForkJoinPool` CAS conflict on *both* precise-on and
> -off, masking the original reclaim — so unverified, not regressed.
>
> **Perf:** the per-invocation `frame_record` CALL was made ~40% cheaper by caching
> the top-frame RBP in a thread-local (`82cf85e9`): **fib44 2.5× → 1.68×**; alloc/
> compute/array are neutral or slightly faster. Residual = the CALL itself (follow-up:
> inline the RBP store in codegen; the NOP-skip lever is unsafe — it anchors the frame
> walk; details in SB-CRASH-04 #5).
>
> **Regression sweep (2026-06-17, default-on vs `CRATONVM_NO_PRECISE_JIT_MAPS`):**
> **zero correctness regressions** — `bintrees10/12/14/16/18`, `matrix600/800`,
> `sieve250k`, `fib44` all checksum-identical; 10 standalone app probes
> (`AR`/`Antora`/`CHMEq`/`CollCopy`/`DOMWalk`/`Builtins`/`CPUtil`/`Asm`/`ArrInst`/`AnonM`)
> byte-identical output to legacy. The full 50+ app gauntlet remains the CI bar.
> Predecessor: the `SHADOW_STACK` reload SIGSEGV fix (`19fd6707`).

The history below predates the fix.

> **Correction (2026-06-17):** A2 was briefly believed fixed via a "JIT-scan-cache
> is unsound → default-OFF" change (`8dfd5c2b` / `bug-06b`). That was a **GC-timing
> mask, not a fix**, and was **reverted** (`b41c0484`): on current `dev` ReflRepro
> crashes identically cache-on and cache-off. `bug-06b-jit-scan-cache-unsound.md` is
> superseded by the reflrepro handoff (the genuine `collection_count` cache key it
> added was kept).

> **Verified on current `dev` (binary built 2026-06-17):** for the A3 repro
> (`MinRegexProbe code 20000`, `CRATONVM_DBG_GC_STRESS=524288`), the *only*
> correct config is `--nojit`. Every conservative trick
> (`CRATONVM_NO_JIT_SCAN_CACHE`, `CRATONVM_JIT_SAFEPOINT_REG_SPILL`,
> `CRATONVM_DBG_FULLSTACK_SCAN`, and combinations) still crashes — confirming the
> root is genuinely register-resident and unreachable by any stack scan. Every
> `CRATONVM_SHADOW_STACK` variant is also currently broken (movable → SIGSEGV;
> `+pin` → hang; `+noreload` → hang). See SB-CRASH-04 for the live signatures.
>
> **Note the asymmetry:** `CRATONVM_SHADOW_STACK` *fixes* A2 (`ReflRepro` — no
> crash) but *crashes* A3 (`MinRegexProbe`). The difference is a distinct
> **shadow-reload codegen bug** that only the A3 path exercises: the post-call
> reload writes the integer `1` into the callee-saved register holding `this`
> (`r13`/`r15`), which a later `getfield` then dereferences. So "complete the
> shadow stack" (the A2 handoff's recommended fix) must *also* fix this reload bug
> before it can be the universal fix. Pinned in SB-CRASH-04 to the reload at
> `Matcher.reset` pc=110 / `Pattern.append` pc=29.

---

## Standalone bugs

| # | Bug | Status | Doc |
|---|---|---|---|
| **C** | Deep JIT→JIT recursion overruns the **native** stack (ANTLR `closure()`); the overflow path faults instead of throwing a catchable `StackOverflowError`. Only arises with an *unmerged* cold-path-throughput experiment; needs stack-banging + a fault-recovery handler. | ⚪ **LATENT** (not a current blocker) | [springrepos-extension-hang-jit-throughput-and-deep-recursion.md](springrepos-extension-hang-jit-throughput-and-deep-recursion.md) §6–7 |

> Bug **B** (JUnit `@Timeout` "interceptor invoked twice") is **FIXED** and archived — it was
> never threading/`MethodHandle`: a synthetic natural-order compare raised `NoSuchMethodError`
> instead of `ClassCastException` for a non-`Comparable` element, escaping Spring's `catch (CCE)`
> and tripping JUnit's chain detector. See [`docs/internal/fixed-suite-bugs/spring-bug-04-junit-timeout-interceptor-double-proceed.md`](../internal/fixed-suite-bugs/spring-bug-04-junit-timeout-interceptor-double-proceed.md).

## Standalone — bug-06 assertion-mismatch family (Spring suite reflection/annotation tail)

The bug-06 census (`spring-suite/crash-reports-2026-06-16/bug-06-assertion-mismatch-families.md`)
clustered ~529 genuine assertion mismatches into 6 families. Families 1–4 are fixed
(field-updaters `fe52db3a`; `HttpClient.executor`; synthetic-`Object` superclass `40b6d94a`;
`findLoadedClass` no-load `4b923e86`, all on `dev`). The two open families are reflection/annotation
**native-return-value** correctness, not GC/JIT:

| # | Bug | Status | Doc |
|---|---|---|---|
| **F5** | Reflection native returns `null` where HotSpot returns a `Class`/`Method` (`getDeclaredMethod on null` ×28). Common paths **verified clean** (`Refl5` == HotSpot); the failing narrow generic/proxy path is not yet attributed to a test. **Do not** touch `synthetic_class_mirror` slot 0 (refuted hypothesis). | 🔴 **OPEN** — needs per-test attribution | [bug06-fam5-reflection-getdeclaredmethod-null.md](bug06-fam5-reflection-getdeclaredmethod-null.md) |
| **F6** | Spring annotation **synthesis** (`@AliasFor`/`MergedAnnotation`/`MirrorSets`) value mismatches + `AnnotationUtilsTests` aborts with a fixed ~2 GB alloc (reproduces on the pre-fix binary → pre-existing, a wrong size computation, not the `findLoadedClass` fix). | 🔴 **OPEN** — `[[spring-bug-01]]` umbrella | [bug06-fam6-annotation-synthesis-mergedannotation.md](bug06-fam6-annotation-synthesis-mergedannotation.md) |

## Consolidated suite bug docs (open & distinct — copied in 2026-06-18)

Genuine open VM defects pulled in from the suite-specific trackers
(`spring-suite/crash-reports-2026-06-16/`, `spring-suite/bugs/`,
`docs/kafka-suite-0617/`) so this folder is the single map. **These are copies** —
the originals remain in their suite folders (which keep their own numbering). Only
open, distinct bugs were copied; FIXED docs (bug-03, crash-01/02/03,
spring-bug-02/03/04/05/09/12, kafka bug-A — see `docs/internal/fixed-suite-bugs/`)
and already-consolidated ones (fam5/6) were left in place.

| Bug | Category | Status | Doc |
|---|---|---|---|
| String constant corrupted → `Object` under load | VM-CORRECTNESS / GC | 🟡 **OPEN / needs batch re-verify** — Family-A; **not observed** in 2026-06-20 per-class + small-batch runs. Family-A precise-maps are default-on now and the `toArray` recursion that destabilised the launcher is fixed; a large single-JVM batch is needed to confirm resolution. | [springsuite-bug-04-string-constant-corrupted-under-load.md](springsuite-bug-04-string-constant-corrupted-under-load.md) |
| `MergedAnnotations` hang | VM-HANG | ✅ **FIXED 2026-06-20** (`8795b88d`) — was the `ReferencePipeline.toArray(IntFunction)` self-recursion; `MergedAnnotationsTests` now 174/178 (residual 4 = synthesis mismatch, cf. bug-06 F6) | [toArray-recursion fix](../internal/fixed-suite-bugs/springsuite-0620-toarray-referencepipeline-recursion.md) |
| Serializable proxy round-trip | VM-CORRECTNESS (proxy + serialization) | 🔴 **OPEN** | [spring-bug-08-serializable-proxy-roundtrip.md](spring-bug-08-serializable-proxy-roundtrip.md) |
| JUnit-platform execution `LoadError` | VM-CORRECTNESS / dispatch | 🔴 **OPEN** — JUnit platform internals; Family-A GC-root race (NOT related to the now-fixed bug-04, which was a non-`Comparable` compare exception-type bug, not a GC race) | [spring-bug-10-junit-platform-execution-loaderr.md](spring-bug-10-junit-platform-execution-loaderr.md) |
| Groovy / scheduler crashes (rc=139) | VM-CRASH | 🟡 **PARTIAL** — Groovy SIGSEGV fixed via the bug-12 HashMap-layout fix; residual = a separate Groovy **hang at BEGIN** (inventory, needs per-cluster trace) | [spring-bug-11-groovy-and-scheduler-crashes.md](spring-bug-11-groovy-and-scheduler-crashes.md) |
| Mockito `mockStatic` + mock dispatch | VM-CORRECTNESS (Mockito dispatch) | 🔴 **OPEN** — root-caused; High (Mockito pervasive in Kafka suite) | [kafka-bug-B-mockito-mockstatic-mock-dispatch.md](kafka-bug-B-mockito-mockstatic-mock-dispatch.md) |
| `WeakHashMap` stream infinite hang | VM-HANG → JIT codegen | 🟡 **HANG FIXED** (`1cd0ab26`, JIT ban; verified) — underlying `dup_x1` field-post-increment codegen defect still OPEN | [kafka-bug-C-weakhashmap-stream-infinite-hang.md](kafka-bug-C-weakhashmap-stream-infinite-hang.md) |

> `springsuite-bug-04` and `spring-bug-10` are **Family A** (GC-root-coverage-under-JIT)
> manifestations seen from the Spring suite — same root cause as A1–A4 above, different
> entry points. Fixing precise JIT stack roots should clear them; tracked there.

## The springrepos handoff (mostly fixed)

[springrepos-extension-hang-jit-throughput-and-deep-recursion.md](springrepos-extension-hang-jit-throughput-and-deep-recursion.md)
is a multi-defect handoff. `dev` now **passes** `SpringRepositoriesExtensionTests`.
Of its decomposed defects: the `SecureClassLoader.pdcache` NPE, the
`AssertionError`-preload JIT bail, the `hashCode`/`equals`-override JIT compile,
and the root-snapshot hang are all **fixed on dev**. Its only still-relevant open
items are **its "defect #2" (= family A3 above)** and **bug C** (the cold-path
deep-recursion overflow). It is kept for that context and the deep-recursion
stack-guard design.

## Standalone — Hibernate JAXB class-load storm (✅ FIXED)

[hibernate-jaxb-classload-synthetic-stub-rescan-storm.md](hibernate-jaxb-classload-synthetic-stub-rescan-storm.md)
— HIB-DEV-03. The **dominant** layer of the JAXB-XML-mapping hang. **Not** GC/JIT,
**not** `retainAll`. Every `HashMap`/`LinkedHashMap` node insert allocates a
`cratonvm/synthetic/AnonymousObject$N` whose synthetic-stub "upgrade" re-ran a
**full classpath scan** (`find_class_bytes_delegated`) — O(num_jars) — on *every
allocation*; with the ~250-JAR Hibernate classpath, map-heavy JAXB model building
crawled to a `rc=124` timeout. ✅ **FIXED** (`fix/hib-dev-03-jaxb-classload`):
memoize the known-absent result in `ClassManager`, re-armed on classpath
extension. A/B: 20 000-node put loop 20 120 ms → 132 ms (now classpath-independent
≈ HotSpot's allocation scaling). Unmasks the deeper JTA/socket cluster below.

## Standalone — Hibernate JTA cluster (Narayana XA + socket loopback) — ✅ RESOLVED

[docs/internal/hibernate-jta-narayana-xa-completion-and-socket-loopback.md](../internal/hibernate-jta-narayana-xa-completion-and-socket-loopback.md)
— found in the full Hibernate ORM 8.x suite census (dev, 2026-06-17); **re-investigated &
resolved 2026-06-20**. **Not** a GC/JIT issue. Three "layers": (0) `ServerSocket.getInetAddress()`
→ null → `TxControl.<clinit>` NPE — **fixed on dev** (`net_phase_e.rs` `getInetAddress`, `ada6cebf`);
(1) Narayana **XA completion** — **was never broken on dev (REFUTED)**: `XaProbe`/`HibBoot` show
`enlist=true → XA.commit(onePhase=true) → commit OK`, the H2 connection commits+releases, `truncate`
does not block; (2) the default-mode "loopback never pairs" hang — **real root cause = a process-wide
`accept()` deadlock** (the global `s2_registry` lock was held across the blocking `TcpListener::accept()`,
so Narayana's Listener thread blocked the main thread's `SocketProcessId` bind) **plus** a separate
`new ServerSocket(0).getLocalPort()==0` bug. Both **fixed** on branch `fix/hib-jta-xa-loopback`
(`e0426050`, pending merge). A full Hibernate 8.1 + Narayana 7.3.4 + H2 begin/persist/commit/read-back/
truncate cycle now passes in default (synthetic-socket) mode.

## Standalone — Hibernate JAXB/ByteBuddy bootstrap slow (XML mapping hangs) — ✅ RESOLVED / no-repro

[docs/internal/hibernate-jaxb-classloading-bytebuddy-bootstrap-slow.md](../internal/hibernate-jaxb-classloading-bytebuddy-bootstrap-slow.md)
— full-suite census (dev, 2026-06-17); **re-verified 2026-06-20**. The dominant class-load rescan storm is
**fixed on dev** (`1db07c35`/`25c42e13`). The residual ByteBuddy `MethodGraph` bootstrap hang **does not
reproduce**: a programmatic JOINED-inheritance bootstrap (forcing `JoinedSubclassEntityPersister` +
`MethodGraph$Compiler$Default.doAnalyze`) completes in ~16s `--nojit` (census mode), well under the 600s
timeout. The original `retainAll` framing was refuted. Caveat: the original JAXB-XML *JUnit* repro classes
can't be re-run via the launcher because of a separate `@ExtendWith` meta-annotation gap (next entry).

## Standalone — JUnit 5 `@ExtendWith` meta-annotation `ParameterResolver` (🔴 OPEN, new)

[junit5-extendwith-meta-annotation-parameterresolver.md](junit5-extendwith-meta-annotation-parameterresolver.md)
— found 2026-06-20 while running the Hibernate JUnit suite. A composed/meta annotation that carries
`@ExtendWith(...)` (e.g. Hibernate's `@Jpa`) does not register its extensions → `No ParameterResolver
registered for [EntityManagerFactoryScope]`. Annotation-synthesis family (the meta-present `@Repeatable`
`@ExtendWith` arm; direct-element merge is already on dev). Blocks running the real Hibernate JUnit suite
end-to-end; subsystems were validated by programmatic bootstrap instead.

## Standalone — Hibernate deserialized SessionFactory is null

[hibernate-deserialization-sessionfactory-reconnect-null.md](hibernate-deserialization-sessionfactory-reconnect-null.md)
— full-suite census (dev, 2026-06-17). 5 serialization round-trip tests NPE
(`getMappingMetamodel`/`getClassLoaderService` on null) because a deserialized
`EntityManager`/`SessionFactory` doesn't reconnect to the live factory. Generic
`readObject`/`readResolve` work on CV (verified); the gap is Hibernate's
`SessionFactoryRegistry.findSessionFactory(uuid,name)` returning null after deser. 🔴 open.

## Hibernate full-suite census (dev 2026-06-17) — additional docs

Per-run bug reports relocated here from the (gitignored) `apps/hibernate-orm/cratonvm-bug-reports/dev-run-20260617/`:
- [hibernate-json-function-sigsegv-al_state-foreign-receiver.md](hibernate-json-function-sigsegv-al_state-foreign-receiver.md) — ✅ **FIXED** (`al_state` ArrayList-layout guard; 4 `function.json.*` SIGSEGV classes).
- [hibernate-throwable-stacktrace-order-reversed-FIXED.md](hibernate-throwable-stacktrace-order-reversed-FIXED.md) — ✅ **FIXED** (`getStackTrace()`/`printStackTrace()` were reversed).
- ~~hibernate-jta-txcontrol-getinetaddress-per-class-report.md~~ — consolidated 2026-06-18, then **moved to [`docs/internal/`](../internal/hibernate-jta-txcontrol-getinetaddress-per-class-report.md) 2026-06-20** (Layer 0 fixed on dev; a redirect stub into the resolved JTA doc).
- [hibernate-hang-clusters-summary.md](hibernate-hang-clusters-summary.md) — census overview; **H1/H2/H3 resolved 2026-06-20** (per-cluster docs now in `docs/internal/`; H4 not re-checked).

Also fixed on dev this run (no standalone doc — see commit): `Locale.toLanguageTag()` dropped all subtags for real Locales (`13e8c761`).

- [hibernate-wrong-result-assertion-failures.md](../internal/fixed-suite-bugs/hibernate-wrong-result-assertion-failures.md) — cluster of CV-only wrong-result assertion FAILs (UniqueConstraintBatching 1-vs-0, DetachedBag true-vs-false, EntityGraphBatchSize, immutable+converter deser, …); each likely a separate root cause. 🔴 open (handoff).

## Consolidation log

- **2026-06-17:** Merged `precise-jit-stack-maps-multithread-fjp-worker-testcase.md`
  (handoff/testcase) and `precise-jit-stack-maps-fork6-findings.md` (findings)
  into a single [fork6-fjp-multithread-jit-root-reclamation.md](fork6-fjp-multithread-jit-root-reclamation.md)
  — they described the *same* Fork6 bug (A4). Added this index framing the
  A1–A4 family + standalone B/C, and recorded the current-`dev` A3 verification.
