# CratonVM Documentation

The canonical manual is the [CratonVM book](book/src/SUMMARY.md). It is
readable directly as Markdown and can be rendered with mdBook:

```bash
mdbook serve docs/book
```

This index is organised by the question you arrived with. Everything listed
here describes CratonVM **as it is now**. Forward-looking proposals live under
[`feature-designs/`](feature-designs/README.md) and are not evidence that a
feature exists; unresolved defects live under
[`known-issues/`](known-issues/).

---

## What is CratonVM?

- [Introduction](book/src/introduction.md) — what the project is and is not.
- [One-page overview](PRESENTATION.md) — the positioning summary.
- [`../ARCHITECTURE.md`](../ARCHITECTURE.md) — the 22 workspace crates, the
  subsystem map, and how to tell whether a feature actually runs on the default
  path.
- [Architecture Overview](book/src/internals/architecture.md) — the concise
  manual version of the same map.
- [`../ROADMAP.md`](../ROADMAP.md) — what is planned next.
- [legal.md](legal.md) — licensing and compliance terminology. CratonVM is not
  JCK-certified.

## How do I install and run it?

- [Installation](INSTALL.md) · [manual chapter](book/src/getting-started/installation.md)
- [Your First Program](book/src/getting-started/first-program.md)
- [Running Programs](book/src/user-guide/running-programs.md)
- [Command-Line Reference](book/src/user-guide/cli-reference.md)
- [Containers and cgroups](CONTAINER.md) · [manual chapter](book/src/user-guide/containers.md)
- [Platform Support](PLATFORMS.md) · [support matrix](book/src/reference/platform-support.md)
- [Troubleshooting](TROUBLESHOOTING.md) · [manual chapter](book/src/user-guide/troubleshooting.md)
- [FAQ](book/src/reference/faq.md) · [Glossary](book/src/reference/glossary.md)

## How do I configure and tune it?

- [Configuration reference](CONFIG.md) — the fifteen user-facing environment
  variables: ten that take a comma-separated token list, plus five scalars.
- [Flag tokens](flag-tokens.md) — every token each grouped variable accepts.
- [Flag inventory](config/flag-inventory.md) — how flag reads are centralised
  and what enforces the surface. The definition lives in
  [`../types/src/flag_groups.rs`](../types/src/flag_groups.rs);
  `types/tests/flag_surface.rs` and `tools/flag-census/check-surface.sh` fail
  the build if a read site appears without a token, or if the reference docs
  name a token that does not exist.
- [Environment variables](book/src/reference/environment-variables.md)
- [Memory and Garbage Collection](book/src/user-guide/memory-and-gc.md) ·
  [GC tuning](gc-tuning.md)
- [JIT compiler options](book/src/user-guide/jit-compiler.md)
- [Modules and `--add-opens`](book/src/user-guide/modules.md)
- [Performance tuning](book/src/performance/tuning.md)

## How do I operate it in production-like settings?

- [Deployment and Operations](book/src/operations/deployment.md)
- [Observability](book/src/operations/observability.md) ·
  [phase accounting](observability/phase-accounting.md) ·
  [`cratonvm.JitCompileDecision`](observability/jit-compile-decision.md) — the
  JIT's per-method admission verdict (which door asked, which backend produced
  the body, and the reason it was admitted or declined) as a JFR event, so
  "why is this method slow?" can be answered from a recording instead of a
  rebuild with a debug flag set.
- [Incident Response](book/src/operations/incident-response.md)
- [Profiling](PROFILING.md) · [manual chapter](book/src/performance/profiling.md)
- [Debugging](book/src/user-guide/debugging.md)

## What Java is supported?

- [Compatibility and Support Policy](book/src/reference/compatibility-policy.md)
- [Java Version Support](book/src/java-support/version-support.md)
- [Language Features](book/src/java-support/language-features.md)
- [Standard Library Coverage](book/src/java-support/standard-library.md)
- [JDK Coverage Inventory](JDK_COVERAGE.md)
- [Synthetic method inventory](synthetic_methods.md)
- [Real-JDK mode design and contract](feature-designs/real-jdk-mode.md) —
  the normative contract and change policy for Real-JDK execution.
- [JCK engineering status](jck-compliance.md)
- [JavaFX status](javafx-status.md)

## How fast is it, and how is that measured?

- [`../BENCHMARK.md`](../BENCHMARK.md) — result tables and the house standard.
- [Benchmarking methodology](benchmarking/methodology.md)
- [Benchmark reliability gate](benchmarking/reliability-gate.md)
- [Benchmarks](book/src/performance/benchmarks.md)
- [JIT compiler internals and optimization](book/src/performance/jit-internals.md)
- [JIT optimization reference](JIT_OPTIMIZATION.md)
- [Framework throughput program](framework-throughput.md)
- [Real-JDK mode benchmarks](benchmarking/real-jdk.md) — non-regression budgets
  for `--real-jdk`. No baseline has been captured, so every number there is an
  engineering gate rather than a measurement.

## How does it work inside?

Manual chapters:

- [Runtime Lifecycle](book/src/internals/runtime-lifecycle.md) ·
  [Runtime Contracts](book/src/internals/runtime-contracts.md)
- [Interpreter](book/src/internals/interpreter.md) ·
  [JIT Compiler](book/src/internals/jit.md) ·
  [Garbage Collector](book/src/internals/garbage-collector.md)
- [Class Loading and Verification](book/src/internals/class-loading.md) ·
  [Threading](book/src/internals/threading.md) ·
  [Native Methods](book/src/internals/native-methods.md)

Focused notes on current architecture ([`architecture/`](architecture/)):

- [Compact object and field layout](architecture/compact-object-and-field-layout.md)
- [Inline allocation and reference publication](architecture/inline-allocation-and-reference-publication.md)
- [Class-loader unloading](architecture/class-loader-unloading.md)
- [Member resolution](architecture/member-resolution.md)
- [Mapped JAR and shared class bytes](architecture/mapped-jar-shared-class-bytes.md)
- [Continuation-backed virtual threads](architecture/continuation-backed-virtual-threads.md)
- [Per-VM state versus process-global state](architecture/per-vm-state.md)
- [JIT safepoint polls](architecture/jit-safepoint-polls.md)
- [JIT cache sharding and code reclamation](architecture/jit-cache-sharding-and-code-reclamation.md)
- [Register allocation, recursion, and inlining](architecture/register-allocation-recursion-and-inlining.md)
- [Shared verified-code IR](architecture/shared-verified-code-ir.md)
- [Natives over real JDK classes](architecture/natives-over-real-jdk-classes.md)
  — how a registered native comes to run instead of real JDK bytecode, why the
  Cargo feature is not the runtime mode, `register()`'s last-wins semantics,
  what a by-name field read cannot tell you, and what the registration censuses
  can and cannot see. Read this before diagnosing "why did/didn't my native
  run".
- [Native target-method metadata](architecture/native-target-method-metadata.md)

Garbage collection: [GC architecture and current state](GC.md).

JIT subsystem references ([`jit/`](jit/)) cover the compilation broker, code
cache lifecycle, deopt metadata, the [helper ABI contract](jit/helper-abi.md),
register allocation, loop transforms, OSR, escape and alias analysis,
vectorization, and AArch64 parity.

Threading contracts: [`ObjectRef` concurrency contract](threading/objectref-concurrency-contract.md)
and [thread transition states](threading/thread-transition-states.md). The
canonical in-source lock hierarchy is
[`../vm/src/runtime/lock_order.rs`](../vm/src/runtime/lock_order.rs).

## What about security?

- [Security Overview](book/src/security/overview.md) ·
  [Sandboxing and Hardening](book/src/security/sandboxing.md)
- [Security hardening reference](SECURITY_HARDENING.md)
- [Cryptography](book/src/security/cryptography.md) ·
  [Cryptographic algorithm status](CRYPTO_STATUS.md) ·
  [Cryptographic failure contract](security/crypto-failure-contract.md)
- [Native and foreign capability gating](security/native-capabilities.md) ·
  [capability wiring](security/capability-wiring.md) ·
  [runtime install and teardown](security/capability-runtime-install.md)
- [Per-VM SecurityManager and Policy state](security/per-vm-security-state.md)
- [Signed-JAR trust boundary](security/signed-jar-trust.md)
- [`reader` resource limits](security/reader/limits.md)
- [Bytecode verifier coverage and residual attacker capability](security/verifier/coverage.md)
- [Real-JDK mode threat model](security/real-jdk-threat-model.md) — advisory;
  `--real-jdk` narrows *which class implementations may execute* and creates no
  isolation boundary.
- [`../SECURITY.md`](../SECURITY.md) — vulnerability reporting policy.

CratonVM is not a security boundary for hostile Java code.

## How do I embed it?

- [Embedding Overview](book/src/embedding/overview.md)
- [C ABI / JNI Invocation API](book/src/embedding/c-abi.md)
- [Rust Embedding Facade](book/src/embedding/rust-facade.md)
- [Standalone embedding reference](EMBEDDING.md)
- [Hosting the VM crate](EMBEDDING_VM_CRATE.md)

## Can I run Java on the GPU?

GPU execution is opt-in and requires a `gpu-driver` build. It is CUDA/NVIDIA
only, and eligibility is a deliberately narrow, documented subset.

- [GPU Offload Overview](book/src/gpu/overview.md) ·
  [GPU Benchmarks](book/src/gpu/benchmarks.md)
- [GPU reference](gpu/README.md) — the source of truth for eligibility, driver
  requirements, and experimental status.
- [Comparison with other Java GPU paths](gpu/COMPARISON.md)

## What is `--real-jdk` mode?

By foundational architecture, CratonVM runs authentic OpenJDK class libraries directly,
treating real JDK bytecode as authoritative. **The cratonvm launcher defaults to Real-JDK mode (`--real-jdk`).**
Under `--real-jdk`, real JDK class bytes are authoritative — no class is fabricated without real
bytecode, and synthetic stubs are strictly rejected.

Fast-boot mode (**`--fast-boot`**, alias `--compatible`) was engineered as an
optimization layer for fast cold-start loading and specialized throughput execution paths, utilizing
synthetic stubs to accelerate startup and bypass complex initialization paths.
Every *embedding* entry point — `libcratonvm`, `cratonvm-embed`, `VmConfig::default` — defaults
to `compatible` deliberately, providing immediate startup and throughput acceleration to host
applications without requiring CLI flags.

- [Design contract and operator guide](feature-designs/real-jdk-mode.md) — the normative interface contract and operator guide.
- [Native promotion review](feature-designs/native-promotion-review.md) — the checklist a
  `SyntheticStub` must pass to become a reviewed `Bridge` or `Intrinsic`.
- [Threat model](security/real-jdk-threat-model.md)
- [Benchmarks and non-regression budgets](benchmarking/real-jdk.md)
- [Open issues](known-issues/) — gaps deliberately deferred
  rather than papered over, with the evidence that makes them actionable.
- [Manual chapter](book/src/user-guide/real-jdk-mode.md)

The native kind a registration carries is **ambient**, not per-call:
`NativeMethodRegistry::register` takes no kind argument, and the registry's
current category defaults to `SyntheticStub`. A `register()` call outside a
`with_category(...)` block is a synthetic stub *by omission*, with no syntactic
marker — so a source grep undercounts by construction and only the booted
registry can answer. `scripts/jdk-only-census.sh` drives the runtime census and
CI publishes its dumps under `target/jdk-only-audit/`.

## What are the known gaps?

- [Known Limitations](book/src/java-support/limitations.md) — the manual's list.
- [`known-issues/`](known-issues/) — unresolved bugs and active
  investigations, with reproducers.
- [Differential testing](testing/differential.md) — what is compared, and how
  to trust the answer. Known divergences are tracked under known issues. ·
  [opcode coverage](testing/opcode-coverage.md)
- [`--diff-hotspot`](testing/diff-hotspot.md) — the single-program door to the
  same comparison: run *your* class or JAR under CratonVM and a reference JDK
  and be told the first divergence, or that there is none. `differential.md`
  covers the corpus-and-CI door; this is the one you point at your own code.
- [Code coverage](COVERAGE.md) — the project does not claim a minimum
  percentage until a reproducible baseline has been measured.
- [Release readiness](RELEASE_READINESS.md) — the evidence a release candidate
  must carry, and the claims a green checklist explicitly does *not* establish.

## How do I contribute?

- [`../CONTRIBUTING.md`](../CONTRIBUTING.md) · [`../BUILD_GUIDE.md`](../BUILD_GUIDE.md)
- [Building](book/src/contributing/building.md) ·
  [Testing](book/src/contributing/testing.md) ·
  [Documentation guide](book/src/contributing/documentation.md)
- [CI evidence](ci/evidence.md) — what each workflow proves, and what a green
  run does *not* prove.
- [`../RELEASING.md`](../RELEASING.md)

Validate maintained Markdown and mdBook membership with:

```bash
python3 tools/check_markdown_links.py
```

`--all` additionally checks untracked public Markdown. Private archives and
vendored trees are outside the documentation gate.

## Where does a document belong?

- `docs/known-issues/` — unresolved bugs and active investigations.
- `docs/architecture/`, `docs/jit/`, `docs/security/`, `docs/threading/`,
  `docs/gpu/`, and the top-level `docs/*.md` files — **current state only**.
  They answer "what does the VM do today", not "how did we get here".
- `docs/feature-designs/` — current feature contracts and explicit implementation limits.
  Proposals belong in the local internal archive.
- Reviews, audits, fixed-bug write-ups, completed plans, and dated
  investigations are archived out of the public set once the work lands. Do not
  link a public document to archived material; inline the durable fact instead.

For current project status use [`../ROADMAP.md`](../ROADMAP.md), the
[compatibility policy](book/src/reference/compatibility-policy.md), and the
[known limitations](book/src/java-support/limitations.md).
