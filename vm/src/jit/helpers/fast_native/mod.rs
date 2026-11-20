#[allow(unused_imports)]
use super::*;
// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// `(vm_ptr, varhandle, receiver)` `extern "C"` function per
// (access mode, primitive return kind) slot.
//
// One implementation, reached two ways. `try_varhandle_instance_field_read`
// below serves the SAME read from inside `jit_invoke_dispatch`, for every site
// this bind declines and for the interpreter's dispatches; both call
// `varhandle_instance_field_read_bits`, so the two routes cannot drift apart
// in what they consider a servable handle.
// ---------------------------------------------------------------------------

/// Erased call-site descriptors for [`VARHANDLE_READ_INFOS`], indexed by the
/// return-kind half of a slot.
///
/// A baked direct call has no `JitInvokeInfo` of its own (see
/// `bytecode_walk.rs`: `info_ptr` is `None` for a thin helper), so the cold arm
/// cannot hand the generic dispatcher the site's real descriptor. These stand
/// in for it.
///
/// For a PRIMITIVE return the substitution is not observable: the descriptor's
/// only readers are the argument decode (one reference coordinate either way),
/// `unbox_poly_return_checked`'s return-type rules and `coerce_native_return`,
/// and all three see the identical return char.
///
/// For a REFERENCE return there is one reader that CAN tell the difference —
/// `varhandle_reference_return_mismatch`, which compares against the site's
/// declared class — and the two reference kinds exist to answer it without a
/// descriptor. `REF_OBJECT` needs no answer (its stand-in IS its real
/// descriptor, and no box is refused at an `Object` return); `REF_STRICT` names
/// a site no box can satisfy at all, so `varhandle_read_direct_reference`
/// raises on any box the cold arm produces. See
/// `cratonvm_jit::VARHANDLE_BOX_ACCEPTING_RETURNS` for the thirteen types that
/// fall between the two and are therefore not bound.
use cratonvm_native_api::NativeCallSiteExt;
#[cfg(test)]
mod nio21_w4_thin_store_satb_tests;
#[cfg(test)]
mod r15w6_vh8_judged_thin_call;
#[cfg(test)]
mod r15w4_vh5_splice_keeps_thin_bind;

#[cfg(test)]
mod r15w2_vh2_thin_bind_exact_gate;
#[cfg(test)]
mod r15w3_vh3_thin_bind_retire;

#[cfg(test)]
mod r15w3_vh4_thin_bound_publication;
/// Round 11 wave 18 (`r11w17-vh-varhandle-reference-slot-group-patch`): the
/// synthetic call site of every write/CAS slot is a site the slot function
/// itself binds to THAT slot, and the value-site classes the dispatch arms
/// from it fall in the slot's static-type group.
#[cfg(test)]
mod r11w18_vh_synthetic_site_groups;
/// Declare the guarded helpers' gates and, in the same breath, the enumeration
/// the tests sweep.
///
/// The enumeration is the reason this is a macro rather than twenty-odd plain
/// `static` items. `jdkonly-thin-jit-direct-helpers-refuse-what-the-tail-then-runs-20260922`
/// opens its "why it was not fixed with its sibling" with *eleven call sites,
/// not one*, and the hazard a per-site gate introduces is the same one
/// `NativeCallSite`'s own doc names: a gate shared between two sites, or a
/// gate whose triple is not the one its helper stands in front of, compiles
/// fine and is silently wrong. A list that is written by the declaration
/// cannot be short, so `every_direct_helper_gate_*` sweeps the population
/// rather than whichever gates a test author remembered.
///
/// nio21 wave 8 L2: the macro also assigns each gate its ordinal (the
/// position of its variant in `DirectHelperGateOrdinal`) and the count
/// [`DIRECT_HELPER_GATE_COUNT`], which sizes the per-VM memo array
/// `JitRealm::direct_helper_gates`. The `static`s keep only `const` metadata.
macro_rules! direct_helper_gates {
    ($(
        $(#[$meta:meta])*
        $gate:ident = $src:tt;
    )*) => {
        /// One variant per gate, in declaration order: the gate's index into
        /// the VM's memo array. Never constructed; only cast.
        #[allow(non_camel_case_types, dead_code, clippy::upper_case_acronyms)]
        #[repr(usize)]
        enum DirectHelperGateOrdinal {
            $($gate,)*
            Count,
        }

        /// How many gates `direct_helper_gates!` declared: the length of the
        /// per-VM memo array (`JitRealm::direct_helper_gates`).
        pub(crate) const DIRECT_HELPER_GATE_COUNT: usize = DirectHelperGateOrdinal::Count as usize;

        $(
            $(#[$meta])*
            pub static $gate: DirectHelperGate =
                direct_helper_gates!(@build $src, DirectHelperGateOrdinal::$gate as usize);
        )*

        /// Every gate declared above, with its item name, for the sweeps.
        #[cfg(test)]
        pub const ALL_DIRECT_HELPER_GATES: &[(&str, &DirectHelperGate)] =
            &[$((stringify!($gate), &$gate)),*];
    };
    (@build (info: $info:ident), $ord:expr) => { DirectHelperGate::for_info(&$info, $ord) };
    (@build (info: $info:ident, capability: $cap:expr), $ord:expr) => {
        DirectHelperGate::for_info_capability(&$info, $cap, $ord)
    };
    (@build (triple: $c:expr, $m:expr, $d:expr), $ord:expr) => {
        DirectHelperGate::for_triple($c, $m, $d, $ord)
    };
}

// ===========================================================================
// H20-1 — the bind-time half: one enumerated address source for every compile
// door, and a `match` the compiler will not let anyone leave incomplete.
//
// # Why the enforcement is here and not on `JitDirectCall`
//
// `H12-1` N1 asked for `CompileAdmission`'s trick to be applied to
// `JitDirectCall`: make it un-constructible outside a constructor that takes a
// policy witness. The analogy is right about the *problem* and wrong about the
// *place*, for a reason that is a property of the backend rather than of taste.
//
// `CompileAdmission` works because it is consumed at a choke point DOWNSTREAM
// of every door (`x64::compile_with_param_slots`) and the refusal is safe
// there: the caller's fallback — "do not compile, keep interpreting" — is
// universal and every door already handles it.
//
// A direct-bind refusal has no such downstream point. `x64/driver.rs`'s
// `reserve_stack_floor` walk states the backend's rule in its own words: *"a
// raw self-call site is an `invokestatic` pc with neither an invoke-info entry
// nor a direct-call plan"*. Every ladder — this crate's two and
// `try_compile_inner`'s two — pushes its row and then `continue`s PAST the
// `invoke_info` construction for that pc (`jit_bridge.rs`'s pushes are at
// 1243/1371/1435, all after the ladder). So a row deleted after the door
// leaves the pc with no metadata at all, and the `0xb8` arm compiles
// `Thread.currentThread()` as a call to the enclosing method. Filtering
// downstream would replace an open door with a wild jump.
//
// `try_compile_inner` already has the only safe shape, and it is the model:
//
//     let entry = direct_native_helper(..);      // 0 == refused
//     if entry != 0 { direct_calls.push(..); continue; }
//     // falls through to the code that builds `invoke_info` for this pc
//
// So the token has to ride on the ENTRY ADDRESS, obtained at the bind site,
// before the `continue`. That is what [`admit_direct_native_entry`] is:
// `Option<usize>`, `None` meaning "fall through", which is byte-for-byte the
// `entry == 0` sentinel the MethodEntry ladder has always used.
//
// # What is compiler-enforced, and what is not
//
// Enforced by this file:
//   * a thirty-second thin helper cannot be added without stating the
//     registered triple it stands in front of — `direct_native_shadows!`
//     takes the triple and the symbol together and writes all three matches
//     from them, so the arms cannot drift and none can be left out;
//   * a door cannot ask for an address without naming which VM it is
//     compiling for and which admission it holds, because those are the only
//     parameters;
//   * **a ladder outside `crate::jit` cannot name a helper item at all.** The
//     thirty-one items are `pub(in crate::jit)` as of 2026-09-22, so
//     `helpers::jit_hashmap_get_direct as *const () as usize` in
//     `vm/src/runtime/interpreter/jit_bridge.rs` is a compile error rather
//     than a bypass a reviewer has to notice.
//
// Enforced by `jit::compile_gate`:
//   * a fourth `CompileDoor` cannot be added without answering
//     `builds_direct_calls`;
//   * a door that never declares a `DirectCallPolicy` has its rows counted by
//     `undeclared_direct_bind_rows`, which all three doors now read zero for
//     and `vm-cli/tests/jit_compile_gate_doors.rs` asserts zero.
//
// This paragraph used to end: *"NOT enforced, and named so it is not mistaken
// for enforced: the raw items below are still `pub` … which is what both
// direct doors do today. Making them `pub(in crate::jit)` is what closes it …
// Lane H20 owns neither and was forbidden to build; H20-1 §6 O1/O2 specify the
// change."* That is what landed on 2026-09-22.
//
// NOT enforced, and named so it is not mistaken for enforced: the eight
// `jdk/internal/misc/Unsafe` scalar accessors are deliberately not
// [`DirectNativeShadow`] variants and are still `pub`, still bound raw at the
// OSR and single-pass doors. `DirectNativeShadow`'s own doc says why, and the
// capability bypass that decision leaves open is tracked at
// `docs/feature-designs/real-jdk-mode.md`.
// `direct_bind_source_witness` below is what notices a helper item widened
// back to `pub` on the way to a thirty-second door.
// ===========================================================================

/// A thin direct-call helper that stands in front of a **registered native**,
/// enumerated so the policy question can be asked once per helper instead of
/// once per door.
///
/// Membership rule, and it is narrower than "everything in `direct_calls`":
/// a variant belongs here iff the emitted `CALL` reaches a VM-side body that
/// re-implements a row in the native registry **and that row is one §7 step 3
/// has something to say about**. That excludes
/// `Math.sqrt` (lowered to `SQRTSD`, shadows nothing), the `String`/`CRC32`
/// call-site intrinsics and the `JitIntrinsic` sentinels, and the
/// `AtomicInteger` family (resolved by `try_resolve_atomic_intrinsic`, which
/// carries its own admission). Those owe no policy question and listing them
/// here would invite a "fix" that is not one — `H12-2` §1's last bullet.
///
/// # 2026-09-22: 9 → 31, and the two lists this is still not
///
/// It was nine when only the nine helpers `H12-2` had measured were spelled,
/// and it stayed nine while twenty-two more thin helpers were added at the
/// doors — which is the same drift `DIRECT_CALL_HELPER_NATIVES` had and for
/// the same reason. `H12-1` O1/O2 asked the OSR and eager-first-call ladders
/// to obtain their addresses through [`admit_direct_native_entry`]; a ladder
/// cannot do that for a helper with no variant, so the population had to
/// become the whole population first.
///
/// **Not `DIRECT_CALL_HELPER_NATIVES`.** That list answers "is this registry
/// row's `invocations` column a floor?" and so carries a row per REGISTRY
/// SLOT: both `ConcurrentMap.get` and `ConcurrentHashMap.get` are on it
/// because either may be the slot a fallback resolves. This one carries a
/// variant per HELPER and names the implementing row only — H7-1's rule, which
/// [`DirectNativeShadow::triple`] restates below.
///
/// **Not the eight `Unsafe` scalar accessors.** `jdk/internal/misc/Unsafe`
/// natives have no bytecode, so §1.4 and §7 step 3 have nothing to prefer over
/// them and `direct_helper_refusal` would admit every one — except that
/// `capability::classify_native` maps the WHOLE class to
/// `CapabilityKind::RawMemory`, so the capability rule would refuse all eight
/// instead. Refusing them here would apply that gate at ONE door of three (the
/// single-pass ladder reads `UnsafeAccessorOp::helper_entry` out of the
/// `DirectHelperTable` and cannot call this function), which is `H12-1`'s own
/// defect with the modes swapped. The bypass is real and is not this
/// enumeration's to close:
/// `docs/feature-designs/real-jdk-mode.md`.
macro_rules! direct_native_shadows {
    ($(
        $(#[$vmeta:meta])*
        $variant:ident = $symbol:ident, ($class:literal, $method:literal, $desc:literal);
    )*) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum DirectNativeShadow {
            $( $(#[$vmeta])* $variant, )*
        }

        impl DirectNativeShadow {
            /// Every shadow, for the doors' sweeps and the tests.
            ///
            /// A slice and not an array: the length is a property of the
            /// invocation below, and a hand-written one is one more thing that
            /// can be wrong about it. It went 9 → 31 in a single change that
            /// would have had to touch the number four times.
            pub const ALL: &'static [DirectNativeShadow] = &[
                $( DirectNativeShadow::$variant, )*
            ];

            /// The registered triple whose native this helper re-implements.
            ///
            /// For `ConcurrentHashMapGet` this is the **implementing** class,
            /// not the `java/util/concurrent/ConcurrentMap` interface the call
            /// site names — H7-1's finding, and the reason
            /// `direct_native_helper_for_impl` exists. A policy question asked
            /// about the interface row is a question about a registration that
            /// does not run.
            pub const fn triple(self) -> (&'static str, &'static str, &'static str) {
                match self {
                    $( DirectNativeShadow::$variant => ($class, $method, $desc), )*
                }
            }

            /// The Rust item name, so a refusal and the source witness can
            /// both name the same symbol the ladders name.
            ///
            /// Test-only: every caller is in this file's `#[cfg(test)]`
            /// module. Nothing in production asks a shadow for its symbol
            /// NAME — the doors take `entry_addr()`, which since `H12-1`
            /// O1/O2 is the only way they can name one at all.
            #[cfg(test)]
            pub const fn helper_symbol(self) -> &'static str {
                match self {
                    $( DirectNativeShadow::$variant => stringify!($symbol), )*
                }
            }

            /// The helper's entry address.
            ///
            /// Taken as `NAME as *const () as usize` rather than read out of
            /// the `*_DIRECT_FN` cell, for the reason both direct doors used
            /// to state at every arm: a door can run before `build_helpers`
            /// has published those cells, so a cell read yields `0` on the
            /// first compile in a process and the bind is silently skipped.
            /// Process-invariant either way — the cells hold exactly these
            /// addresses (`build_helpers`, registered unconditionally since
            /// 2026-08-06).
            ///
            /// Private on purpose, and since 2026-09-22 the items it names are
            /// `pub(in crate::jit)` as well, so a ladder outside this module
            /// cannot reach one without [`admit_direct_native_entry`]. That is
            /// `H12-1` N1's ask, landed at the address rather than at
            /// `JitDirectCall` — see the module block above for why the token
            /// has to ride there.
            pub(in crate::jit::helpers) fn entry_addr(self) -> usize {
                match self {
                    $( DirectNativeShadow::$variant => $symbol as *const () as usize, )*
                }
            }
        }
    };
}



#[cfg(test)]
mod zr7_rv6_box_owed_tests;
#[cfg(test)]
mod j27_clone_copy_tests;
#[cfg(test)]
mod r22_intrinsics_vectorized_mismatch_tests;

#[cfg(test)]
mod str17_vectorized_hash_tests;

#[cfg(test)]
mod r16_strings_decimal_digits_tests;
#[cfg(test)]
mod nio21_volatile_helper_tests;
/// tl21 (TLSUB21-7): the direct-mapped memo keeps a rotation of 32 classes
/// (the old 16-row FIFO list kept none of them), keeps a class's `get` and
/// `set` rows apart, empties on a new epoch, and never serves key 0.
#[cfg(test)]
mod tl21_kind_memo_tests;
/// `R21TlsubRows tl-set-override`: the thin `set` helper and its private-body
/// miss route ask the receiver's own `set(Object)` override before storing
/// (text pins: the helper cannot be driven without compiled code).
#[cfg(test)]
mod tl_set_override_tests;

#[cfg(test)]
mod tl_raw_field_tests;

/// tl21: `jdk.traceVirtualThreadLocals` is read as the JDK reads it (derived
/// from the JDK 21 `ThreadLocal.traceVirtualThreadLocals()` bytecode, not
/// measured on JDK 25): absent and `false` are off, the bare `-D` (an empty
/// value) and any case of `true` are on.
#[cfg(test)]
mod tl21_trace_property_tests;
/// nio21 wave 16, lane L4: text pins for the inline-cache profile seed (the
/// helper cannot be driven without compiled code).
#[cfg(test)]
mod nio21w16_ic_profile_seed_tests;

/// nio21 wave 15, lane L1: text pins for the bound-site inline cache (the
/// helper cannot be driven without compiled code).
#[cfg(test)]
mod nio21w15_bound_site_ic_tests;
#[cfg(test)]
mod gch_h1fr_mic_trace_tests;

/// JIT round 21, lane tlsub (TL321-4): the layout producer's one-class query
/// shares the helper's map-kind encoding, and declines -- writing nothing --
/// without a VM, under `--compatible`, and before the `ThreadLocal` classes
/// are loaded.
#[cfg(test)]
mod jr21_tlsub_receiver_query_tests;

/// j27 wave 3 (lane BACKLOG, STR16-P7): a text pin for the `DecimalDigits`
/// thin helpers (they cannot be driven without compiled code). Every compiled
/// `int`/`long` -> decimal conversion runs this body, so it must not write a
/// process-wide counter per call: a shared cache line every converting thread
/// bounces, for a census nothing read.
#[cfg(test)]
mod j27_backlog_decimal_digits_tests;

mod varhandle_read_direct_counts;
#[allow(unused_imports)]
pub use varhandle_read_direct_counts::*;
#[allow(unused_imports)]
use varhandle_read_direct_counts::*;
mod jit_integer_value_of_direct_body_on;
#[allow(unused_imports)]
pub use jit_integer_value_of_direct_body_on::*;
#[allow(unused_imports)]
use jit_integer_value_of_direct_body_on::*;
mod jit_scoped_memory_put_int_direct;
#[allow(unused_imports)]
pub use jit_scoped_memory_put_int_direct::*;
#[allow(unused_imports)]
use jit_scoped_memory_put_int_direct::*;
mod jit_concurrent_hashmap_get_direct;
#[allow(unused_imports)]
pub use jit_concurrent_hashmap_get_direct::*;
#[allow(unused_imports)]
use jit_concurrent_hashmap_get_direct::*;
pub(super) mod note_ic_profile_seed;
#[allow(unused_imports)]
pub use note_ic_profile_seed::*;
#[allow(unused_imports)]
use note_ic_profile_seed::*;

mod nio_charset_ir_layout_words;
pub(in crate::jit::helpers) use nio_charset_ir_layout_words::*;
