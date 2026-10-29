// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 5 (lane PROXY; page
//! `docs/known-issues/jit/proxy-row-residual-is-classfile-api-code-quality-and-flat-vm-dispatch-cost-20261024.md`,
//! "the getters' inlining"; `CRATONVM_JIT_IR_CHA_INTERFACE_CLASS_SET_SPLICE`,
//! perf, **opt-in**): the CLASS-SET guard of an `invokeinterface` site.
//!
//! The ClassFile API reaches `AbstractPoolEntry.index()` (and its siblings)
//! through `PoolEntry`, an interface: `BufWriterImpl.writeIndex` calls
//! `maybeClone(..).index()` on every pool reference it writes. Seventeen
//! concrete entry classes inherit the one body, so the site is megamorphic
//! for the receiver profile and every call pays the inline-cache helper door
//! (`jit_invoke_virtual_mic_body`). The r23 CHA watermark splice
//! (`CRATONVM_JIT_IR_CHA_MONOMORPHIC_SPLICE`) covers `invokevirtual` only: the
//! verifier does not check an `invokeinterface` receiver, so "every class
//! below the watermark" is not a sound hit test there.
//!
//! The guard here needs no hierarchy assumption at all. The VM lists the
//! loaded classes `C` for which an `invokeinterface` on an object of EXACTLY
//! `C` selects the spliced body (`ClassRealm::cha_interface_class_set`: `C`
//! implements the site's interface, and its superclass chain reaches the
//! body's declaring class with no other declaration of the method on the
//! way), and the hit test is `word == c0 | word == c1 | ...` on the
//! receiver's class word. Each member is an exact class, so every argument of
//! the one-class guarded splice (`IrBuilder::begin_guarded_receiver_splice`)
//! holds member by member: class ids are never reused, a class defined later
//! is not in the set and takes the call, and a null (word `0`) or a lambda
//! proxy (no member is one) misses too. Only the body is shared, which is why
//! the body is built with no exact-class fact about its receiver, as at a
//! watermark site.
//!
//! Opt-in: it cannot be measured without a build, and the r23 triage measured
//! the `invokevirtual` twin neutral on this row.

// The VM's profile-receiver resolver carries the set to the planner on the
// answer it already returns (`InlineSite::cha_class_set`): no hand-off state.
use super::{CmpOp, IrBuilder, IrType, NodeId, Op};

/// The most classes one site's set test compares (the profile's class first).
/// A longer list is cut: any subset is sound, the rest take the call.
pub const CHA_CLASS_SET_MAX: usize = 16;

/// `CRATONVM_JIT_IR_CHA_INTERFACE_CLASS_SET_SPLICE` (perf, opt-in, `=1`
/// enables). Read per compile and per planned site, no `static`.
pub fn ir_cha_interface_class_set_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_CHA_INTERFACE_CLASS_SET_SPLICE")
}

/// The receiver class the planner seeds a class-set site with when every
/// other receiver reader declined the site: the profile's top class at any
/// share, at an `invokeinterface` whose profile is neither bimorphic nor
/// (with `polymorphic_on`) polymorphic. Those shapes keep their own plans.
/// `None` with the switch off. Without a set from the VM the plan is the
/// one-class guarded plan, which the profile then rejects as a guard, so the
/// site stays the call it was.
pub(crate) fn cha_class_set_seed_class(
    profile: Option<&crate::profile::MethodProfile>,
    pc: usize,
    opcode: u8,
    polymorphic_on: bool,
) -> Option<u32> {
    if opcode != 0xb9 || !ir_cha_interface_class_set_enabled() {
        return None;
    }
    let counts = profile.and_then(|p| p.receivers.get(&pc))?;
    let evidence = crate::profile::ReceiverEvidence::of(counts);
    if evidence.summary().observations < crate::MIN_GUARDED_RECEIVER_EVIDENCE
        || super::profile_bimorphic_receiver_classes(profile, pc).is_some()
        || (polymorphic_on && super::profile_polymorphic_receiver_classes(profile, pc).is_some())
    {
        return None;
    }
    evidence.seed_a_guarded_cache(1).filter(|&class_id| class_id != 0)
}

/// The set a site may test: `first_class` first, then the other listed ids in
/// order, without `0`, ids outside the `int` range (the class word is read
/// sign-extended) or repeats, cut at [`CHA_CLASS_SET_MAX`]. `None` unless
/// `first_class` is listed and at least one other class is: one class is the
/// one-class guarded splice's job. Pure.
pub(crate) fn admitted_class_set(class_ids: &[u32], first_class: u32) -> Option<Vec<u32>> {
    let usable = |id: u32| id != 0 && id <= i32::MAX as u32;
    if !usable(first_class) || !class_ids.contains(&first_class) {
        return None;
    }
    let mut set = vec![first_class];
    for &id in class_ids {
        if set.len() >= CHA_CLASS_SET_MAX {
            break;
        }
        if usable(id) && !set.contains(&id) {
            set.push(id);
        }
    }
    (set.len() >= 2).then_some(set)
}

impl IrBuilder {
    /// The guarded site at `caller_pc` (registered with
    /// [`Self::add_exact_receiver_site`] for `first_class` and
    /// [`Self::add_guarded_receiver_site`]) tests its receiver's class word
    /// against every class of `class_ids` instead of `first_class` alone
    /// ([`Self::cha_class_set_receiver_test`]), and its body is built with no
    /// exact-class fact about the receiver. A no-op answering `false` for a
    /// set [`admitted_class_set`] refuses; `true` once registered.
    pub fn add_cha_class_set_site(
        &mut self,
        caller_pc: usize,
        first_class: u32,
        class_ids: &[u32],
    ) -> bool {
        match admitted_class_set(class_ids, first_class) {
            Some(set) => {
                self.cha_class_set_sites.insert(caller_pc, set);
                true
            }
            None => false,
        }
    }

    /// The class set of the OUTERMOST site at `pc`, when it has one.
    pub(super) fn cha_class_set_at(&self, pc: usize) -> Option<Vec<u32>> {
        if self.splice.is_empty() {
            self.cha_class_set_sites.get(&pc).cloned()
        } else {
            None
        }
    }

    /// Is the OUTERMOST site at `pc` guarded by a class-hierarchy test (a
    /// watermark or a class set) rather than one exact class? Such a site's
    /// hit edge proves no single class, so its body gets no exact-class fact
    /// and never takes a frameless forwarder.
    pub(super) fn cha_guarded_site(&self, pc: usize) -> bool {
        self.splice.is_empty()
            && (self.cha_watermark_sites.contains_key(&pc)
                || self.cha_class_set_sites.contains_key(&pc))
    }

    /// The hit test of a class-set site: `(word == c0) | (word == c1) | ...`
    /// on the receiver's class word ([`Op::ClassWordOf`]), `0` for a null.
    pub(super) fn cha_class_set_receiver_test(
        &mut self,
        receiver: NodeId,
        class_ids: &[u32],
        pc: usize,
    ) -> NodeId {
        let word = self.add_data(Op::ClassWordOf, IrType::Int, vec![receiver], pc);
        let mut hit: Option<NodeId> = None;
        for &class_id in class_ids {
            // Cast: `admitted_class_set` keeps only ids in the `int` range.
            let id = self.iconst(i64::from(class_id as i32));
            let is = self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![word, id], pc);
            hit = Some(match hit {
                Some(prev) => self.add_data(Op::Or, IrType::Int, vec![prev, is], pc),
                None => is,
            });
        }
        // An empty list never reaches here (`admitted_class_set`); answer
        // "miss" rather than panic if one ever does.
        match hit {
            Some(hit) => hit,
            None => self.iconst(0),
        }
    }
}
