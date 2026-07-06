# vm crate unit-test residuals uncovered after the monitor.rs SIGSEGV fix

**Status:** 🟠 4 open, non-order-dependent test failures — each confirmed to fail even when run
completely alone (`--exact <name>`), so none of these are test-order/global-state pollution. All
reproduce on `dev` post `eebcad66` (`fix/vm-monitor-test-object-heap-uaf`) in both debug and
`--release` builds via `cargo test -p cratonvm-vm --lib`.

**Context.** `cargo test -p cratonvm-vm --release --lib` used to SIGSEGV partway through the suite
(see the now-internal doc for that fix). Once fixed, the suite ran to completion for the first time
in a while and surfaced 16-17 failures. Of those:
- 8 were `runtime::lock_order::tests::*` — a false alarm, not a bug: that module's lock-order
  enforcement is gated behind `cfg!(debug_assertions)`, which `--release` disables, so its own
  `#[should_panic]` tests correctly fail when the enforcement they're testing is compiled out. They
  all pass under a plain (non-`--release`) `cargo test -p cratonvm-vm --lib`.
- 3 were `vm::vm_init::tests::{shared_vm_default_config,vm_self_arc_initialized,real_jdk_mode_registers_fewer_natives}`
  — stale hardcoded counts (`class_manager.loaded_count() == 5`, natives `< 7000`) left over from
  before the unmodifiable-collection-view bootstrap work (`d3474b3e`, `c2d68883`) legitimately grew
  `SharedVm::new`'s bootstrap footprint to 25 classes / ~8800 natives. **FIXED** in this pass — see
  commit on `fix/vm-monitor-test-object-heap-uaf` (bumped to the verified-current values with a
  comment pointing at the responsible commits, so a future genuine regression is still caught).
- 1 (`native::jni::tests::process_vm_publish_and_resolve`) only reproduced under `--release`, passed
  clean under a debug build — likely an optimization-sensitive ordering/timing artifact of the
  global `process_vm` cell; not investigated further (didn't reproduce standalone in debug, low
  suspicion of a real bug, but not proven either way).

The remaining 4 are real, deterministic, reproduce identically in debug and release:

## 1. `threading::virtual_scheduler::tests::release_cannot_exceed_carrier_count`

`vm/src/threading/virtual_scheduler.rs:122`. Sequence: `new(2)` → 3 stray `release()` while full (no-op,
passes) → 2x`acquire()` (available 2->0) → 2x`release()` (expects available `== 1`, "double release adds
at most one permit"; actual `== 2`).

Root cause: `release()` (line ~76) is `let next = (state.available + 1).min(self.carrier_count);` — a
pure ceiling clamp against the static `carrier_count`, exactly matching what the "Bug B4 (round-9)"
doc comment above it describes and fixes (a stray release while *already full* is a no-op). The test
wants a *stricter*, different invariant: releases should be bounded by how many permits are
currently legitimately outstanding, not just by the static ceiling — i.e. it wants the scheduler to
tell a legitimate release from an over-release even when not already at the ceiling. The current
`AtomicUsize`-based design has no concept of "permits currently out" (no acquire-token / down-counter),
so it structurally cannot distinguish "2 real completions" from "1 real + 1 spurious" — implementing
the test's expectation would need a real design change (track outstanding acquires, not just a
clamped counter), not a one-line fix. Needs a decision: is the test's stricter invariant actually
required by a real caller, or should the test be relaxed to match the (already-shipped, documented)
Bug B4 ceiling-clamp fix?

## 2. `vm::vm_exec::tests::coerce_aligned_long_to_ref_does_not_fabricate_objectref`

`vm/src/vm/vm_exec.rs:13704`. Fails at the first assertion (line 13713):
`assert!(jlong_bits_as_aligned_object_ptr(0x4000_0000u64).is_some())`.

Root cause: `jlong_bits_as_aligned_object_ptr` (`types/src/value.rs:699`) delegates to
`object_ref_payload_is_known` (`types/src/value.rs:250`), which checks a global provenance bitmap
(`PROVENANCE_L1`) recording addresses that have *actually* been constructed as real `ObjectRef`s —
not just "looks like a plausible aligned non-null pointer." The test uses a synthetic constant
(`0x4000_0000`) that was never allocated through the real heap, so it correctly has no provenance
entry and `is_some()` is `false`. This reads like the test predates a (correct, more secure)
hardening of "is this a real object pointer" from a purely structural heuristic to an actual
provenance-tracked check — which is exactly the property the test's own doc comment wants ("must NOT
be reinterpreted as a heap ObjectRef... that fabricated pointer would later be marked/moved by GC and
crash"). The current implementation is arguably *more* correct than what the test exercises. Fixing
this properly means rewriting the test to allocate a real object, capture its address, register it
(if there's a public provenance-registration entry point) — or use `jlong_bits_as_aligned_object_ptr`
against that real address — rather than a bare constant. Did not attempt this rewrite: it touches
GC-pointer-provenance code that's clearly a deliberate recent security hardening by someone else, and
a wrong test rewrite could mask a real regression instead of just modernizing a stale assumption.

## 3. `runtime::frame::tests::set_local_compact_long_preserves_kind_and_upper_half_mark`

`vm/src/runtime/frame.rs:2101`. Writes `CompactValue::object(0x1000)` into local slot 1, then
`CompactValue::long(-1)` into local slot 0 (a category-2 value that spans slots 0-1). Expects slot 1's
tag to become `VTAG_LONG` with raw `== 0` (the long's upper-half placeholder correctly overwrites
whatever was in slot 1 before). Actual: slot 1's raw value is `18445618173802708992` (close to but not
`u64::MAX`, and not `0`) — looks like `set_local_compact`'s category-2 write only updates slot 1's
*tag* bits, not fully clearing its *raw* payload bits, leaving a mix of the long's upper-half sentinel
and the stale `0x1000` object-pointer bits from the earlier write. Given the test's own framing
("preserves kind and upper half mark" / "stale upper half in later scans and OSR snapshots") this is
in the immediate neighborhood of the plain-field 16-byte slot-tearing work that landed on `dev` the
same day (`fix/plain-field-slot-tearing`, see internal memory) — worth checking whether that change
(or something adjacent to it) touched `Frame::set_local_compact`'s category-2 path, since the symptom
(leftover raw bits from a differently-typed prior write) is exactly the class of bug that work was
about. Not root-caused further here — needs someone with context on the current `CompactValue`/
`Frame` local-slot layout to trace `set_local_compact`'s exact bit-write sequence for the two slots.

## 4. `jit::skip_list::tests::keycloak_credential_lazy_init_getters_lifted_under_safe_default`

`vm/src/jit/skip_list.rs:2896`. Expects `check("org/keycloak/models/credential/dto/PasswordCredentialData",
"getAdditionalParameters", false, true, SkipPolicy::Conservative)` to return `None` (JIT-eligible).
Actual: `Some(SkipReason::RustJvmTestFixture)` — the method IS being skip-listed. `RustJvmTestFixture`
is a real, frequently-used `SkipReason` variant (20+ return sites in `check`'s body, not a test-only
marker), so this isn't test pollution — some heuristic branch in `check`/`is_known_miscompile`
genuinely matches this Keycloak class+method under `Conservative` policy. Did not trace which of the
~20 `RustJvmTestFixture`-returning branches fires for this specific (class, method) pair — the
function is large and the match isn't obvious from a skim; whoever picks this up should add a quick
`eprintln!`/`dbg!` at each candidate branch (or binary-search by commenting branches out) to isolate
which condition over-matches, then decide whether to narrow that condition or whether the skip-list
entry is intentionally new and the test is the one that's stale.

## How to reproduce all 4 at once

```
ssh -i ~/.ssh/azure.pem -o IdentitiesOnly=yes victor@<current-azure-host-ip>
cd /data/data/cratonvm   # or any fresh worktree off dev
cargo test -p cratonvm-vm --lib -- --test-threads=1 \
  threading::virtual_scheduler::tests::release_cannot_exceed_carrier_count \
  vm::vm_exec::tests::coerce_aligned_long_to_ref_does_not_fabricate_objectref \
  runtime::frame::tests::set_local_compact_long_preserves_kind_and_upper_half_mark \
  jit::skip_list::tests::keycloak_credential_lazy_init_getters_lifted_under_safe_default
```
(Works in a plain debug build too — none of these 4 are `--release`-only, unlike the lock_order
cluster. See `reference_azure_build_host` memory for the current host IP.)
