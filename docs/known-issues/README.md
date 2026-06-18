# Known issues — index & bug map

This folder collects CratonVM-only defects found while running upstream Java
suites. The docs had grown to describe the **same underlying bug from several
angles**; this index is the consolidated map. Read it first.

## How many distinct bugs are here?

Four of the docs are all manifestations of **one root-cause family** — the
*GC-root-coverage-under-JIT* family — plus **two standalone bugs**. Net: **2 open
GC-root manifestations, 1 open standalone bug, 1 latent standalone bug** (the
other GC-root manifestations are already fixed on `dev`).

That headline covers the original GC-root cluster. The folder also holds the
**Hibernate** standalone cluster, the **bug-06** Spring reflection/annotation
families (F5/F6), and — added 2026-06-18 — **7 more open, distinct suite bugs**
consolidated from the per-suite trackers (see *Consolidated suite bug docs* below).
Two of those (`springsuite-bug-04`, `spring-bug-10`) are themselves Family A
manifestations seen from the Spring suite.

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
| **B** | JUnit `@Timeout` interceptor chain `proceed()` invoked twice (cross-thread invocation / `MethodHandle` re-entry on the timeout worker). **Not GC-related.** Clears a broad band of Spring tests. | 🔴 **OPEN** | [spring-bug-04-junit-timeout-interceptor-double-proceed.md](spring-bug-04-junit-timeout-interceptor-double-proceed.md) |
| **C** | Deep JIT→JIT recursion overruns the **native** stack (ANTLR `closure()`); the overflow path faults instead of throwing a catchable `StackOverflowError`. Only arises with an *unmerged* cold-path-throughput experiment; needs stack-banging + a fault-recovery handler. | ⚪ **LATENT** (not a current blocker) | [springrepos-extension-hang-jit-throughput-and-deep-recursion.md](springrepos-extension-hang-jit-throughput-and-deep-recursion.md) §6–7 |

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
spring-bug-02/03/05/09/12, kafka bug-A) and already-consolidated ones (fam5/6,
spring-bug-04) were left in place.

| Bug | Category | Status | Doc |
|---|---|---|---|
| String constant corrupted → `Object` under load | VM-CORRECTNESS / GC | 🔴 **OPEN** — a **Family A** (GC-root-undercount) manifestation, load-dependent | [springsuite-bug-04-string-constant-corrupted-under-load.md](springsuite-bug-04-string-constant-corrupted-under-load.md) |
| `MergedAnnotations` hang | VM-HANG | 🔴 **OPEN** — first hang found in the suite; annotation synthesis (cf. bug-06 F6) | [spring-bug-06-mergedannotations-hang.md](spring-bug-06-mergedannotations-hang.md) |
| Serializable proxy round-trip | VM-CORRECTNESS (proxy + serialization) | 🔴 **OPEN** | [spring-bug-08-serializable-proxy-roundtrip.md](spring-bug-08-serializable-proxy-roundtrip.md) |
| JUnit-platform execution `LoadError` | VM-CORRECTNESS / dispatch | 🔴 **OPEN** — JUnit platform internals; GC-race-adjacent (cf. bug-04) | [spring-bug-10-junit-platform-execution-loaderr.md](spring-bug-10-junit-platform-execution-loaderr.md) |
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

## Standalone — Hibernate JTA cluster (Narayana XA + socket loopback)

[hibernate-jta-narayana-xa-completion-and-socket-loopback.md](hibernate-jta-narayana-xa-completion-and-socket-loopback.md)
— found in the full Hibernate ORM 8.0 suite census (dev, 2026-06-17). **Not** a
GC/JIT issue. Three layers: (0) `ServerSocket.getInetAddress()` → null →
`TxControl.<clinit>` NPE — **fixed** (`net_phase_e.rs` `getInetAddress` native);
(1) Narayana **XA transaction completion** doesn't commit/release the enlisted H2
connection → `@AfterEach truncate` blocks on H2 lock timeout → hang (🔴 open);
(2) default-mode synthetic `ServerSocket` accept/connect **loopback** doesn't pair
→ `TransactionStatusManager` bring-up hangs (🔴 open). Layers 1–2 are a deep
JTA/XA + socket-subsystem handoff.

## Standalone — Hibernate JAXB/ByteBuddy bootstrap slow (XML mapping hangs)

[hibernate-jaxb-classloading-bytebuddy-bootstrap-slow.md](hibernate-jaxb-classloading-bytebuddy-bootstrap-slow.md)
— full-suite census (dev, 2026-06-17). XML/JAXB mapping classes + complex-entity
bootstrap time out (`rc=124` @ 600s). **Re-diagnosed**: NOT a `retainAll`/collection
loop (standalone `LinkedHashMap.keySet().retainAll` is correct). Live `cdb` shows the
time is in **class loading** (`alloc_object → ensure_synthetic_class → ZipArchive::by_name
→ indexmap/hashbrown`) during JAXB reflection model-building, and ByteBuddy `MethodGraph`
proxy generation — i.e. interpreter throughput / class-loading, possibly an intermittent
zip-index hot spot. 🔴 open (handoff).

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
- [hibernate-jta-txcontrol-getinetaddress-per-class-report.md](hibernate-jta-txcontrol-getinetaddress-per-class-report.md) — per-class companion to the JTA Narayana known-issue (entry crash ✅ fixed; XA/socket layers 🔴 open).
- [hibernate-hang-clusters-summary.md](hibernate-hang-clusters-summary.md) — overview of the 22 census hangs grouped by root cause (JAXB/class-load, ByteBuddy MethodGraph, JTA/socket).

Also fixed on dev this run (no standalone doc — see commit): `Locale.toLanguageTag()` dropped all subtags for real Locales (`13e8c761`).

## Consolidation log

- **2026-06-17:** Merged `precise-jit-stack-maps-multithread-fjp-worker-testcase.md`
  (handoff/testcase) and `precise-jit-stack-maps-fork6-findings.md` (findings)
  into a single [fork6-fjp-multithread-jit-root-reclamation.md](fork6-fjp-multithread-jit-root-reclamation.md)
  — they described the *same* Fork6 bug (A4). Added this index framing the
  A1–A4 family + standalone B/C, and recorded the current-`dev` A3 verification.
