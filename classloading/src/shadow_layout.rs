// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Shadow-layout diff — the overlay detector's per-class instrument.
//!
//! The access-site overlay hunter in `vm/src/vm/vm_exec.rs` fires on a *type*
//! mismatch: a native writing a primitive into a reference slot, or the
//! reverse. That is a real signal, but it is only one of the ways a
//! hand-numbered slot model goes wrong against a real JDK layout, and it is
//! blind to the other two (see
//! `jdk-only-fabricated-object-layouts-FIXED-20260810.md`):
//!
//! * an `Int` written into the *wrong* `Int` slot type-checks and passes; and
//! * a read of a slot whose real field is a different field entirely returns a
//!   perfectly well-typed value of the wrong thing.
//!
//! Neither has a per-access tell. What they do have is a per-*class* one: this
//! VM already writes down what it thinks each slot of a well-known JDK class
//! means, in `ClassManager::synthetic_stub_fields`. That table is the model the
//! natives were written against — it is what sizes a bytecode `new` of the
//! stub, and it is what
//! [`define_class_with_options`](crate::ClassManager::define_class_with_options)
//! pads a *real* class up to so those natives' writes are not discarded.
//!
//! So when a class has both — a model here and real bytes on the class path —
//! the two layouts can simply be diffed, once, at define time. Every slot where
//! they disagree is a slot on which any positional native access is suspect,
//! whatever the value's type tag happens to be. That answers reads and
//! same-kind writes together, at no per-access cost.
//!
//! # What this can and cannot see
//!
//! The model's anonymous slots (`_fN`, declared `Ljava/lang/Object;` — see
//! `instance_fields`) carry an index and nothing else. Against those:
//!
//! * a real **primitive** at that index is a hard disagreement
//!   ([`SlotVerdict::TypeMismatch`]) — the model says "a reference lives here",
//!   the image says otherwise, and that is the family the access-site hunter
//!   already reports for writes and now also reports for reads;
//! * a real **reference** at that index is *unfalsifiable from here*. The model
//!   says `Ljava/lang/Object;` and so does every reference field in the JDK.
//!   This is exactly kind 5 — a VM-internal reference written into a real
//!   reference slot — and no amount of running this diff will find one. Only a
//!   named model, or a behavioural probe diffed against the host JDK, can.
//!
//! Where the model *is* named (the `named_field(...)` arms of
//! `synthetic_stub_fields`) the diff is much sharper: a name that does not match
//! the real declaration at that index is [`SlotVerdict::NameMismatch`], and that
//! catches the reference-into-reference case too. Naming more of the model is
//! therefore the way to widen this instrument.
//!
//! # `_vmN`: the slots that cannot be named right
//!
//! Some slots hold a value the *VM* invented — an fd, a wrapped stream, a
//! discriminator — for which the real class declares no field anywhere. Neither
//! spelling above describes one honestly: naming it after the real field at that
//! index would make the diff agree with an overlay, and leaving it anonymous
//! makes the diff go quiet about one. `_vmN` is the third spelling, and it
//! always reports ([`SlotVerdict::VmInternal`]) when a real field exists
//! underneath. `java/io/BufferedWriter` slot 0 is the worked example: an fd from
//! `Files.newBufferedWriter` sitting on `java.io.Writer.writeBuffer`, which read
//! as an innocuous `pad` for as long as the slot was anonymous.

use crate::class::{Class, ClassId, ClassStore};
use cratonvm_reader::field::ClassFileField;
use std::sync::Arc;

/// How CratonVM's model for one slot compares against the real declaration.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlotVerdict {
    /// The model and the real layout are compatible at this index — same type
    /// class, and (for a named model field) the same name. For an anonymous
    /// model slot over a real *reference* field this means "not falsifiable
    /// from here", not "verified"; see the module docs.
    Agrees,
    /// The model says reference and the image says primitive, or vice versa.
    /// Every positional access to this slot is suspect.
    TypeMismatch,
    /// The model names a field the real class does not declare at this index.
    /// The sharpest verdict this instrument produces, and the only one that
    /// catches a reference written over a different reference.
    NameMismatch,
    /// The model addresses a slot past the end of the real layout. These exist
    /// only because `define_class_with_options` pads real classes up to the
    /// model's slot count; a write here is stored and can never be seen by real
    /// bytecode.
    ModelOverruns,
    /// The model declares this slot `_vmN`: a value the *VM* keeps on the
    /// object — a file descriptor, a wrapped stream, a discriminator — for
    /// which the real JDK class has no field at all. A real field does exist at
    /// this index, so the VM's value is sitting on the JDK's storage.
    ///
    /// This is **kind 3** in
    /// `jdk-only-fabricated-object-layouts-FIXED-20260810.md`,
    /// and it is the one family a corrected model cannot fix: there is nowhere
    /// right to put the value, so it wants a side table (or an index anchored
    /// past the real field count, which reads back as
    /// [`SlotVerdict::ModelOverruns`] — harmless, and the shape to aim for).
    ///
    /// Naming these explicitly is what keeps them countable. The alternative —
    /// leaving the slot anonymous — makes the census go quiet without anything
    /// being fixed, which is exactly how `java/io/BufferedWriter` slot 0 hid a
    /// live fd write behind a `pad`.
    VmInternal,
}

impl SlotVerdict {
    /// Does this verdict mean a positional access to the slot is suspect?
    ///
    /// [`SlotVerdict::ModelOverruns`] deliberately does **not**: a pad slot
    /// belongs to nobody, so writing it corrupts no real field. It is reported
    /// in the census (it says the model is bigger than the class) but it does
    /// not make the access-site hunter fire.
    #[must_use]
    pub fn is_disagreement(self) -> bool {
        matches!(
            self,
            SlotVerdict::TypeMismatch | SlotVerdict::NameMismatch | SlotVerdict::VmInternal
        )
    }

    /// Short tag used in the census output and the per-access log line.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            SlotVerdict::Agrees => "ok",
            SlotVerdict::TypeMismatch => "TYPE",
            SlotVerdict::NameMismatch => "NAME",
            SlotVerdict::ModelOverruns => "pad",
            SlotVerdict::VmInternal => "VM",
        }
    }
}

/// One slot of the diff.
#[derive(Clone, Debug)]
pub struct ShadowSlot {
    /// Absolute instance-field index, which is what positional native access
    /// uses.
    pub index: usize,
    /// The model's field name at this index (`_fN` when anonymous).
    pub model_name: Arc<str>,
    /// The model's declared descriptor (`Ljava/lang/Object;` when anonymous).
    pub model_desc: Arc<str>,
    /// The real class's field name at this index, if it has one.
    pub real_name: Option<Arc<str>>,
    /// The real class's declared descriptor at this index, if it has one.
    pub real_desc: Option<Arc<str>>,
    pub verdict: SlotVerdict,
}

/// The whole diff for one class: what our model claims, slot by slot, against
/// what the loaded image declares.
#[derive(Clone, Debug)]
pub struct ShadowLayoutDiff {
    pub class_name: Arc<str>,
    pub slots: Vec<ShadowSlot>,
    /// `index -> is_disagreement`, sized to `slots.len()`. Kept separate so the
    /// access-site hunter's check is an array index, not a scan.
    disagreeing: Box<[bool]>,
    disagreement_count: usize,
}

impl ShadowLayoutDiff {
    /// Is a positional access to `index` addressing a slot where the model and
    /// the image disagree?
    #[must_use]
    pub fn is_disagreeing(&self, index: usize) -> bool {
        self.disagreeing.get(index).copied().unwrap_or(false)
    }

    /// The slot record for `index`, if the model covers it.
    #[must_use]
    pub fn slot(&self, index: usize) -> Option<&ShadowSlot> {
        self.slots.get(index)
    }

    /// How many slots carry a [`SlotVerdict::is_disagreement`] verdict.
    #[must_use]
    pub fn disagreement_count(&self) -> usize {
        self.disagreement_count
    }

    /// Render the census block for this class. One line per slot, so a reader
    /// can map a native's hand-numbered constant onto the real field directly.
    #[must_use]
    pub fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "[OVERLAY-LAYOUT] {} — model has {} slot(s), {} disagree with the loaded image",
            self.class_name,
            self.slots.len(),
            self.disagreement_count,
        );
        for s in &self.slots {
            let _ = writeln!(
                out,
                "[OVERLAY-LAYOUT]   slot {:>2} {:<4} model={}:{} real={}:{}",
                s.index,
                s.verdict.tag(),
                s.model_name,
                s.model_desc,
                s.real_name.as_deref().unwrap_or("<none>"),
                s.real_desc.as_deref().unwrap_or("<none>"),
            );
        }
        out
    }
}

/// Reference-or-primitive. The only distinction a descriptor makes that a
/// positional write can violate without being caught by the value's type tag.
fn is_reference_descriptor(desc: &str) -> bool {
    matches!(desc.as_bytes().first(), Some(b'L') | Some(b'['))
}

/// Is this model field one of `instance_fields`/`pad_to`'s anonymous slots?
///
/// Anonymous slots carry an index and no meaning, so their *name* cannot
/// disagree with anything — only their type class can.
fn is_anonymous_model_field(name: &str) -> bool {
    name.strip_prefix("_f")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// Is this model field one of `vm_internal_field`'s `_vmN` slots — a value the
/// VM parks on the object for which the real class declares no field?
///
/// Distinct from an anonymous `_fN` slot, which only means "the model has
/// nothing to say here". `_vmN` is a positive claim, and against a real layout
/// it is always a finding — see [`SlotVerdict::VmInternal`].
fn is_vm_internal_model_field(name: &str) -> bool {
    name.strip_prefix("_vm")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// The real instance field at absolute index `index`, walking the superclass
/// chain.
///
/// Mirrors `Class::field_at_index` plus the superclass step, and — like every
/// other slot walker in the tree — skips **static** fields, because
/// `self.fields` interleaves statics with instance fields in declaration order
/// (`java.util.regex.Matcher` declares two static constants in the middle of
/// its instance fields).
///
/// Returns `None` when the index is past the end of the real layout, or when
/// any class on the chain is itself a synthetic stub — a stub's `_fN`
/// descriptors are placeholders and diffing a model against a model answers
/// nothing.
fn real_field_at_index<'a>(
    store: &'a ClassStore,
    class_id: ClassId,
    index: usize,
) -> Option<&'a ClassFileField> {
    let mut cid = Some(class_id);
    while let Some(id) = cid {
        let cls = store.get(id)?;
        if cls.origin.is_compatibility_stub() {
            return None;
        }
        if index >= cls.first_field_index {
            let local = index - cls.first_field_index;
            let mut instance_idx = 0usize;
            for f in &cls.fields {
                if f.is_static() {
                    continue;
                }
                if instance_idx == local {
                    return Some(f);
                }
                instance_idx += 1;
            }
            // The index lands in this class's range but past its declared
            // fields: it is padding, not a field.
            return None;
        }
        cid = cls.superclass;
    }
    None
}

/// One `JDK-ONLY-LAYOUT: safe` claim, made checkable.
///
/// A `safe` verdict in the marker sweep says "this raw slot index really is
/// that field on the JDK we support". Every one of them was verified by a
/// person reading `javap` once, and nothing in the build re-checked it — so a
/// JDK upgrade that reorders a private field would not fail a test, it would
/// silently corrupt an object. That is wave-2 step 4, and this is its type: the
/// claim written down in a form the loaded image can contradict.
///
/// A claim names a field by index AND descriptor. The descriptor matters: two
/// adjacent `int`s reorder without the name check noticing anything if only the
/// type is compared, and two fields of different types can swap without the
/// index moving.
#[derive(Clone, Copy, Debug)]
pub struct PositionalClaim {
    /// Internal class name the claim is about.
    pub class: &'static str,
    /// Absolute instance-field index, which is what the raw access uses.
    pub index: usize,
    /// The field the code believes is at `index`.
    pub name: &'static str,
    /// Its declared descriptor.
    pub descriptor: &'static str,
    /// Where the claim is made, so a failure names the code to fix.
    pub site: &'static str,
}

/// Every `JDK-ONLY-LAYOUT: safe` positional claim in the tree.
///
/// Adding a `safe` marker without adding its row here is the failure mode this
/// table exists to prevent, and `every_safe_claim_names_a_real_site` keeps the
/// two spellings from drifting into different vocabularies.
pub const SAFE_POSITIONAL_CLAIMS: &[PositionalClaim] = &[
    // `vm/src/vm/vm_object.rs:28` — the file-level anchor every `java/lang/String`
    // slot literal in that file inherits. JDK 9+ compact strings.
    PositionalClaim {
        class: "java/lang/String",
        index: 0,
        name: "value",
        descriptor: "[B",
        site: "vm/src/vm/vm_object.rs String slot anchor",
    },
    PositionalClaim {
        class: "java/lang/String",
        index: 1,
        name: "coder",
        descriptor: "B",
        site: "vm/src/vm/vm_object.rs String slot anchor",
    },
    PositionalClaim {
        class: "java/lang/String",
        index: 2,
        name: "hash",
        descriptor: "I",
        site: "vm/src/vm/vm_object.rs String slot anchor",
    },
    PositionalClaim {
        class: "java/lang/String",
        index: 3,
        name: "hashIsZero",
        descriptor: "Z",
        site: "vm/src/vm/vm_object.rs String slot anchor",
    },
    // `vm/src/vm/vm_util.rs` post-clinit fixups.
    PositionalClaim {
        class: "java/util/concurrent/atomic/AtomicInteger",
        index: 0,
        name: "value",
        descriptor: "I",
        site: "vm/src/vm/vm_util.rs ServiceContainerImpl fixup",
    },
    PositionalClaim {
        class: "sun/text/normalizer/NormalizerBase$ModeImpl",
        index: 0,
        name: "normalizer2",
        descriptor: "Lsun/text/normalizer/Normalizer2;",
        site: "vm/src/vm/vm_util.rs NormalizerBase fixup",
    },
];

/// Check every claim this table makes about `class_id`, against the layout the
/// image actually declares.
///
/// Returns one message per BROKEN claim, empty when the class carries no claim
/// or every claim holds. Callers report these unconditionally: the whole point
/// is that a JDK upgrade is loud, so this must not sit behind a debug flag the
/// way the census does.
///
/// A claim about a class that is itself a fabricated stub is skipped rather
/// than failed — there is no image to contradict it, and `--real-jdk`'s own
/// machinery is what refuses fabrication.
#[must_use]
pub fn check_positional_claims(store: &ClassStore, class_id: ClassId) -> Vec<String> {
    let Some(class) = store.get(class_id) else {
        return Vec::new();
    };
    if class.origin.is_compatibility_stub() {
        return Vec::new();
    }
    let name = &*class.name;
    let mut broken = Vec::new();
    for claim in SAFE_POSITIONAL_CLAIMS.iter().filter(|c| c.class == name) {
        match real_field_at_index(store, class_id, claim.index) {
            Some(f) if &*f.name == claim.name && &*f.descriptor == claim.descriptor => {}
            Some(f) => broken.push(format!(
                "{} slot {} is `{}:{}` on this image, not `{}:{}` ({})",
                name, claim.index, f.name, f.descriptor, claim.name, claim.descriptor, claim.site,
            )),
            None => broken.push(format!(
                "{} slot {} does not exist on this image; `{}:{}` was assumed ({})",
                name, claim.index, claim.name, claim.descriptor, claim.site,
            )),
        }
    }
    broken
}

/// Diff `model` (CratonVM's `synthetic_stub_fields` entry for the class) against
/// the real layout of `class_id`.
///
/// `model` is the full field list from the table, statics included; only the
/// instance fields participate, in declaration order, starting at absolute index
/// 0 — because that is how positional native access indexes an object, and the
/// whole point of this diff is to describe what those natives actually hit.
///
/// Returns `None` when there is nothing to say: no model, or the class is
/// itself a fabricated stub (so there is no real layout to disagree with).
#[must_use]
pub fn diff_against_model(
    store: &ClassStore,
    class_id: ClassId,
    model: &[ClassFileField],
) -> Option<ShadowLayoutDiff> {
    let class: &Class = store.get(class_id)?;
    if class.origin.is_compatibility_stub() {
        return None;
    }
    let model_instance: Vec<&ClassFileField> = model.iter().filter(|f| !f.is_static()).collect();
    if model_instance.is_empty() {
        return None;
    }

    let mut slots = Vec::with_capacity(model_instance.len());
    let mut disagreeing = Vec::with_capacity(model_instance.len());
    let mut disagreement_count = 0usize;
    for (index, mf) in model_instance.iter().enumerate() {
        let real = real_field_at_index(store, class_id, index);
        let verdict = match real {
            // A `_vmN` slot past the real layout is the shape we WANT — the
            // value is anchored on padding nobody else owns — so the overrun
            // arm has to come first, and it stays a `pad`, not a finding.
            None => SlotVerdict::ModelOverruns,
            Some(_) if is_vm_internal_model_field(&mf.name) => SlotVerdict::VmInternal,
            Some(rf) => {
                if is_reference_descriptor(&mf.descriptor)
                    != is_reference_descriptor(&rf.descriptor)
                {
                    SlotVerdict::TypeMismatch
                } else if !is_anonymous_model_field(&mf.name) && mf.name != rf.name {
                    SlotVerdict::NameMismatch
                } else {
                    SlotVerdict::Agrees
                }
            }
        };
        if verdict.is_disagreement() {
            disagreement_count += 1;
        }
        disagreeing.push(verdict.is_disagreement());
        slots.push(ShadowSlot {
            index,
            model_name: Arc::clone(&mf.name),
            model_desc: Arc::clone(&mf.descriptor),
            real_name: real.map(|f| Arc::clone(&f.name)),
            real_desc: real.map(|f| Arc::clone(&f.descriptor)),
            verdict,
        });
    }

    Some(ShadowLayoutDiff {
        class_name: Arc::clone(&class.name),
        slots,
        disagreeing: disagreeing.into_boxed_slice(),
        disagreement_count,
    })
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod production_model_order_tests;
