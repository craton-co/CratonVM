# Feature Designs — XL Strategic Roadmap

Actionable design / implementation-plan docs for the XL features that are too
large to land in a single pass. Each is grounded in the current implementation
(cited `file:line`) and structured as: **Goal · Current state · Design ·
Implementation steps (ordered) · Risks · Effort**.

These are roadmap docs, not specs of shipped behavior. They capture *what to
build and in what order*, including the cross-feature dependencies below.

## The docs

| Doc | Feature | Effort | Key dependency |
|---|---|---|---|
| [`real-frame-deopt.md`](real-frame-deopt.md) | **Keystone.** Materialize the interpreter frame at the trapping bci from JIT register/stack state (vs. today's whole-method re-run). | XL | — (gates the rest) |
| [`default-moving-young-gen.md`](default-moving-young-gen.md) | Make a moving/compacting young gen the default to close the bt18 ~23x gap (invariant: checksum = 68332206). | XL | precise JIT roots / deopt maps |
| [`wire-tiered-manager.md`](wire-tiered-manager.md) | Turn the dormant tiered policy + queue into a real C1+C2 pipeline with a background compile thread and OSR. | L | OSR precision ← deopt |
| [`activate-ir-optimizer.md`](activate-ir-optimizer.md) | **Largely landed (inc 1–29).** GVN/fold/DSE/LICM + escape→scalar-replacement broad; φ/branch dam fixed; `Op::Load`/`Store`/`New`/`Call` + a full **long (64-bit) value tier** built & **default-ON** (`IR_CALL`/`SCALAR_NEW`/`IR_LONG`/`IR_CALL_SPECIAL`). **Remaining:** long/double call *returns* (`i64::MIN`-sentinel), `ldiv`/`lrem` (long deopt-resume), `double`/`float` XMM tier — see the doc's "Remaining roadmap (post-inc-29)". | L | guard-surviving SR ← deopt |
| [`real-cdi-bean-container.md`](../internal/fixed-suite-bugs/real-cdi-bean-container.md) | Retire the per-framework shim cluster (ArC/Spring/WildFly/MSC/Infinispan/Agroal) with real bytecode. | XL | general `<clinit>`/classloading fixes |
| [`jep358-helpful-npe.md`](jep358-helpful-npe.md) | Helpful NPE messages via bci-context analysis + `getExtendedNPEMessage`. | M | JIT-NPE parity ← deopt |
| [`proxy-real-classfile.md`](../internal/fixed-suite-bugs/proxy-real-classfile.md) | Generate a real `$ProxyN` class file (vs. name-lookup synthetic shim). | M | runtime defineClass (WP2.3) |
| [`jit-osr-exit-and-recompile.md`](jit-osr-exit-and-recompile.md) | **Livelock memo and visibility gap closed.** The per-pc compile memo was already built; OSR lifecycle counters (`osr_entered`/`osr_exited`/`osr_refused_entry`/`osr_compile_declined`) are now ungated, because a silent exit is otherwise indistinguishable from never having entered. Remaining: the exit-state differential — an OSR bail resuming at the wrong state re-runs loop iterations, a wrong-answer bug no termination test sees. | M (remaining) | — |
| [`jit-osr-entry-metadata.md`](jit-osr-entry-metadata.md) | **Contract executable and enforced.** OSR metadata spans **three** coordinate spaces (interpreter bci / output pc / local index); a publication-time check refuses OSR for the method when the vectors disagree, because a short dead mask reads as "safe" through `unwrap_or(0)`. Remaining: one door for "produce an OSR-capable artifact" — the OSR path still calls `x64::compile` directly. | M (remaining) | — |
| [`jit-machine-level-and-instruction-selection.md`](jit-machine-level-and-instruction-selection.md) | **Increment 0 landed; 1–4 on hold, and the hold is the result.** Four compiler levels, not three; the missing one is a machine list. Shadow selection measured **15.7–19.0%** tiler coverage on real compiles with `Rule::Lea`/`AluImm` firing zero times, so the next step is six 32-bit pattern rows — not a machine level. | S (then L, gated) | — |
| [`embedding-api.md`](embedding-api.md) | `libcratonvm` C-ABI + JNI Invocation-API parity (`JNI_CreateJavaVM`). | L | — |
| [`keystore-mldsa-mlkem.md`](keystore-mldsa-mlkem.md) | `KeyStore.getInstance` PKCS12/JKS + route ML-DSA/ML-KEM to a real provider. | M | — |

## Dependency note

`real-frame-deopt.md` is the **keystone**: precise per-safepoint register→slot
maps unlock (a) safe default moving GC, (b) precise OSR in the tiered manager,
(c) guard-surviving scalar replacement in the IR optimizer, and (d) precise
JIT-thrown-NPE bci context. The others (`real-cdi-bean-container`,
`embedding-api`, `proxy-real-classfile`, `keystore-mldsa-mlkem`) are independent
and can proceed in parallel.
