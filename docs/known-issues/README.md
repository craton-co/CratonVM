# Known issues — index & bug map

This folder collects CratonVM-only defects found while running upstream Java
suites. The docs had grown to describe the **same underlying bug from several
angles**; this index is the consolidated map. Read it first.

## How many distinct bugs are here?

Four of the docs are all manifestations of **one root-cause family** — the
*GC-root-coverage-under-JIT* family — plus **two standalone bugs**. Net: **2 open
GC-root manifestations, 1 open standalone bug, 1 latent standalone bug** (the
other GC-root manifestations are already fixed on `dev`).

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
| **A2** | **Register-resident / above-band missed root** — a reflection-result oop returns in a register (or spills *above* the per-JIT-entry scan band) and is reclaimed by the non-moving sweep | `wildfly-suite/repro/ReflRepro` | 🔴 **OPEN** | [reflrepro-register-resident-jit-root-handoff.md](reflrepro-register-resident-jit-root-handoff.md) |
| **A3** | **Register-invisibility** — a live oop sits only in a CPU register at a young-GC safepoint, invisible to the stack-only scan (single thread) | `apps/spring-boot/buildSrc/runner/MinRegexProbe` | 🔴 **OPEN** | [SB-SUITE-CRASH-04-jit-inline-new-heap-corruption.md](SB-SUITE-CRASH-04-jit-inline-new-heap-corruption.md) |
| **A4** | Multi-thread: live `ForkJoinTask`s reclaimed under **FJP worker threads** + a **lost-tag** interpreter local; amplified by selective-promotion evacuation | `scratch/xworker/Fork6` (needs `CRATONVM_REAL_FORKJOINPOOL=1`) | 🟡 **OPEN** (partial fix on branch `fix/multithread-jit-roots-stw`) | [fork6-fjp-multithread-jit-root-reclamation.md](fork6-fjp-multithread-jit-root-reclamation.md) |

Only **A1 is fixed**; **A2, A3, and A4 are the same open bug** — the non-moving /
selective-promotion young sweep marks from a root set that omits register-resident
(and above-`entry_sp`) JIT oops. They all want the same fix: precise (or
register-aware) JIT roots. A3 (`MinRegexProbe`) and A2 (`ReflRepro`) are the
cleanest deterministic repros; A4 extends it across threads.

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

## The springrepos handoff (mostly fixed)

[springrepos-extension-hang-jit-throughput-and-deep-recursion.md](springrepos-extension-hang-jit-throughput-and-deep-recursion.md)
is a multi-defect handoff. `dev` now **passes** `SpringRepositoriesExtensionTests`.
Of its decomposed defects: the `SecureClassLoader.pdcache` NPE, the
`AssertionError`-preload JIT bail, the `hashCode`/`equals`-override JIT compile,
and the root-snapshot hang are all **fixed on dev**. Its only still-relevant open
items are **its "defect #2" (= family A3 above)** and **bug C** (the cold-path
deep-recursion overflow). It is kept for that context and the deep-recursion
stack-guard design.

## Consolidation log

- **2026-06-17:** Merged `precise-jit-stack-maps-multithread-fjp-worker-testcase.md`
  (handoff/testcase) and `precise-jit-stack-maps-fork6-findings.md` (findings)
  into a single [fork6-fjp-multithread-jit-root-reclamation.md](fork6-fjp-multithread-jit-root-reclamation.md)
  — they described the *same* Fork6 bug (A4). Added this index framing the
  A1–A4 family + standalone B/C, and recorded the current-`dev` A3 verification.
