# vm crate unit-test residuals uncovered after the monitor.rs SIGSEGV fix

**Status:** ✅ RESOLVED (2026-07-06). All 4 residuals fixed on `dev` — commit `f32778db`
(`fix/vm-unit-test-residuals-20260706`), merged via `a131ff07`. Verified on the Azure build host:
`cargo test -p cratonvm-vm --lib` (debug) 2144/2144 passed, 0 failed; `--release --lib` all pass
except the pre-existing, documented `runtime::lock_order::tests::*` release-only false-alarm
cluster (unrelated, unchanged by this pass — see below).

**Context.** `cargo test -p cratonvm-vm --release --lib` used to SIGSEGV partway through the suite
(see the now-internal doc for that fix). Once fixed, the suite ran to completion for the first time
in a while and surfaced 16-17 failures. Of those:
- 8 (now 9, a canary test was added later) were `runtime::lock_order::tests::*` — a false alarm, not
  a bug: that module's lock-order enforcement is gated behind `cfg!(debug_assertions)`, which
  `--release` disables, so its own `#[should_panic]` tests correctly fail when the enforcement they're
  testing is compiled out. They all pass under a plain (non-`--release`) `cargo test -p cratonvm-vm
  --lib` — reconfirmed during this pass (2144/2144 debug, 0 failed).
- 3 were `vm::vm_init::tests::{shared_vm_default_config,vm_self_arc_initialized,real_jdk_mode_registers_fewer_natives}`
  — stale hardcoded counts, already fixed in an earlier pass (`fix/vm-monitor-test-object-heap-uaf`).
- 1 (`native::jni::tests::process_vm_publish_and_resolve`) only reproduced under `--release`, passed
  clean under a debug build — not investigated further, not part of this pass.

The remaining 4 were real, deterministic, reproduced identically in debug and release — each is
fixed below.

## 1. `threading::virtual_scheduler::tests::release_cannot_exceed_carrier_count` — test was wrong

`../../../vm/src/threading/virtual_scheduler.rs`. The test asserted that after `new(2)` → 2×`acquire()`
(available 2→0) → 2×`release()`, `available()` should be `1`, not `2` — i.e. it wanted the second
`release()` treated as an illegitimate "double release" even though 2 real, unreleased `acquire()`
calls were outstanding.

Decision: **the test's invariant was incorrect, not the scheduler.** With 2 real acquires
outstanding, both matching releases are legitimate and must fully restore `available` to capacity
(`2`), matching real semaphore semantics and the already-shipped, documented Bug B4 ceiling-clamp
`release()`. Every production caller pairs `acquire()`/`release()` per blocking event
(`vt_acquire_carrier`/`vt_release_carrier`, and the symmetric `release`/`acquire` pair around
`park` in `vm_exec.rs`), so two *different* virtual threads legitimately releasing back-to-back with
no intervening acquire is an expected, common pattern — not a bug. Implementing the test's stricter
"outstanding-permit" invariant would require tracking real permit tokens (an API redesign) and, worse,
would silently discard a legitimate permit in exactly that common back-to-back-release scenario,
introducing a real permit-starvation bug where none existed. Fixed the test: corrected the final
assertion to expect `2` (full restore) and added a follow-up assertion that a further, truly
unmatched `release()` is still clamped at `2` (not accumulated), preserving Bug B4's actual coverage.

## 2. `vm::vm_exec::tests::coerce_aligned_long_to_ref_does_not_fabricate_objectref` — test needed a real registered address

`../../../vm/src/vm/vm_exec.rs`. Fixed by registering the synthetic constant's provenance before checking it:
`let _known = unsafe { ObjectRef::from_raw(aligned as *mut u8) };` immediately before the
`jlong_bits_as_aligned_object_ptr(aligned).is_some()` assertion — mirroring the existing sibling test
`jlong_bits_as_aligned_object_ptr_matches_coerce_contract` in `../../../types/src/value.rs`, which does the
same thing. `jlong_bits_as_aligned_object_ptr` only recognizes bits that have crossed the
`ObjectRef::from_raw`/`from_raw_nonnull` provenance boundary (`../../../types/src/value.rs`'s `PROVENANCE_L1`
bitmap) — a bare untouched constant correctly has no entry. The provenance hardening itself was
already correct; only the test's setup was stale. (The rest of the test, exercising
`coerce_value_for_return`'s unconditional `Value::Long(_) => Value::Object(None)` mapping for `L`/`[`
return types, never depended on provenance at all — that coercer doesn't call
`jlong_bits_as_aligned_object_ptr`.)

## 3. `runtime::frame::tests::set_local_compact_long_preserves_kind_and_upper_half_mark` — real bug, fixed

`../../../vm/src/runtime/frame.rs`'s `Frame::invalidate_cat2_upper_half` filled a category-2 upper-half slot
with `CompactValue::int(0)` while marking `local_kinds[i+1]` as `LKIND_LONG`/`LKIND_DOUBLE`. But
`CompactValue::int(0)` is NaN-boxed (`raw_bits() == 0xFFFC_0000_0000_0000`, per
`../../../types/src/compact_value.rs`'s NaN-boxing scheme), not the raw untagged `0` that every other
`LKIND_LONG`/`_DOUBLE`-marked-slot reader assumes (`get_local_raw`, `locals_snapshot`,
`try_osr`'s per-local snapshot) — those readers return `CompactValue::raw_bits()` verbatim when the
kind mark says "long/double", exactly matching how a *real* `CompactValue::long`/`double` is stored
(bit-exact, no NaN-boxing). This was a genuine kind/representation mismatch: the filler's kind mark
promised "raw bits", but the filler didn't hold raw bits.

Fixed: use `CompactValue::long(0)` instead of `CompactValue::int(0)` — bit-exact `0`, still provably
a non-object (an untagged `0` never satisfies `is_nan_tagged`), and with **zero change to GC-scan
behavior**: `scan_local_objects`/`update_local_refs` gate purely on `local_kinds[i] ==
LKIND_LONG/_DOUBLE` (never inspect the slot's actual bits for this check), so the safety property the
original comment described (upper half never treated as a stale object root) is fully preserved.

## 4. `jit::skip_list::tests::keycloak_credential_lazy_init_getters_lifted_under_safe_default` — real bug, fixed

`../../../vm/src/jit/skip_list.rs`. Root cause: the KC26-PIC.1 blanket `org/keycloak/` ban (2026-07-05, added
for an unrelated Picocli-command / SmallRye-config-mapper boot timeout) unconditionally skip-lists
every class under `org/keycloak/`, which accidentally re-caught
`org/keycloak/models/credential/dto/{PasswordCredentialData,PasswordSecretData}` and
`org/keycloak/models/credential/CredentialModel`. An *earlier* fix, KC-CRED.LAZY (2026-07-01), had
already deliberately narrowed a real correctness bug in exactly the first two methods to a targeted
`is_known_miscompile` entry gated behind `callee_saved_gpr_local_homes_enabled()` (default off) — i.e.
these getters were already meant to be JIT-eligible under the safe Conservative default, and this test
was written to lock that in. The later, broader KC26-PIC.1 ban silently regressed that decision
without anyone noticing the overlap (the two bans were added in different sessions, four days apart).

Fixed: added a carve-out — `&& !class_name.starts_with("org/keycloak/models/credential/")` — to the
KC26-PIC.1 gate, so the credential DTO/model package stays exempt from the blanket Keycloak ban while
every other `org/keycloak/`, `picocli/`, and `io/smallrye/` class remains banned as before (verified
the existing `keycloak_picocli_smallrye_packages_skip_under_conservative` test, which only exercises
`org/keycloak/quarkus/runtime/cli/Picocli`, is unaffected).

## How this was reproduced/verified

```
ssh -i ~/.ssh/azure.pem -o IdentitiesOnly=yes victor@<current-azure-host-ip>
cd /data/data/cratonvm   # or any fresh worktree off dev
cargo test -p cratonvm-vm --lib -- --test-threads=1 \
  threading::virtual_scheduler::tests::release_cannot_exceed_carrier_count \
  vm::vm_exec::tests::coerce_aligned_long_to_ref_does_not_fabricate_objectref \
  runtime::frame::tests::set_local_compact_long_preserves_kind_and_upper_half_mark \
  jit::skip_list::tests::keycloak_credential_lazy_init_getters_lifted_under_safe_default
```
All 4 pass post-fix, in both debug and `--release`. Full-suite runs on `dev @ a131ff07`: debug
2144/2144 (0 failed); release all pass except the pre-existing `lock_order` release-only cluster
(unrelated, unchanged).
