# L7 — Make fabrication refusable, and migrate the callers that fire

**Owns:** `classloading/src/class_manager.rs` and its fabrication call sites
**Gated on:** nothing.
**Effort:** M
**Evidence:** [`ensure-synthetic-class-cannot-enforce-only-record.md`](../../known-issues/jdk-only/ensure-synthetic-class-cannot-enforce-only-record.md),
[`vm-internal-classes-mislabelled-compatibility-stub.md`](../../known-issues/jdk-only/vm-internal-classes-mislabelled-compatibility-stub.md)

## Goal

`ensure_synthetic_class` returns a bare `ClassId`, so under `--jdk-only` it
records the violation and **fabricates anyway**. Contract §5 says the policy may
refuse; today it cannot. The fallible siblings (`try_ensure_synthetic_class`)
exist and have zero callers, so nothing changed operationally.

## Size it from the census, not the grep

The record scopes this at "52 live call sites in 27 files". **Three fire on a
strict boot** — `vm_init.rs:1052`, `:1110`, `:1227` and `vm_exec.rs:9814`
(measured 2026-08-04 via `#[track_caller]` through
`ensure_* → fabricate_class → admit_compatibility_class`). The other 49 are
reached by workloads a boot does not run.

This is the single most useful fact in the lane: a plan built on 52 does most of
its work blind and none of it in priority order. Take the class-origin census
for the workload you care about (`--dump-class-origins`, with `requested_by`
naming the **Rust** call site) and migrate what actually fires.

## Steps

1. Take `--dump-class-origins` for: a strict boot, `JdkOnlyCensusLoadProbe`,
   `JdkOnlyBreadthProbe`, and ideally H2 or Spring Boot (L8 overlaps here).
   Rank call sites by fabrication count.
2. Migrate the firing callers to `try_ensure_synthetic_class` and give each a
   real refusal path. "Refuse" must mean a diagnosable error, not a silent
   `None` — the record notes that a strict boot today *silently loses*
   `Enumeration$Impl` / `Comparator$Native`.
3. **`Proxy$Instance` (item 5).** Its origin question is already answered:
   `VmInternal`, not `GeneratedProxy`, and the in-code marker has been
   corrected. Flipping it is not attempted because `is_synthetic_stub` is
   derived from `ClassOrigin` and 181 read sites across 20 files depend on it —
   **two of which gate native-vs-bytecode dispatch**. Find those two first;
   they decide whether this is a one-line flip or a dispatch change.
   `AnonymousObject$N` was migrated to `VmInternal` on 2026-08-04 and verified
   (`compatibility-stub` 14 → 13, `vm-internal` 0 → 1, total unchanged) — copy
   that verification shape.
4. `Function$Identity` is a known trap: it is `Bridge` (so `--jdk-only`
   registers and invokes it) but **has no class file anywhere** — it is minted
   by `alloc_concurrent_synthetic`, which contract §5 forbids under `JdkOnly`.
   Today the two defects cancel: fabrication records the violation and proceeds.
   **Fixing either one alone exposes the other.** The real fix is upstream —
   `Function.identity()` should return the real lambda.

## Verification

* Class-origin census before/after: `compatibility-stub` count falls by exactly
  the number of migrated fabrications; `total` unchanged unless you intend
  otherwise.
* Strict boot must still boot. This is the area that produced
  `InternalError: null property: java.home`.
* Both probes vs HotSpot in both modes.

## Done when

Every call site that fires on the measured workloads is fallible and refuses
diagnosably under `JdkOnly`, and the record's "52" is replaced by the measured
count with the workload named.
