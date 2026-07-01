# Known issues — index & bug map

This folder collects CratonVM-only defects found while running upstream Java
suites. The docs had grown to describe the **same underlying bug from several
angles**; this index is the consolidated map. Read it first.

## Bug-document lifecycle

Every unresolved bug document belongs under `docs/known-issues`. Once the bug
is fixed, resolved, or refuted, move the write-up out of this folder and archive
it under `docs/internal`.

## How many distinct bugs are here?

After consolidation (full re-count 2026-06-18, kafka-bug-B/C status reconciled 2026-07-01),
the ~30 docs map to **one root-cause family + ~15 distinct standalone bugs**, of which
**9 are already FIXED on `dev`** (the 6 prior + `kafka-bug-C` and both `kafka-bug-B`
fixes). Headline:

**~8 distinct OPEN defects + 1 latent** (was ~10 — family-A **A2** was fixed
2026-06-23, `6e3ddb05`), grouped as:

1. **Family A — GC root coverage under JIT** (one root cause, several manifestations). Open members:
   **A4** (Fork6 FJP multi-thread, gated) — the last open member. **A1/A2/A3/A5 are FIXED** — **A2**
   (`ReflRepro`) was re-diagnosed and fixed 2026-06-23 (`6e3ddb05`): it was **never** a
   register/native-return missed root, but a GC-side non-moving-sweep free-list double-serve
   (overlapping free blocks not coalesced → `Arena::alloc` served the same region twice); A5 was
   root-caused to the **compiled entry-point
   `main`'s JIT frame being unregistered** (invoked via `Vm::invoke` without a `JitEntryGuard`, so
   the moving young collector relocated its roots); fixed by detecting an unregistered JIT frame on
   the native stack → non-moving sweep + full-stack mark (dev `77c98761`; writeup moved to
   [`docs/internal/app-jvm-bugs/gc-stress-bintrees-main-args-unregistered-jit-frame-FIXED.md`](../internal/app-jvm-bugs/gc-stress-bintrees-main-args-unregistered-jit-frame-FIXED.md)).
   `spring-bug-10` is a Family-A manifestation seen from
   the Spring suite (same root cause, different entry point); `springsuite-bug-04` was the same race
   but **no longer reproduces** (doc removed). The suite-scale field evidence in
   `jit-junit-discovery-reflection-corruption.md` is the same race.
2. **Standalone B** — JUnit `@Timeout` interceptor double-`proceed()` (open).
3. **Standalone C** — deep JIT→JIT recursion native-stack overflow (latent; only with an unmerged experiment).
4. **bug-06 F5** — reflection native returns null vs a `Class`/`Method` (open, unattributed).
5. **bug-06 F6 / `spring-bug-06` / `spring-bug-01`** — annotation **synthesis** value-mismatches.
   (The `MergedAnnotations` **hang** + the `AnnotationUtilsTests` "~2 GB OOM" in this cluster were the
   `toArray` self-recursion and are **FIXED** 2026-06-20, commit `8795b88d`; `MergedAnnotationsTests`
   now runs 174/178, `AnnotationUtilsTests` 72/72. Only the synthesis value-mismatches remain open.)
6. **`spring-bug-08`** — serializable JDK-proxy round-trip (open).
7. **`spring-bug-11` residual** — Groovy hang at `BEGIN` (the SIGSEGV half is FIXED via bug-12; open).
8. ~~**`kafka-bug-B`**~~ — Mockito `mockStatic` + `mock`/`mockConstruction` dispatch:
   **FIXED on `dev`**. The stale JIT call-site residual was fixed by redef-time compiled
   dispatch quiescing; archived at
   [`docs/internal/fixed-suite-bugs/kafka-bug-B-mockstatic-capturing-lambda-jit.md`](../internal/fixed-suite-bugs/kafka-bug-B-mockstatic-capturing-lambda-jit.md).
9. ~~**`kafka-bug-C`**~~ — `WeakHashMap.values().stream()` infinite hang: **FIXED on `dev`** (`1cd0ab26`; doc removed).
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
    - ✅ **getBeanClassName bean-filter (bug-B) FIXED** — primary filter fix holds, and **bug-B2
      (CGLIB method-injection) is fully implemented 2026-06-21** (commit `ffa71253`): the instantiate
      shim synthesises a concrete subclass overriding each abstract `<lookup-method>`/`@Lookup` method
      via `bf.getBean(name|args)` (NullBean→null), `bf.getBeanProvider(ResolvableType.forMethodReturnType(m)).getObject()`
      for generic by-type, with overload-aware + child-precedence override matching.
      **`LookupMethodTests` 0/7 → 7/7** (JIT and `--nojit`), **`LookupAnnotationTests` 0/10 → 10/10**.
      (The "flaky JIT `invokeVoid`" turned out to be a one-byte emitter typo — `IFEQ` vs `IFNULL` —
      not a VM defect.) [bug-B / bug-B2 doc](../internal/spring/springsuite-0619-getbeanclassname-bean-filter.md) — ✅ **FIXED on dev** (`6d596473`; moved to docs/internal).
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
    global default). Supersedes the former `reactor-worker-thread-leak-at-shutdown.md` (removed — see git history).
17. **[CompletableFuture untimed `get()` never wakes on cross-thread completion](../internal/app-jvm-bugs/gc-gen-promotion-completablefuture-completion-loss.md)** —
    ✅ **FIXED on dev** (moved to `docs/internal/app-jvm-bugs/`). The original gen-GC "lost young `Signaller`"
    theory was **refuted** (the hang is deterministic + GC-independent); the real cause was the synthetic
    `CompletableFuture.complete` native never running `postComplete()`. Fixed in the native — no GC change.
18. **[GC: live young `Thread` mirror in a blocked thread's frame reclaimed (Tomcat real-net/real-AQS HARD CRASHES)](gc-blocked-thread-frame-stale-thread-mirror.md)** —
    🟠 **OPEN on dev**, but a **defensive crash-mitigation is MERGED** (`6a04b0e3` + `e06ed934`). Six Tomcat
    encoding/tribes classes SIGSEGV/panic (`compact_value.rs:502`) under `CRATONVM_REAL_NET_SOCKETS` +
    `CRATONVM_REAL_AQS`: a live young `java.lang.Thread` mirror held in a **blocked** thread's frame local
    (object-tagged, so NOT the lost-tag item 15) is reclaimed because it is missing from that thread's
    deposited `root_snapshot`; the freed slot is reused as byte-buffer data and decoded as an object pointer.
    Likely the **same underlying bug** as the `currentThread()`-mirror reclamation in
    [`repros/gc-concurrent-spawn-reclamation/`](repros/gc-concurrent-spawn-reclamation/) whose fix is on an
    **unpushed** branch (`feat/precise-maps-a4-finish`) → not on dev. The mitigation (`plausible_heap_pointer`
    gate at every ref-decode + JIT receiver-deref boundary) degrades a stale ref to a Java NPE — all 6 are now
    **crash-free jit+nojit** but still fail/time out (the reclamation itself is unfixed).
19. **[Hibernate `type.temporal.*` — moving GC strands lambda refs in native stream/collection intrinsics](hib-temporal-gc-lambda-native-stale-local.md)** —
    🟠 **OPEN** (fix in progress: per-native pinning). The 5 `org.hibernate.orm.test.type.temporal.*` classes
    abort rc=1 / SIGSEGV with `linkage error: no such method java/lang/Object.<sam>` — **not** a java.time
    binding bug. Same native-stale-Rust-local family as the StackWalker corruption
    ([hibernate-bytearraymapping-stackwalk-gc-corruption.md](hibernate-bytearraymapping-stackwalk-gc-corruption.md)):
    `Stream.forEach`/`sorted`, `Spliterator.tryAdvance`/`forEachRemaining`, `ArrayList.forEach` hold the lambda
    + materialized elements in Rust locals across `invoke_virtual`; the moving young collector relocates them
    out from under the stale local. `-Xmx8g` passes; default heap ~50–70 % crash. Fix = `pin_native_root` /
    `read_native_pin` per native (NOT force-non-moving — that hits the HIB-CV-33 precise-root gap).

FIXED bugs whose standalone docs were **removed** from this folder (resolved; full writeups in
`git` history or [`docs/internal/fixed-suite-bugs/`](../internal/fixed-suite-bugs/)): A1 (reflection
mirror-array pinning), A3 (register-invisibility — precise maps default-on), the Hibernate JAXB
class-load rescan storm (HIB-DEV-03), the JSON-function `al_state` SIGSEGV, the reversed stack-trace
order, the `ReferencePipeline.toArray(IntFunction)` recursion, and the off-heap DirectBuffer
(bug-A/A2). The springrepos hang is mostly fixed (`dev` passes the test; only the latent
deep-recursion item remains — see below).

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

> **Current-dev refresh (re-verified 2026-06-29, fresh build off dev HEAD
> `9928052c`; precise-jit-maps-default Steps 1–8).** **A3 is CLOSED** by precise
> JIT oop maps (default-on) — validated green across the GC-root repro+bench lane,
> OSR frames, and the BouncyCastle app suites
> (`test-infra/regression-pool/gc-root-lane.sh` + `gc-root-apps-lane.sh`).
> **A2 is now FIXED** (`6e3ddb05`, 2026-06-23): `ReflRepro 8000 @ GC_STRESS=65536`
> → `ok=8000 bad=0 rc=0` (and `20000 @ GC_STRESS=524288 --Xmx 256m` → `bad=0`),
> previously `rc=139`. It was **never** a register/native-return missed root — it
> was a GC-side non-moving-sweep free-list double-serve (overlapping free blocks
> not coalesced), fixed in the sweep coalescer; precise maps are orthogonal.
> **A4 is OPEN but non-fatal on the repro here:** gated
> `CRATONVM_REAL_FORKJOINPOOL=1` Fork6 is **26/26 ALL-OK** on current dev (8
> sequential + 18 concurrent-stress; the older ~15% reclamation does not
> reproduce), though the cross-thread STW JIT-root gap is still *exercised*
> (`scan_active_jit_frames` WARN, `cross_thread_jit_gap_hits` incrementing) — that
> scan is the tracked follow-up. The *family is not yet formally retired* (A4 +
> container-app CI remain); **nothing was removed** — all repros, GC guards,
> `CRATONVM_DBG_*` knobs, and the shadow stack are retained as experimental/debug
> tools.
>
> See `docs/feature-designs/precise-jit-maps-default.md` "Step 8 — GC-root family
> retirement status".

The eventual correct fix for the whole family is **precise JIT stack roots**
(know exactly which registers/slots hold oops at each safepoint), tracked under
`project_precise_jit_stack_maps`. The `CRATONVM_SHADOW_STACK` mechanism is the
current (incomplete/buggy) implementation of that.

| # | Manifestation | Repro | Status | Doc |
|---|---|---|---|---|
| **A1** | Reflection mirror-array builders held an `ObjectRef` array in a Rust local across allocating calls (`Field[]`/`Method[]`/annotation arrays) | `wildfly-suite/repro/MinRepro` | ✅ **FIXED on dev** (`pin_native_root` sweep) | _(doc removed; resolved)_ |
| **A2** | **`implausible object size` young-sweep-walker crash** (reflection/String-array allocation churn) — a *distinct* bug, NOT the register root: a non-moving-sweep free-list double-serve (overlapping free blocks not coalesced) | `wildfly-suite/repro/ReflRepro` | ✅ **FIXED on dev** (`6e3ddb05`, 2026-06-23; coalesce overlapping free blocks) — `ReflRepro 8000 @ GC_STRESS=65536` → `ok=8000 bad=0` (re-verified 2026-06-29); precise maps orthogonal; moved to docs/internal | [reflrepro-register-resident-jit-root-handoff.md](../internal/app-jvm-bugs/reflrepro-register-resident-jit-root-handoff.md) |
| **A3** | **Register-invisibility** — a live oop sits only in a CPU register at a young-GC safepoint, invisible to the stack-only scan (single thread) | `apps/spring-boot/buildSrc/runner/MinRegexProbe` | ✅ **FIXED on dev** (`32649b56`, precise maps default-on) | _(doc removed; resolved)_ |
| **A4** | Multi-thread: live `ForkJoinTask`s reclaimed under **FJP worker threads** + a **lost-tag** interpreter local | `scratch/xworker/Fork6` (needs `CRATONVM_REAL_FORKJOINPOOL=1`) | 🟡 **OPEN but non-fatal here** — `Fork6` 26/26 ALL-OK on current dev (re-verified 2026-06-29; the older ~15% reclaim does not reproduce), but the cross-thread STW JIT-root gap is still *exercised* (`scan_active_jit_frames` WARN); genuine residual = a register-only oop at a non-call safepoint. Benign real-FJP `cas_long` retry noise still fires | [fork6-fjp-multithread-jit-root-reclamation.md](fork6-fjp-multithread-jit-root-reclamation.md) |
| **A5** | **Object-binarytrees moving-GC corruption** — the compiled entry-point `main`'s JIT frame is invisible to `gc_quiescence` (invoked via `Vm::invoke` without a `JitEntryGuard`), so the **moving** young collector relocates its roots and can't rewrite the raw stack slots → stale all-zero receiver. (The earlier "register-only stale root in `bottomUpTree`" framing was wrong — `bottomUpTree` isn't even compiled at the crash.) | [`repros/gc-stress-bintrees-main-args/`](repros/gc-stress-bintrees-main-args/) (`VAAload`) | ✅ **FIXED** (dev `77c98761`) — detect an unregistered JIT frame on the native stack → non-moving sweep + full-stack mark. Residual: Windows-only (portable stack-bound is a follow-up) | [docs/internal/.../gc-stress-bintrees-main-args-unregistered-jit-frame-FIXED.md](../internal/app-jvm-bugs/gc-stress-bintrees-main-args-unregistered-jit-frame-FIXED.md) |

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
> have *separate* bugs. **[SUPERSEDED 2026-06-23: A2 is now FIXED — `6e3ddb05`, a
> free-list double-serve in the sweep coalescer, not a register root; `ReflRepro`
> `bad=0`. See the top-of-section refresh + the A2 table row. A4 remains open.]**
> **A2** (`ReflRepro`) *(then)* still crashed with `implausible object
> size` / `inconsistent header` on the young-sweep WALK (a core array/String
> allocation↔sweep bug, distinct from the register root — precise maps fix root
> *scanning*, not the sweep walker; it actually surfaced *more* of A2's corruption by
> retaining more — a symptom-timing effect, since cured). **A4** (`Fork6`, under the experimental `CRATONVM_REAL_FORKJOINPOOL`
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
| **MT-STW** | `Thread.join` monitor-ownership **desync under concurrent GC** (JIT-off): `monitor_wait`/terminate-tail used a raw `ObjectRef` captured before blocking, then `arrive_and_wait` let the in-flight STW relocate+zero it → `ensure_inflated` on the stale address synthesised a fresh `owner=None` monitor → IMSE in the javac synchronized-exit loop → joiner livelock → STW wedge. ~1–3% on `scratch_churn/Churn.java`; the tail after the four barrier+expansion fixes (`68c6993e`, 0%→~97%). | ✅ **FIXED** (`74b195b4`) — remap the receiver through `arrive_and_wait`'s returned pointer map; Churn 0 hangs / ~480 runs | [../internal/mt-stw-join-monitor-desync.md](../internal/mt-stw-join-monitor-desync.md) |

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
| **F6** | Spring annotation **synthesis** (`@AliasFor`/`MergedAnnotation`/`MirrorSets`) value mismatches (the `AnnotationUtilsTests` "~2 GB OOM" half was the `toArray` self-recursion, now **FIXED**). | 🔴 **OPEN** — `[[spring-bug-01]]` umbrella | _(doc removed; tracked on branch `fix/bug06-fam6-repeatable-merge`)_ |

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
| String constant corrupted → `Object` under load | VM-CORRECTNESS / GC | ✅ **RESOLVED / NOT REPRODUCED 2026-06-20** — a 45-class single-JVM spring-core batch on dev ran clean (0 `status=java.lang.Object`, all OK, rc=0); Family-A precise-maps default-on + `toArray` recursion fixed. Doc removed (writeup in git history). | _(removed)_ |
| `MergedAnnotations` hang | VM-HANG | ✅ **FIXED 2026-06-20** (`8795b88d`) — was the `ReferencePipeline.toArray(IntFunction)` self-recursion; `MergedAnnotationsTests` now 174/178 (residual 4 = synthesis mismatch, cf. bug-06 F6) | [toArray-recursion fix](../internal/fixed-suite-bugs/springsuite-0620-toarray-referencepipeline-recursion.md) |
| Serializable proxy round-trip | VM-CORRECTNESS (proxy + serialization) | ✅ **RESOLVED on `dev`** — the standalone serialize→deserialize JDK-proxy repro round-trips correctly; `SerializableTypeWrapperTests` generic-type-render residual tracked on branch `fix/generic-array-type-tostring`. Doc removed. | _(removed)_ |
| JUnit-platform execution `LoadError` | VM-CORRECTNESS / dispatch | 🔴 **OPEN** — JUnit platform internals; Family-A GC-root race (NOT related to the now-fixed bug-04, which was a non-`Comparable` compare exception-type bug, not a GC race) | [spring-bug-10-junit-platform-execution-loaderr.md](../internal/spring-bug-10-junit-platform-execution-loaderr.md) |
| Groovy / scheduler crashes (rc=139) | VM-CRASH | 🟡 **PARTIAL** — Groovy SIGSEGV fixed via the bug-12 HashMap-layout fix; residual = a separate Groovy **hang at BEGIN** (inventory, needs per-cluster trace) | [spring-bug-11-groovy-and-scheduler-crashes.md](../internal/spring-bug-11-groovy-and-scheduler-crashes.md) |
| Mockito `mockStatic` + mock dispatch | VM-CORRECTNESS (Mockito dispatch) | ✅ **FIXED on `dev`** — the dispatch/shadowing half landed earlier, and the stale JIT call-site residual was fixed by redefine-time compiled dispatch quiescing. | [fixed residual](../internal/fixed-suite-bugs/kafka-bug-B-mockstatic-capturing-lambda-jit.md) |
| `WeakHashMap` stream infinite hang | VM-HANG → JIT codegen | ✅ **FIXED on `dev`** (`1cd0ab26`, JIT ban; verified; doc removed). The underlying `dup_x1` field-post-increment codegen weakness is tracked in the [JIT regalloc family doc](jit-regalloc-callee-saved-clobber-family.md). | _(removed)_ |

> `spring-bug-10` is a **Family A** (GC-root-coverage-under-JIT) manifestation seen from the
> Spring suite — same root cause as A1–A4 above, a different entry point. Fixing precise JIT
> stack roots should clear it; tracked there. (`springsuite-bug-04` was the same race but no
> longer reproduces — doc removed.)

## The springrepos handoff (mostly fixed)

[springrepos-extension-hang-jit-throughput-and-deep-recursion.md](springrepos-extension-hang-jit-throughput-and-deep-recursion.md)
is a multi-defect handoff for `SpringRepositoriesExtensionTests`. The hang,
parse-NPE (#1), generics (#2), and the indy `MethodHandle.type()` layers
(3/3b/3c/3d) are all **fixed**, and **layer 3e is now ✅ FIXED on dev** (`7335f918`)
— the test is **11/11 FULL GREEN**. The 3e writeup (the Groovy indy call on a
**Mockito mock**, `this.repositories.maven { … }`) moved to
[`docs/internal/spring/spring-boot-groovy-indy-mockito-mock-dispatch.md`](../internal/spring/spring-boot-groovy-indy-mockito-mock-dispatch.md).
The 3c/3d fix writeup is in
[`docs/internal/spring-boot-groovy-indy-runtime-argcount-3c-FIXED.md`](../internal/spring-boot-groovy-indy-runtime-argcount-3c-FIXED.md).
Also still relevant: **defect #2 (= family A3 above)** and **bug C** (the
cold-path deep-recursion overflow).

## Resolved standalone bugs (full writeups in `docs/internal/`)

These were open here and are now **fixed / do-not-reproduce**; the detailed writeups live under
[`docs/internal/`](../internal/):

- **Hibernate JAXB class-load storm** (HIB-DEV-03) — ✅ FIXED (`fix/hib-dev-03-jaxb-classload`): the
  synthetic-stub "upgrade" re-ran a full O(num_jars) classpath scan on every map-node allocation; memoizing
  the known-absent result dropped a 20k-node put loop 20 120 ms → 132 ms.
- **Hibernate JTA cluster** (Narayana XA + socket loopback) — ✅ RESOLVED. L0 `getInetAddress` NPE fixed on
  dev (`ada6cebf`); L1 XA-completion was never broken (refuted); L2 was a process-wide `accept()` deadlock +
  `getLocalPort()==0`, fixed on `fix/hib-jta-xa-loopback`. → [`docs/internal/hibernate-jta-narayana-xa-completion-and-socket-loopback.md`](../internal/hibernate-jta-narayana-xa-completion-and-socket-loopback.md).
- **Hibernate JAXB/ByteBuddy bootstrap slow** — ✅ RESOLVED / no-repro (`1db07c35`/`25c42e13`). → [`docs/internal/hibernate-jaxb-classloading-bytebuddy-bootstrap-slow.md`](../internal/hibernate-jaxb-classloading-bytebuddy-bootstrap-slow.md).
- **JUnit 5 `@ExtendWith` meta-annotation `ParameterResolver`** — ⚠️ MISDIAGNOSED / does-not-reproduce;
  annotation discovery is byte-identical to HotSpot and `JtaCustomAfterCompletionTest` passes 5/5. → [`docs/internal/junit5-extendwith-meta-annotation-parameterresolver.md`](../internal/junit5-extendwith-meta-annotation-parameterresolver.md).

## Standalone — Hibernate deserialized SessionFactory is null

[docs/internal/hibernate-deserialization-sessionfactory-reconnect-null.md](../internal/hibernate-deserialization-sessionfactory-reconnect-null.md)
— full-suite census (dev, 2026-06-17). 5 serialization round-trip tests NPE
(`getMappingMetamodel`/`getClassLoaderService` on null) because a deserialized
`EntityManager`/`SessionFactory` doesn't reconnect to the live factory. Generic
`readObject`/`readResolve` work on CV (verified); the gap is Hibernate's
`SessionFactoryRegistry.findSessionFactory(uuid,name)` returning null after deser. 🔴 open.

## Hibernate full-suite census (dev 2026-06-17) — additional docs

Per-run bug reports from the (gitignored) `apps/hibernate-orm/cratonvm-bug-reports/dev-run-20260617/`.
(The JSON-function `al_state` SIGSEGV and the reversed stack-trace order were **FIXED** and their docs
removed; the JTA `getInetAddress` per-class report was consolidated into the resolved JTA doc in
[`docs/internal/`](../internal/).)
- [hibernate-hang-clusters-summary.md](../internal/hibernate-hang-clusters-summary.md) — census overview (now in `docs/internal/`); **H1/H2/H3 resolved 2026-06-20**, **H4 root-caused** (its own open doc below).
- [hql-antlr-parser-cold-prediction-throughput.md](hql-antlr-parser-cold-prediction-throughput.md) — 🔴 **OPEN** (census H4, root-caused 2026-06-20). `function.json.JsonArrayUnnestTest` "hang" is the **HQL/ANTLR parser**, not JSON: cold full-context prediction runs interpreted (~1000× HotSpot) because the ATN-simulation hot methods (`closure_`, `closureCheckingStopState`, `mergeArrays`, …) are declined by the single-pass JIT backend (instrumented via `CRATONVM_DBG_JITC`). Terminates (2-item select = 52s), warm re-parse = 0.65s; deferred JIT-backend-coverage cluster. Mitigation: run the suite in one shared JVM.

Also fixed on dev this run (no standalone doc — see commit): `Locale.toLanguageTag()` dropped all subtags for real Locales (`13e8c761`).

- **Hibernate wrong-result assertion failures** — cluster of CV-only wrong-result assertion FAILs (UniqueConstraintBatching 1-vs-0, DetachedBag true-vs-false, EntityGraphBatchSize, immutable+converter deser, …); each likely a separate root cause. 🔴 open (handoff; standalone doc not preserved).

## Hibernate suite residuals (2026-06-24)

Triaged while fixing the collection-delegation native stack overflow (`7b224d8a`:
`try_delegate_real_collection` self-recursion → `EXCEPTION_STACK_OVERFLOW` building a
`SessionFactory`). Once that crash was fixed, three pre-existing CratonVM-only
residuals surfaced (all fail identically at baseline `b0aab8f9`, so none is from the
regression):

- [hib-proxyclassreuse-loader-blind-class-resolution.md](hib-proxyclassreuse-loader-blind-class-resolution.md) —
  🔴 **OPEN.** `ProxyClassReuseTest.testNoReuse`: `CONSTANT_Class` resolution is loader-blind
  (a class constant inside custom-loader bytecode resolves through the flat global/app store, not
  the holder's defining loader), so an isolated loader's `MyEntity` collapses to the app namespace
  and its ByteBuddy proxy collides. loadClass-override isolation itself works; this is deeper
  (core class-store change, broad blast radius). Min repro `.scratch-hhsf/IsoProbe3.java`. Same
  family as **SBR-14** / `SC-custom-classloader`.
- [hib-sortnatural-persistentsortedset-cascade-drop.md](../internal/hibernate-bugs/hib-sortnatural-persistentsortedset-cascade-drop.md) —
  ✅ **FIXED on dev** (`9ac7ef1d`; moved to docs/internal). `SortNaturalTest` (`sorted.set`/`sorted.map`): a cascaded `@OneToMany SortedSet`
  drops an element on **persist** (only 1 of 2 rows inserted; `size()` 2→1). Core `TreeSet` verified
  correct on every access path; the loss is in the Hibernate `PersistentSortedSet` cascade
  interaction. Part of the HIB-CV-35 cvonly long-tail.
- [hib-nodepth-shrinkwrap-par-archive-url.md](../internal/hibernate-bugs/hib-nodepth-shrinkwrap-par-archive-url.md) —
  ✅ **FIXED on dev** (`9258f821`; moved to docs/internal). `NoDepthTests` JPA variants: ShrinkWrap in-memory `.par` archive +
  `ShrinkWrapClassLoader` need a custom `URLStreamHandler` ("Could not create URL for archive");
  the 2 non-JPA variants pass.

## Test-suite repair findings (2026-06-21)

While repairing the in-repo test suites (most failures were stale tests / missing
fixtures / a wrong feature set — all fixed on `dev`), three defects were left open
because each needs a risky core change or a large quality pass. One of the three (the
JIT divide-by-zero re-run) has since been fixed; the other two remain open:

- ✅ **JIT `idiv`/`irem` divide-by-zero re-runs the whole method** (side effects double-execute):
  FIXED on `dev` (direct-throw of `ArithmeticException`, verified vs HotSpot + bt18 soak). The
  historical record moved to [../internal/nested-try-catch-jit-divzero-rerun.md](../internal/nested-try-catch-jit-divzero-rerun.md);
  regression coverage is `test_jit_*_zero_no_double_side_effect` in `vm/tests/exception_tests.rs`.
- **Brooks read-barrier vs CompactHeader forwarding** (`load_and_forward` reads the legacy
  forwarding slot while the test installs the compact one):
  [tier1-brooks-compactheader-forwarding.md](tier1-brooks-compactheader-forwarding.md). Needs a
  maintainer call on which forwarding format the live collector uses before any change.
- **T11 safety-annotation coverage below thresholds**:
  [t11-safety-annotation-coverage.md](t11-safety-annotation-coverage.md). Documentation-only but
  large (~264 cast annotations in interpreter.rs); must be authored accurately, not marker-spammed.

## Consolidation log

- **2026-06-17:** Merged `precise-jit-stack-maps-multithread-fjp-worker-testcase.md`
  (handoff/testcase) and `precise-jit-stack-maps-fork6-findings.md` (findings)
  into a single [fork6-fjp-multithread-jit-root-reclamation.md](fork6-fjp-multithread-jit-root-reclamation.md)
  — they described the *same* Fork6 bug (A4). Added this index framing the
  A1–A4 family + standalone B/C, and recorded the current-`dev` A3 verification.

## Spring Framework full-suite run (2026-06-21) — new entries

A fresh full `spring-core` run (binary built from dev) surfaced these CratonVM
divergences. Consolidated index: [spring-core-suite-2026-06-21.md](spring-core-suite-2026-06-21.md).
**7 fixes from this run already landed on `dev`** (stream close-handler OOB, CHM.remove(null)+
LinkedHashMap.putIfAbsent, generic-bounds reify CCE, keySet.contains delegate, StAX cursor natives,
lambda-SAM param-type dispatch, `String.format("%s",boolean)`, and synthetic `Collector`
supplier/accumulator/finisher/combiner). The **open** ones documented here:

- ~~SC-custom-classloader-ignored.md~~ — ✅ RESOLVED (`Class.forName`/`loadClass` now honor user `ClassLoader`s — override-first redefinition + loader-scoped `findLoadedClass` + non-colliding `defineClass`). Moved to [docs/internal/fixed-suite-bugs/SC-custom-classloader-ignored.md](../internal/fixed-suite-bugs/SC-custom-classloader-ignored.md). Unblocks the annotation `TypeNotPresentException` foundation (Bug A).
- [SC-jspecify-nullness-reflection.md](SC-jspecify-nullness-reflection.md) — type-use + package annotations dropped by reflection (`getTypeAnnotationBytes0` null) — 26 tests.
- ~~SC-annotation-introspection-family.md~~ — Bug A (Class-attr `TypeNotPresentException`) ✅ FIXED — Class members resolve through the declaring class's loader + deferred `TypeNotPresentException`; see [docs/internal/fixed-suite-bugs/SC-custom-classloader-ignored.md](../internal/fixed-suite-bugs/SC-custom-classloader-ignored.md) and `vm/tests/annotation_loader_isolation.rs`. Bug B FIXED on dev; Bug C (Spring scan traversal) / Bug D (HotSpot `getDeclaredMethods` ordering — not cleanly fixable) were the residual. Doc removed in the dev docs refactor.
- [SC-env-classreading.md](SC-env-classreading.md) — getenv/getProperties identity; `Object.equals` shadows overrides (`precedenceOf`=-1); `int.class` via classreading; custom-CL `getResourceAsStream`.
- [SC-resource-io-family.md](SC-resource-io-family.md) — NIO write-channel stub, `Path.toUri()` authority; several FileNotFoundExceptions are harness-CWD artifacts.
- [SC-stax-xml-family.md](SC-stax-xml-family.md) — namespace SAX-event-sequence mismatch (cursor natives + element prefix already fixed on dev).
- [SC-task-retry-util-misc.md](SC-task-retry-util-misc.md) — Properties.store #date line, Throwable deser, retry timing, ByteBuddy ClassInjector, AQS throttle.
- [SC-misc-core-spring.md](SC-misc-core-spring.md) — SortedProperties OutputStream store (CHM.remove(null) fixed on dev).
- [SC-hangs-mergedannotations-charsequence.md](SC-hangs-mergedannotations-charsequence.md) — two hangs: `MergedAnnotations.stream().toArray()` re-entry; Reactor `StepVerifier` producer never scheduled.

Cross-cutting: ByteBuddy `ClassInjector$UsingReflection` failure breaks AssertJ `assertSoftly` + Mockito; JUnit "TimeoutExtension multiple times" masks underlying VM errors.

## Open bug docs relocated from `docs/internal/` (2026-06-22)

Still-**OPEN** bug docs are consolidated here so every unfixed bug lives under
`docs/known-issues/`. (A2/A4 are the `reflrepro-…` / `fork6-…` docs above;
FIXED/resolved bugs stay in `docs/internal/` — they are moved out only when fixed.)

**app-jvm-bugs/**
- [bug-bc-crypto-prng-abnormal-exit-127.md](app-jvm-bugs/bug-bc-crypto-prng-abnormal-exit-127.md)
- [bug-commons-math-full-reactor.md](app-jvm-bugs/bug-commons-math-full-reactor.md)
- [bug-commons-math-junit-probe-jit-execute.md](app-jvm-bugs/bug-commons-math-junit-probe-jit-execute.md)
- [bug-elasticsearch-log4j2-serviceloader.md](app-jvm-bugs/bug-elasticsearch-log4j2-serviceloader.md)
- [bug-gpu-build-native-builtins-crash.md](app-jvm-bugs/bug-gpu-build-native-builtins-crash.md)
- [bug-hibernate-duplicate-persistence-unit-scan.md](app-jvm-bugs/bug-hibernate-duplicate-persistence-unit-scan.md)
- [bug-hibernate-jpa-persistence-xml-properties.md](app-jvm-bugs/bug-hibernate-jpa-persistence-xml-properties.md)
- [bug-hibernate-log-format-placeholder.md](app-jvm-bugs/bug-hibernate-log-format-placeholder.md)
- [bug-wildfly-jaxp-premature-end-of-file.md](app-jvm-bugs/bug-wildfly-jaxp-premature-end-of-file.md)
- [bug-wildfly-msc-service-start-callback.md](app-jvm-bugs/bug-wildfly-msc-service-start-callback.md)
- [bug-wildfly-throwable-stack-trace-capture.md](app-jvm-bugs/bug-wildfly-throwable-stack-trace-capture.md)

**gaps/**
- [gap-bc-math-ec-crypto-regression-timeout.md](gaps/gap-bc-math-ec-crypto-regression-timeout.md)
- [gap-jit-fastmath-transform-miscompile.md](gaps/gap-jit-fastmath-transform-miscompile.md)

**h2-suite-bugs/**
- [bug-h2-charset-cp500-unsupported.md](h2-suite-bugs/bug-h2-charset-cp500-unsupported.md)
- [bug-h2-inprocess-javac-resource-bundle.md](h2-suite-bugs/bug-h2-inprocess-javac-resource-bundle.md)
- [bug-h2-mvstore-insert-loop-perf-hang.md](h2-suite-bugs/bug-h2-mvstore-insert-loop-perf-hang.md)
- [bug-h2-netutils-missing-pbe-algparams.md](h2-suite-bugs/bug-h2-netutils-missing-pbe-algparams.md)
- [bug-h2-timezone-dst-offset.md](h2-suite-bugs/bug-h2-timezone-dst-offset.md)

**keycloak-crash-reports/**
- [06-keypair-verifier-decode.md](keycloak-crash-reports/06-keypair-verifier-decode.md)
- [08-stripsecrets-json-comparison.md](keycloak-crash-reports/08-stripsecrets-json-comparison.md)
- [09-streamsutil-onclose-propagation.md](keycloak-crash-reports/09-streamsutil-onclose-propagation.md)
- [10-jwksutils-one-failure.md](keycloak-crash-reports/10-jwksutils-one-failure.md)

**tomcat-suite-bugs/**
- [04-embedded-server-throughput-wall-OPEN.md](tomcat-suite-bugs/04-embedded-server-throughput-wall-OPEN.md)
- [05-suite-rerun-fail-triage.md](tomcat-suite-bugs/05-suite-rerun-fail-triage.md)

**wildfly-suite-bugs/**
- [bug-06b-jit-scan-cache-unsound.md](wildfly-suite-bugs/bug-06b-jit-scan-cache-unsound.md)

## Spring Boot runner-probe sweep (2026-06-22)

A 103-probe sweep of `apps/spring-boot/buildSrc/runner/` (each a standalone
`main()`) under a fresh dev build (`cvsbfull.exe`, worktree `CratonVM-sbfull`) vs
HotSpot jdk-25 found **14 CratonVM-only bugs** (0 crash, 11 hang, 19 real DIFF).
Index + per-bug reports: [spring-boot-probe-sweep/INDEX.md](spring-boot-probe-sweep/INDEX.md).

**3 FIXED + merged to `dev`** (writeups in [`docs/internal/`](../internal/)):
- **SBR-03** (`a245002c`) strict array `instanceof` ([writeup](../internal/SBR-03-array-interface-instanceof.md)) — `Object[] instanceof I[]` → `false`.
- **SBR-06** (`bce39db7`) Constructor `Parameter.getParameterizedType()` generics
  ([writeup](../internal/SBR-06-field-getgenerictype-raw.md)) — populate the Constructor mirror's `signature` field.
- **SBR-02** (`0d7dfc28`, merge `01375f90`) regex `replaceAll` / literal `replace` throughput wall
  ([writeup](../internal/SBR-02-string-regex-throughput.md)) — flip `CRATONVM_NATIVE_STRING_REGEX`
  default-ON; `String.{replaceAll,replaceFirst,matches,replace(CharSequence,…)}` route to fast
  Rust-regex natives. Was the `PluginXmlParserTests` hang.

**Open** (root-caused; none a safe one-liner — see each report):
- **SBR-01** Groovy `parseClass` hang ×9 — overlaps [spring-boot-buildsrc-coldpath-hangs](spring-boot-buildsrc-coldpath-hangs-2026-06-22.md). **handoff**
- **SBR-14** custom `URLClassLoader(parent=null)` bypassed → `AppClassLoader` (classloader isolation). **handoff**
- **SBR-04** annotation `getClass()`/`toString`; **SBR-05** `getDeclaredMethods` order (won't-fix, spec-unspecified);
  **SBR-07** `getSimpleName` (real defect = `getDeclaringClass0`/InnerClasses for Kotlin classes);
  **SBR-08/09/10/11/13** object-identity cluster (CV synthesizes JDK objects as abstract/base-typed —
  jar conn, NIO FS, IntStream, MethodHandle, ProtectionDomain); **SBR-12** `cratonvm.internal.UnmodifiableList`
  name leak (needs real `ImmutableCollections` or a guarded alias).

