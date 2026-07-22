# CratonVM Documentation

> **📖 The complete manual lives in [`book/`](book/src/SUMMARY.md).**
> It is a unified, navigable documentation site (mdBook-buildable, and readable
> as Markdown on GitHub) covering installation, the user guide, Java support,
> security, performance, GPU offload, embedding, internals, contributing, and a
> full reference. Build it locally with `mdbook serve docs/book` (or just read
> the Markdown). The standalone files below remain the source for several deep
> reference tables and are linked from the book.

Index of the CratonVM documentation set. Links are relative to this `docs/`
folder. See also the root [`README.md`](../README.md) for a project overview
and [`ARCHITECTURE.md`](../ARCHITECTURE.md) for the system design.

## Getting started

- [INSTALL.md](INSTALL.md) — install pre-built binaries or build from source.
- [CONFIG.md](CONFIG.md) — configuration reference for the `cratonvm` launcher flags and `CRATONVM_*` env vars.
- [CONTAINER.md](CONTAINER.md) — container / cgroup awareness and ergonomic resource defaults.
- [PLATFORMS.md](PLATFORMS.md) — platform support matrix; which host OS supports which syscall-touching features.
- [TROUBLESHOOTING.md](TROUBLESHOOTING.md) — common issues and their solutions.

## Using and embedding

- [EMBEDDING.md](EMBEDDING.md) — embed CratonVM via the C-ABI (`libcratonvm`) or the Rust facade (`cratonvm-embed`).
- [internal/embedding.md](internal/embedding.md) — host `cratonvm-vm` inside a Rust application (internal notes).
- [internal/gc-tuning.md](internal/gc-tuning.md) — tune heap and GC behaviour for a given workload.
- [PROFILING.md](PROFILING.md) — measure and improve CratonVM performance.
- [COVERAGE.md](COVERAGE.md) — generate code coverage with `cargo-llvm-cov` (local + advisory CI).

## Reference and status

- [JDK_COVERAGE.md](JDK_COVERAGE.md) — JDK class/method coverage, auto-generated from the native crates.
- [internal/jck-compliance.md](internal/jck-compliance.md) — internal JCK compliance estimate matrix.
- [legal.md](legal.md) — JCK licensing and legal requirements.
- [RELEASE_READINESS.md](RELEASE_READINESS.md) - public release, crates.io dry-run, Apache-2.0 notice, and repository readiness checklist.
- [SECURITY_HARDENING.md](SECURITY_HARDENING.md) — sandboxing, egress/SSRF policy, and crypto-hardening reference (companion to [`SECURITY.md`](../SECURITY.md)).
- [CRYPTO_STATUS.md](CRYPTO_STATUS.md) — per-algorithm cryptographic implementation status (companion to [`SECURITY.md`](../SECURITY.md)).
- [internal/javafx-status.md](internal/javafx-status.md) — JavaFX as an out-of-tree, non-core module.
- [PRESENTATION.md](PRESENTATION.md) — JIT performance results write-up.

## GPU offload

- [gpu/README.md](gpu/README.md) — single source of truth for the opt-in GPU offload feature; phase specs and reports live alongside it under [`gpu/`](gpu/).

## Design notes and policy

- [`../vm/src/runtime/lock_order.rs`](../vm/src/runtime/lock_order.rs) — canonical, in-source definition of the global lock acquisition order (the `LockLevel` hierarchy) and its runtime-enforcement wrappers.
- [internal/app-jvm-bugs/jvm-no-synthetic-stubs.md](internal/fixed-suite-bugs/app-jvm-bugs/jvm-no-synthetic-stubs.md) — project rule: run real Java classes, no synthetic stubs.
- [internal/feature_roadmap_interpreter_intrinsic_table.md](internal/feature_roadmap_interpreter_intrinsic_table.md) — interpreter intrinsic table roadmap.
- [internal/feature_roadmap_jit_intrinsics.md](internal/feature_roadmap_jit_intrinsics.md) — roadmap for JIT-inlined intrinsics beyond `java.lang.Math`.

## Design proposals (forward-looking)

Grounded engineering designs for larger, not-yet-landed features. See [`feature-designs/`](feature-designs/):

- [feature-designs/precise-jit-maps-default.md](feature-designs/precise-jit-maps-default.md) — precise JIT stack maps as the validated default.
- [feature-designs/deopt-osr.md](feature-designs/deopt-osr.md) — real-frame deoptimization + virtual-object rematerialization + precise OSR.
- [feature-designs/concurrent-gc-maturation.md](feature-designs/concurrent-gc-maturation.md) — mature G1 into a selectable, validated collector.
- [feature-designs/foreign-thread-attach.md](feature-designs/foreign-thread-attach.md) — foreign-thread attach with safepoint participation.
- [feature-designs/differential-fuzzer.md](feature-designs/differential-fuzzer.md) — semantic differential fuzzer vs HotSpot.

## Open investigations

- [internal/bc-ec-mod-mododdinverse-investigation.md](internal/bc-ec-mod-mododdinverse-investigation.md) — BouncyCastle EC `Mod.modOddInverse` residual failures.
- [internal/app-jvm-bugs/tomcat-selector-investigation.md](internal/fixed-suite-bugs/app-jvm-bugs/tomcat-selector-investigation.md) — Tomcat NIO selector investigation.
- [internal/app-jvm-bugs/jit-safepoint-revert.md](internal/fixed-suite-bugs/app-jvm-bugs/jit-safepoint-revert.md) — JIT precise-oop-map fixes, reverted.

## Internal notes

`docs/internal/` holds non-normative internal development notes (round logs,
blocker maps, session handoffs, benchmark scratch). These are working notes,
not authoritative. For canonical project status see the root
[`ROADMAP.md`](../ROADMAP.md) and [`SECURITY.md`](../SECURITY.md).
