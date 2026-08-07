# CratonVM Documentation

The canonical manual is the [CratonVM book](book/src/SUMMARY.md). It is
readable directly as Markdown and can be rendered with mdBook:

```bash
mdbook serve docs/book
```

The manual covers installation, everyday use, Java compatibility, security,
performance, operations, GPU offload, embedding, architecture, contributing,
and reference material. Root and standalone documents provide deeper evidence
and large reference tables without duplicating the manual.

## Start here

- [Introduction](book/src/introduction.md)
- [Installation](book/src/getting-started/installation.md)
- [Your First Program](book/src/getting-started/first-program.md)
- [Running Programs](book/src/user-guide/running-programs.md)
- [Command-Line Reference](book/src/user-guide/cli-reference.md)
- [Configuration](book/src/user-guide/configuration.md)
- [Troubleshooting](book/src/user-guide/troubleshooting.md)
- [FAQ](book/src/reference/faq.md)

## Operating CratonVM

- [Deployment and Operations](book/src/operations/deployment.md) — packaging,
  immutable configuration, sizing, rollout, rollback, and operational
  checklists.
- [Observability](book/src/operations/observability.md) — logs, JFR, stack
  dumps, native coverage audits, and incident evidence.
- [Incident Response](book/src/operations/incident-response.md) — repeatable
  crash, hang, wrong-result, OOM, compatibility, and regression triage.
- [Containers and cgroups](book/src/user-guide/containers.md)
- [Memory and Garbage Collection](book/src/user-guide/memory-and-gc.md)
- [Security Overview](book/src/security/overview.md)
- [Sandboxing and Hardening](book/src/security/sandboxing.md)

## Performance

- [Performance Tuning](book/src/performance/tuning.md) — the recommended
  correctness-first tuning workflow and current runtime fast paths.
- [Benchmarks](book/src/performance/benchmarks.md) — manual overview.
- [`../BENCHMARK.md`](../BENCHMARK.md) — detailed methodology, current result
  tables, raw-evidence expectations, and performance gate.
- [Profiling](book/src/performance/profiling.md)
- [How the JIT Got Fast](book/src/performance/jit-internals.md)
- [JIT Optimization History](JIT_OPTIMIZATION.md)
- [GC Tuning](gc-tuning.md)
- [Moving-young throughput status](moving-young-throughput.md)
- [Framework throughput program](framework-throughput.md)
- [JDK-only mode benchmarks](benchmarking/jdk-only.md) — proposed non-regression
  budgets for `--jdk-only`; no baseline captured yet, so every number there is
  an engineering gate rather than a measurement.

## Architecture and internals

- [`../ARCHITECTURE.md`](../ARCHITECTURE.md) — deep crate and subsystem
  orientation.
- [Architecture Overview](book/src/internals/architecture.md) — concise manual
  map.
- [Runtime Lifecycle](book/src/internals/runtime-lifecycle.md) — launcher,
  typed bootstrap, loading, interpretation/JIT, GC, natives, and shutdown.
- [Runtime Contracts](book/src/internals/runtime-contracts.md) — loader
  identity, roots, barriers, safepoints, exception frames, dispatch, monitors,
  native capabilities, flags, and bootstrap invariants.
- [Interpreter](book/src/internals/interpreter.md)
- [JIT Compiler](book/src/internals/jit.md)
- [Garbage Collector](book/src/internals/garbage-collector.md)
- [Class Loading and Verification](book/src/internals/class-loading.md)
- [Threading and Concurrency](book/src/internals/threading.md)
- [Native Methods](book/src/internals/native-methods.md)

Current focused architecture notes live under [`architecture/`](architecture/).
Forward-looking proposals live under
[`feature-designs/`](feature-designs/README.md); a proposal is not evidence that
the feature is implemented.

Current deep dives include:

- [Compact object and field layout](architecture/compact-object-and-field-layout.md)
- [Class-loader unloading](architecture/class-loader-unloading.md)
- [Inline allocation and reference publication](architecture/inline-allocation-and-reference-publication.md)
- [Continuation-backed virtual threads](architecture/continuation-backed-virtual-threads.md)
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
- [Mapped JAR and shared class bytes](architecture/mapped-jar-shared-class-bytes.md)
- [Runtime flag census](flag-census.md)
- [Differential testing and divergence log](differential-testing.md)
- [Synthetic versus real JDK paths](synthetic-vs-real-explained.md)

The canonical in-source lock hierarchy is
[`../vm/src/runtime/lock_order.rs`](../vm/src/runtime/lock_order.rs). The
[no-synthetic-stubs policy](contributing/no-synthetic-stubs.md) and
[stub ratchet](contributing/stub-ratchet.md) govern application-visible JDK
compatibility work.

## Compatibility and platform status

- [Compatibility and Support Policy](book/src/reference/compatibility-policy.md)
- [Java Version Support](book/src/java-support/version-support.md)
- [Language Features](book/src/java-support/language-features.md)
- [Standard Library Coverage](book/src/java-support/standard-library.md)
- [Known Limitations](book/src/java-support/limitations.md)
- [JDK Coverage Inventory](JDK_COVERAGE.md)
- [Platform Support Matrix](book/src/reference/platform-support.md)
- [JCK Engineering Status](jck-compliance.md)
- [JavaFX Status](javafx-status.md)

CratonVM is not JCK-certified. See [legal.md](legal.md) for licensing and
compliance terminology.

## JDK-only mode (`--jdk-only`)

An **internal diagnostic stage**, not a supported runtime mode. `--jdk-only`
asks the VM to treat real JDK class bytes as authoritative — no fabricated
compatibility class, no synthetic-stub native registered or invoked — so that
every compatibility substitution CratonVM performs becomes a counted,
attributed event. It is expected to fail on programs that run fine under
`--real-jdk`; that failure is the measurement. **`--real-jdk` remains the
default and is unaffected.**

- [JDK-only mode — design contract](feature-designs/jdk-only-mode.md) — the
  normative interface contract. A proposal document: read it for semantics, not
  as evidence that a wave has landed.
- [Migration and operator guide](jdk-only-migration.md) — what the flag means,
  how to read a violation, and the staged path from diagnostic to default.
- [Reproducible audit](jdk-only-audit.md) — how to *measure* substitutions:
  the census dumps, their schemas, and why a source grep is not the oracle.
- [Blocker inventory by service area](jdk-only-runtime-services.md) — the
  confirmed, open work list; a floor, not a ceiling.
- [Native promotion review](jdk-only-native-review.md) — the checklist a
  `SyntheticStub` must pass to become a reviewed `Bridge` or `Intrinsic`.
- [Natives over real JDK classes](architecture/natives-over-real-jdk-classes.md)
  — the mechanism facts every wave-2 lane rediscovered: reachability,
  feature-vs-mode, last-wins registration, by-name field reads, slot indices,
  census scope, and measurement discipline. Read it before reading a record in
  the list below.
- [Object-layout audit](jdk-only-object-layout-audit.md) — every site that
  reaches a field by assumed synthetic slot index, a prerequisite for
  dropping the corresponding stub.
- [Threat model](security/jdk-only-threat-model.md) — what the flag does and
  does not change about the VM's trust posture. It is a correctness and
  provenance control, **not** an isolation boundary.
- [Benchmarks and non-regression budgets](benchmarking/jdk-only.md) — proposed
  gates; no baseline has been captured yet.
- [Open wave-2 issues](known-issues/jdk-only/README.md) — gaps deliberately
  deferred rather than papered over, with the evidence that makes them
  actionable.

## Embedding and native integration

- [Embedding Overview](book/src/embedding/overview.md)
- [C ABI / JNI Invocation API](book/src/embedding/c-abi.md)
- [Rust Embedding Facade](book/src/embedding/rust-facade.md)
- [Standalone Embedding Reference](EMBEDDING.md)
- [Hosting the VM Crate](EMBEDDING_VM_CRATE.md)

## Security and cryptography

- [Security Overview](book/src/security/overview.md)
- [Sandboxing and Hardening](book/src/security/sandboxing.md)
- [Cryptography](book/src/security/cryptography.md)
- [Security Hardening Reference](SECURITY_HARDENING.md)
- [Cryptographic Algorithm Status](CRYPTO_STATUS.md)
- [JDK-only mode threat model](security/jdk-only-threat-model.md) — advisory;
  `--jdk-only` narrows *which class implementations may execute* and creates no
  isolation boundary.
- [`../SECURITY.md`](../SECURITY.md) — vulnerability reporting policy.

## GPU offload

- [GPU Offload Overview](book/src/gpu/overview.md)
- [GPU Benchmarks](book/src/gpu/benchmarks.md)
- [GPU Reference](gpu/README.md)

GPU execution is opt-in and requires a `gpu-driver` build. The GPU reference is
the source of truth for eligibility, driver requirements, and experimental
status.

## Contributors and maintainers

- [`../BUILD_GUIDE.md`](../BUILD_GUIDE.md)
- [`../CONTRIBUTING.md`](../CONTRIBUTING.md)
- [Testing](book/src/contributing/testing.md)
- [Documentation Guide](book/src/contributing/documentation.md)
- [`../RELEASING.md`](../RELEASING.md)
- [Release Readiness](RELEASE_READINESS.md)
- [`../ROADMAP.md`](../ROADMAP.md)
- [Code Coverage](COVERAGE.md)

Validate maintained Markdown and mdBook membership with:

```bash
python3 tools/check_markdown_links.py
```

Use `--all` to audit historical/internal Markdown too. The default check covers
the maintained public documentation so historical evidence with intentionally
preserved links does not block normal documentation work.

## Issue and evidence taxonomy

- `docs/known-issues/` contains unresolved bugs and active investigations.
- After a bug is fixed and covered, move its document to ``.
- `` is non-normative historical evidence and audit material.
- `docs/architecture/` describes current architecture that remains useful
  outside a single fix.
- `docs/feature-designs/` contains proposals and must state when work is not yet
  implemented.

For current project status, use [`../ROADMAP.md`](../ROADMAP.md), the
[compatibility policy](book/src/reference/compatibility-policy.md), and the
[known limitations](book/src/java-support/limitations.md), not an old internal
audit.
