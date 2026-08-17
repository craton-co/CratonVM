# Feature designs — status index

One document per large feature, each stating **what the feature does today**
before it explains why it is built that way. These are current-state docs, not
a roadmap: if a doc says a thing is shipped, the code says so too, and if it
says a thing is not built, that is a claim about the tree as it stands.

Status vocabulary:

| Term | Meaning |
|---|---|
| **Shipped (default on)** | built, and active with no configuration |
| **Shipped (opt-in)** | built, correct as far as it is soaked, but off unless a flag is set |
| **Partial** | some of it is live; the doc names what is missing |
| **Designed, not built** | analysis and a plan, no implementation |
| **Not planned** | scoped, then declined for a stated reason |

Environment flags are declared centrally in `types/src/flags.rs` and
`types/src/flag_groups.rs`, but a flag's **default is decided at its read
site** — usually `vm/src/runtime/env_cache.rs` for VM-side accessors, or the
owning module for jit-side ones. `docs/CONFIG.md` is the generated inventory.

## Compiler and JIT

| Feature | Status | What it does today |
|---|---|---|
| [IR optimizer](activate-ir-optimizer.md) | Shipped (default on) | GVN, const-fold, DSE, DCE, LICM and unrolling over a Sea-of-Nodes IR, with escape analysis → scalar replacement, and complete long and float/double value tiers. |
| [Tiered compilation manager](wire-tiered-manager.md) | Shipped (default on) | Compilation runs on a background `cratonvm-jit-compiler` thread; the mutator interprets until the worker publishes. `CRATONVM_BG_COMPILE=0` restores inline compilation. |
| [Real-frame deoptimization](real-frame-deopt.md) | Shipped (default on) | A failed guard resumes the interpreter at the trapping bci instead of re-running the whole method. |
| [Deopt + precise OSR (combined design)](deopt-osr.md) | Shipped (default on) | The shared per-pc state map, virtual-object re-materialization, de-speculation and cat-2/FP resume. |
| [OSR entry metadata](jit-osr-entry-metadata.md) | Shipped (always on) | A publication-time check refuses OSR for a method whose metadata vectors disagree across coordinate spaces. |
| [OSR exit and recompile](jit-osr-exit-and-recompile.md) | Shipped (default on) | Per-pc compile memo, exit-site classification, and ungated lifecycle counters — one of which is declared but never recorded. |
| [Precise JIT stack maps](precise-jit-maps-default.md) | Shipped (default on) | The collector gets a precise oop description per compiled frame. `CRATONVM_NO_PRECISE_JIT_MAPS` opts out; the old `CRATONVM_PRECISE_JIT_MAPS` is a no-op. |
| [JIT local exception handlers](jit-local-exception-handlers.md) | Shipped (default on) | Methods combining `athrow` with a local exception table compile, behind a compile-time dataflow safety check. |
| [Profile-guided inlining](profile-guided-inlining.md) | Shipped (opt-in via `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE`) | Monomorphic and bimorphic virtual/interface sites inlined behind receiver class-id guards. Doubly gated: profile recording needs `CRATONVM_TIER_PGO`. |
| [Machine level and instruction selection](jit-machine-level-and-instruction-selection.md) | Partial | The encoder level is built and byte-anchored; the machine list is not, and every emitting mode is default-off. |

## Garbage collection

| Feature | Status | What it does today |
|---|---|---|
| [Moving young generation](default-moving-young-gen.md) | Shipped (default on); remaining scope **not planned** | Semispace Cheney copy, fail-closed to a non-moving sweep when per-cycle root coverage is unproven. The throughput premise it was scoped against was measured and refuted. |
| [Compact reference-field layout](compact-ref-field-layout.md) | Shipped (default on) | Reference instance fields are bare 8-byte pointers, not 16-byte tagged cells. |
| [Concurrent garbage collection](concurrent-gc-maturation.md) | Partial | Concurrent old-gen mark and sweep run by default on the Generational collector; G1 is opt-in and has a real marker thread; ZGC has neither. |
| [Production ZGC](zgc-production-implementation-plan.md) | Partial | The default collector. **Concurrent marking landed 2026-08-16, opt-in** (`CRATONVM_ZGC_CONC_START=60`; pause -38..-58%, throughput -37..-55%) and compaction on 2026-08-13; the sweep is still stop-the-world, marking is snapshot-at-the-beginning rather than ZGC's load barrier, and it is not generational. See [the concurrent+generational plan](zgc-concurrent-and-generational-plan-20260813.md). |
| [ZGC JIT load barrier](zgc-jit-load-barrier.md) | Designed, not built | Correctness argument, site inventory and cost model. No barrier emission exists in the JIT. |
| [ZGC reference-slot representation](zgc-reference-slot-representation.md) | Designed, not built | What it would cost to give the barrier a CAS-able slot. Reference slots are plain pointers today. |
| [Native root handles](native-handle-discipline.md) | Shipped (default on), unenforced | A scoped handle API with ~405 adoption sites. Nothing fails a build when native code holds a raw `ObjectRef` across an allocating call. |

## Embedding and runtime services

| Feature | Status | What it does today |
|---|---|---|
| [`libcratonvm` embedding API](embedding-api.md) | Shipped (default on) | `cdylib`/`staticlib` exposing the JNI Invocation API, a flat `cratonvm_*` C ABI, and a curated Rust facade. |
| [Foreign-thread attach](foreign-thread-attach.md) | Shipped (default on) | A host-created OS thread becomes a GC-safe Java thread that participates in stop-the-world. |
| [JEP 358 helpful NPE messages](jep358-helpful-npe.md) | Shipped for the CLI; partial for embedders | Backward expression reconstruction producing HotSpot's `because "x" is null`. An embedder that never wires the flag gets the legacy strings. |
| [JVMTI event delivery](jvmti-delivery-threading.md) | Partial | Every interpreter delivery site is VM-attributed. There is no C `jvmtiEnv` function table, and `GetEnv` returns a `JNIEnv` for any requested version. |
| [KeyStore, ML-DSA and ML-KEM](keystore-mldsa-mlkem.md) | Shipped (default on), real-JDK mode only | Real PKCS#12 and JKS; post-quantum algorithms route to the JDK's own implementations, so there is no native lattice crypto. |

## Class loading, natives and modes

| Feature | Status | What it does today |
|---|---|---|
| [JDK-only mode](jdk-only-mode.md) | Partial | `--jdk-only` is a policy orthogonal to `JdkMode`; it refuses synthetic stubs and class fabrication, but the two large hard-coded dispatch lists survive, bypassed rather than removed. |
| [Collection base-class interception](collections-interception.md) | Shipped (default on) | Natives on `AbstractCollection`/`AbstractSet` and friends are registered in the default real-JDK build, not only under the `synthetic-jdk` feature. |
| [Fallible synthetic-class fabrication](synthetic-class-fallibility.md) | Partial | The fallible spelling exists end to end; 32 native call sites still use the infallible one against 15 that do not. |
| [`get_field_by_name` descriptor awareness](by-name-field-reads.md) | Partial | The accessor is still not descriptor-aware; ~22 high-consequence call sites are mitigated through a descriptor-safe reader. |

## Correctness infrastructure

| Feature | Status | What it does today |
|---|---|---|
| [Semantic differential fuzzer](differential-fuzzer.md) | Shipped (default on) | `cratonvm-difftest` diffs observable behaviour against a real JDK. Its CI gate is blocking. |
| [Fuzzing harness](fuzzing-state.md) | Partial | Seventeen libfuzzer targets, all reaching real parsers, all built by CI on every commit — and never executed. |
| [Class-file parser hardening](class-file-parser-hardening.md) | Shipped (default on) | Parsing is fully fallible, every attacker-controlled length is bounded by bytes remaining, and no production path panics. |

## Where the other material went

Audits, censuses, campaign plans and per-increment delivery logs are not
current-state documents and are not kept here. Retired write-ups that are still
worth reading for provenance live under the internal documentation tree; a
public document should inline the durable fact rather than link to one.
