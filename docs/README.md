# CratonVM Documentation

Index of the CratonVM documentation set. Links are relative to this `docs/`
folder. See also the root [`README.md`](../README.md) for a project overview
and [`ARCHITECTURE.md`](../ARCHITECTURE.md) for the system design.

## Getting started

- [INSTALL.md](INSTALL.md) — install pre-built binaries or build from source.
- [CONFIG.md](CONFIG.md) — configuration reference for the `cratonvm` launcher flags.
- [PLATFORMS.md](PLATFORMS.md) — platform support matrix; which host OS supports which syscall-touching features.
- [TROUBLESHOOTING.md](TROUBLESHOOTING.md) — common issues and their solutions.

## Using and embedding

- [embedding.md](embedding.md) — host `cratonvm-vm` inside a Rust application.
- [gc-tuning.md](gc-tuning.md) — tune heap and GC behaviour for a given workload.
- [PROFILING.md](PROFILING.md) — measure and improve CratonVM performance.

## Reference and status

- [JDK_COVERAGE.md](JDK_COVERAGE.md) — JDK class/method coverage, auto-generated from the native crates.
- [jck-compliance.md](jck-compliance.md) — internal JCK compliance estimate matrix.
- [legal.md](legal.md) — JCK licensing and legal requirements.
- [CRYPTO_STATUS.md](CRYPTO_STATUS.md) — per-algorithm cryptographic implementation status (companion to [`SECURITY.md`](../SECURITY.md)).
- [javafx-status.md](javafx-status.md) — JavaFX as an out-of-tree, non-core module.
- [PRESENTATION.md](PRESENTATION.md) — JIT performance results write-up.

## GPU offload

- [gpu/README.md](gpu/README.md) — single source of truth for the opt-in GPU offload feature; phase specs and reports live alongside it under [`gpu/`](gpu/).

## Design notes and policy

- [lock-order.md](lock-order.md) — global lock acquisition order to avoid deadlocks.
- [jvm-no-synthetic-stubs.md](jvm-no-synthetic-stubs.md) — project rule: run real Java classes, no synthetic stubs.
- [feature_roadmap_interpreter_intrinsic_table.md](feature_roadmap_interpreter_intrinsic_table.md) — interpreter intrinsic table roadmap.
- [feature_roadmap_jit_intrinsics.md](feature_roadmap_jit_intrinsics.md) — roadmap for JIT-inlined intrinsics beyond `java.lang.Math`.

## Open investigations

- [bc-ec-mod-mododdinverse-investigation.md](bc-ec-mod-mododdinverse-investigation.md) — BouncyCastle EC `Mod.modOddInverse` residual failures.
- [tomcat-selector-investigation.md](tomcat-selector-investigation.md) — Tomcat NIO selector investigation.
- [jit-safepoint-revert.md](jit-safepoint-revert.md) — JIT precise-oop-map fixes, reverted.

## Internal notes

`docs/internal/` holds non-normative internal development notes (round logs,
blocker maps, session handoffs, benchmark scratch). These are working notes,
not authoritative. For canonical project status see the root
[`ROADMAP.md`](../ROADMAP.md) and [`SECURITY.md`](../SECURITY.md).
