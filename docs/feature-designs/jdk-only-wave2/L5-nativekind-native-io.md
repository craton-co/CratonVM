# L5 — `register_with_kind` migration, starting with `native-io`

**Owns:** `native-io/src/*.rs`
**Gated on:** nothing.
**Effort:** M per crate. This lane is the template; L5b/L5c repeat it for other
crates and can run in parallel **once this one has set the pattern**.
**Evidence:** [`native-kind-is-ambient-and-defaults-to-syntheticstub.md`](../../known-issues/jdk-only/native-kind-is-ambient-and-defaults-to-syntheticstub.md)

## Goal

A native's `NativeKind` is never stated at its registration site — it is
inherited from a mutable field on the registry that an enclosing function
happened to set. Measured 2026-08-04 on JDK 25: **11,909 registrations,
`kind_stated` false on every one.** `register_with_kind` exists and has **zero
callers**.

`native-io` first because it carries the strongest *adjudicated* verdicts:
`net.rs` is marked "the strongest bridge evidence in the repo — 36 of …",
`nio_native.rs`, `process.rs` and `random_access_file.rs` are all marked
`bridge` with reasoning. Those are registrations someone actually decided about.

## Rules

* **Do not bulk-convert a crate-wide `set_category` line.** The
  `native-collections` `set_category(Bridge)` covers 1,195 registrations and its
  own marker says flipping it is the 2026-07-14 regression shape at ~8× blast
  radius. `kind_stated: true` must mean *someone adjudicated this*, not *someone
  ran a codemod*. A migration that makes the census lie is worse than no
  migration.
* Start from the `JDK-ONLY-CLASSIFY` markers. Those tagged `bridge` with
  evidence are safe to state explicitly. Those tagged `unknown — needs census`
  are not — take the census first (L6's artefact answers most of them).
* Contract §8 forbids editing `native-builtins/src/lib.rs` for the 157-stub
  reclassification. That is a *different* wave. This lane states existing kinds
  explicitly; it does not change any kind.

## Steps

1. For each `JDK-ONLY-CLASSIFY: bridge` registrar in `native-io`, convert its
   `register(...)` calls to `register_with_kind(..., NativeKind::Bridge)` and
   delete the now-redundant `set_category` scope.
2. Re-run the schema-3 census. Every converted row must show `kind_stated: true`
   **and the same `kind` as before**. A changed kind means the ambient category
   was not what the marker claimed — stop and investigate rather than accepting
   it.
3. Confirm the static adjudication agrees: for rows you tag `Bridge`,
   `image_declaring_method.acc_native` should be `true`. Contract §1.5 defines a
   `Bridge` as what an `ACC_NATIVE` method binds to. Where it is `false`, the
   marker is wrong and this is a reclassification question, not a migration one.
4. Repeat per registrar, re-running `stub_ratchet` each time.

## Verification

* `native-builtins/tests/stub_ratchet.rs` — `BASELINE_SYNTHETIC_STUBS = 157`
  exactly, `SLACK = 0`. This lane must **not** move it. If it does, a kind
  changed and that is out of scope.
* `CRATONVM_DBG_DROPPED_STUBS=1` on a real-JDK boot before and after: the drop
  list must not gain an entry the boot then needs. This is exactly how the
  2026-07-14 `java.util.Properties` bootstrap regression presented — as an
  unrelated `InternalError: null property: java.home`.
* `CRATONVM_NO_STUBS=1` boot before and after.
* Census diff: `kind_stated` count rises by exactly the number of converted
  registrations; no `kind` changes.

## Done when

Every `native-io` registrar with an adjudicated marker states its kind, the
ratchet is unmoved, and the census shows the rise in `kind_stated` with zero
`kind` churn.
