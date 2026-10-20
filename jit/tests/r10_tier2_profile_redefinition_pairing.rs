// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `TieredCompilationManager::on_class_redefined` purges every tiering verdict
//! about a redefined class (`ineligible`, `c2_bailout`, `tier_fail_count`, OSR
//! denials, the JIT bail list, the runtime de-speculation registry). It does
//! not, and cannot from `jit/src/tiered.rs` alone, purge
//! `crate::profile::ProfileStore` — a sibling component of `SharedVm.jit`, not
//! something `TieredCompilationManager` holds a handle to.
//!
//! `ProfileStore` is keyed by `(class_id, method_name, descriptor)`
//! (`crate::profile::MethodKey`), and `class_id` is stable across a
//! redefinition — the JVMTI object identity does not change, only the bytecode
//! behind it does. So a redefinition that purged only the tiering half would
//! leave the old bytecode's branch counts, back-edge counts and receiver-type
//! tables sitting under the exact key the new bytecode's own recordings will
//! accumulate into, with nothing to tell the two apart.
//!
//! # This is the FIXED shape, and what each half now proves
//!
//! It used to be the gap, and this file used to characterize it: it asserted
//! "the receiver profile is still there after `on_class_redefined`", and its
//! doc said to invert that assertion and retire the known-issue page the day
//! someone wired `ProfileStore::invalidate_class` into a redefinition path.
//! That day came — the five production call sites in `vm/src/native/jni.rs`
//! and `vm/src/vm/vm_exec.rs` each call it beside `on_class_redefined`, and
//! `docs/internal/retired/r10-tier2-profile-store-survives-class-redefinition-20260921-RETIRED-20260922.md`
//! is the retired page.
//!
//! What was NOT inverted is the first assertion, and deliberately: the two
//! stores really are independent, and `on_class_redefined` really does leave
//! the profile alone. That is not the bug — it is the reason the pairing has
//! to be made by the CALLER, and asserting it here is what stops a future
//! reader from "fixing" it inside `tiered.rs`, where there is no handle to
//! reach. So this file proves the two building blocks:
//!
//!  1. `on_class_redefined` does not, and structurally cannot, clear a profile;
//!  2. `ProfileStore::invalidate_class` does clear one, under the same class
//!     identity the redefinition path has in hand.
//!
//! The half neither of these can prove — that the five production call sites
//! actually MAKE the paired call — is a source ratchet over the two files that
//! hold them: `vm::jit::redefinition_invalidation`'s
//! `every_redefinition_site_invalidates_the_profile_store_too`. It has to live
//! in the `vm` crate because that is where the call sites are, and this crate
//! cannot see them.

use cratonvm_jit::profile::{MethodKey as ProfileMethodKey, ProfileStore};
use cratonvm_jit::tiered::{MethodKey as TieredMethodKey, TieredCompilationManager};
use cratonvm_types::ClassId;

#[test]
fn redefinition_pairs_tiering_reset_with_an_explicit_profile_invalidation() {
    // `ProfileStore::record_receiver` no-ops behind the process-wide
    // `PROFILING_ENABLED` gate (default off — see `profile.rs`'s module
    // comment on why). Process-global, so this file holds exactly one test
    // (its own binary; cargo gives every `tests/*.rs` file its own process),
    // the same discipline `code_cache_cap_tiering.rs` documents for
    // `COMMITTED_JIT_CODE_BYTES`.
    cratonvm_jit::profile::enable_profiling(true);

    let class_id = ClassId::new(0x00CA_CE03);
    let class_name = "cratonvm/test/RedefinedReceiverSite";
    let profile_key = ProfileMethodKey {
        class_id: class_id.as_u32(),
        method_name: "dispatch".into(),
        descriptor: "(Ljava/lang/Object;)V".into(),
    };

    // Record a receiver-type profile at one call site, as the interpreter
    // would while warming up the OLD bytecode: two distinct receiver classes,
    // the polymorphic shape a redefinition can turn monomorphic (or vice
    // versa) by changing what the call site's static type actually is.
    let profiles = ProfileStore::new();
    profiles.record_receiver(&profile_key, 12, 100);
    profiles.record_receiver(&profile_key, 12, 200);
    let before = profiles
        .get_profile(&profile_key)
        .expect("a profile was just recorded for this key");
    assert_eq!(
        before.receivers.get(&12).map(|r| r.len()),
        Some(2),
        "setup: the receiver table at pc 12 should hold both classes"
    );

    // Step 1: the tiering half, through the same manager call every production
    // call site makes.
    let tiered = TieredCompilationManager::with_default_policy();
    let tiered_key =
        TieredMethodKey::with_class_id(class_id, class_name, "dispatch", "(Ljava/lang/Object;)V");
    // Give the manager a `MethodState` to forget, so the call below is
    // exercising real cleanup and not a no-op on an unknown key. One
    // invocation is enough: `on_method_invocation` inserts the entry
    // unconditionally, before it even asks whether the method is hot.
    let _ = tiered.on_method_invocation(&tiered_key);
    tiered.on_class_redefined(class_id, class_name);

    // ...and it reaches exactly as far as its own store. NOT a defect, and not
    // a thing to "fix" here: `TieredCompilationManager` holds no handle to a
    // `ProfileStore`, so there is nothing in `tiered.rs` that could make this
    // call do more. It is the reason the pairing is the caller's job, and the
    // reason a source ratchet in the `vm` crate is what guards it.
    let after_tiering_only = profiles
        .get_profile(&profile_key)
        .expect("on_class_redefined has no handle to a ProfileStore: untouched");
    assert_eq!(
        after_tiering_only.receivers.get(&12).map(|r| r.len()),
        Some(2),
        "on_class_redefined must not be expected to reach the profile store — \
         if this ever starts passing by clearing the profile, the two stores \
         were wired together and vm::jit::redefinition_invalidation's ratchet \
         should be retired with this assertion"
    );

    // Step 2: the profile half, which every production redefinition site now
    // makes on the line beside the one above (vm/src/native/jni.rs:4682,
    // vm/src/vm/vm_exec.rs:8928/11442/11518/11666), exactly as the class
    // UNLOAD path in vm/src/memory/gc.rs has always done.
    profiles.invalidate_class(class_id.as_u32());
    assert!(
        profiles.get_profile(&profile_key).is_none(),
        "ProfileStore::invalidate_class is the second half of the pairing; \
         with it the redefined class's methods re-warm from zero instead of \
         from the old bytecode's branch ratios and receiver tables"
    );
}
