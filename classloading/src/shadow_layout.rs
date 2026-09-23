// SPDX-License-Identifier: Apache-2.0
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
/// than failed — there is no image to contradict it, and `--jdk-only`'s own
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
mod tests {
    use super::*;
    use crate::class::{ClassLoaderId, ClassState};
    use crate::class_origin::ClassOrigin;
    use cratonvm_reader::class_access_flags::{ClassAccessFlags, FieldAccessFlags};
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

    fn field(name: &str, desc: &str) -> ClassFileField {
        ClassFileField {
            access_flags: FieldAccessFlags::empty(),
            name: cratonvm_types::intern_arc(name),
            descriptor: cratonvm_types::intern_arc(desc),
            attributes: vec![],
        }
    }

    fn static_field(name: &str, desc: &str) -> ClassFileField {
        ClassFileField {
            access_flags: FieldAccessFlags::STATIC,
            name: cratonvm_types::intern_arc(name),
            descriptor: cratonvm_types::intern_arc(desc),
            attributes: vec![],
        }
    }

    fn anon(n: usize) -> Vec<ClassFileField> {
        (0..n)
            .map(|i| field(&format!("_f{i}"), "Ljava/lang/Object;"))
            .collect()
    }

    fn make_class(
        id: ClassId,
        name: &str,
        superclass: Option<ClassId>,
        fields: Vec<ClassFileField>,
        first_field_index: usize,
        num_total_fields: usize,
    ) -> Class {
        Class {
            id,
            loader_id: ClassLoaderId::Bootstrap,
            name: cratonvm_types::intern_arc(name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass,
            interfaces: vec![],
            fields,
            methods: vec![],
            first_field_index,
            num_total_fields,
            bootstrap_methods: vec![],
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: ClassOrigin::default(),
            has_finalizer: false,
            code_source: None,
            array_info: None,
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
        }
    }

    /// Register a real (non-stub) class with `fields`, optionally under a
    /// superclass, and return its id.
    fn add_real(
        store: &mut ClassStore,
        name: &str,
        superclass: Option<ClassId>,
        fields: Vec<ClassFileField>,
    ) -> ClassId {
        let first_field_index = superclass
            .and_then(|s| store.get(s))
            .map_or(0, |c| c.num_total_fields);
        let own_instance = fields.iter().filter(|f| !f.is_static()).count();
        let id = store.next_id();
        let class = make_class(
            id,
            name,
            superclass,
            fields,
            first_field_index,
            first_field_index + own_instance,
        );
        store.add(class);
        id
    }

    #[test]
    fn anonymous_model_over_a_real_primitive_is_a_type_mismatch() {
        let mut store = ClassStore::new();
        // Real class: slot 0 reference, slot 1 int.
        let cid = add_real(
            &mut store,
            "java/util/Fake",
            None,
            vec![field("defaults", "Ljava/util/Fake;"), field("count", "I")],
        );
        let diff = diff_against_model(&store, cid, &anon(2)).expect("diff");
        assert_eq!(diff.slot(0).unwrap().verdict, SlotVerdict::Agrees);
        assert_eq!(diff.slot(1).unwrap().verdict, SlotVerdict::TypeMismatch);
        assert!(diff.is_disagreeing(1));
        assert!(!diff.is_disagreeing(0));
        assert_eq!(diff.disagreement_count(), 1);
    }

    /// Gap 2, the one the type-based hunter cannot see: our model and the image
    /// agree that the slot holds an `int`, and it is still the wrong `int`.
    /// Only a NAMED model can say so.
    #[test]
    fn named_model_catches_a_same_kind_wrong_slot() {
        let mut store = ClassStore::new();
        let cid = add_real(
            &mut store,
            "java/util/Fake",
            None,
            vec![field("threshold", "I"), field("modCount", "I")],
        );
        let model = vec![field("size", "I"), field("modCount", "I")];
        let diff = diff_against_model(&store, cid, &model).expect("diff");
        assert_eq!(
            diff.slot(0).unwrap().verdict,
            SlotVerdict::NameMismatch,
            "model `size` over real `threshold` — same type class, different field"
        );
        assert_eq!(diff.slot(1).unwrap().verdict, SlotVerdict::Agrees);
        assert_eq!(diff.disagreement_count(), 1);
    }

    /// Kind 3: the model parks a VM value on a slot the JDK owns. Neither
    /// alternative spelling reports it — an anonymous `_fN` over a real
    /// reference is unfalsifiable, and naming the slot after the real field
    /// makes the diff *agree* with the overlay. `_vmN` is the only spelling
    /// that says "a value with no home is living here".
    #[test]
    fn a_vm_internal_slot_over_a_real_field_is_reported() {
        let mut store = ClassStore::new();
        let cid = add_real(
            &mut store,
            "java/io/Writer",
            None,
            vec![
                field("writeBuffer", "[C"),
                field("lock", "Ljava/lang/Object;"),
            ],
        );
        // What `java/io/BufferedWriter`'s model says today.
        let model = vec![
            field("_vm0", "Ljava/lang/Object;"),
            field("lock", "Ljava/lang/Object;"),
        ];
        let diff = diff_against_model(&store, cid, &model).expect("diff");
        assert_eq!(diff.slot(0).unwrap().verdict, SlotVerdict::VmInternal);
        assert_eq!(diff.slot(0).unwrap().verdict.tag(), "VM");
        assert!(diff.is_disagreeing(0));
        assert_eq!(diff.slot(1).unwrap().verdict, SlotVerdict::Agrees);
        assert_eq!(diff.disagreement_count(), 1);

        // The control: spelling the same slot anonymously reports NOTHING, which
        // is the state this verdict exists to end. If this half ever starts
        // failing, `_vmN` has stopped being load-bearing.
        let anonymous = vec![
            field("_f0", "Ljava/lang/Object;"),
            field("lock", "Ljava/lang/Object;"),
        ];
        let quiet = diff_against_model(&store, cid, &anonymous).expect("diff");
        assert_eq!(quiet.disagreement_count(), 0);
    }

    /// A `_vmN` past the real field count is the SHAPE TO AIM FOR — the value
    /// sits on padding nobody owns — so it must not be reported. Otherwise
    /// fixing an overlay by anchoring it past the layout would look like
    /// causing one.
    #[test]
    fn a_vm_internal_slot_anchored_past_the_layout_is_a_pad() {
        let mut store = ClassStore::new();
        let cid = add_real(
            &mut store,
            "java/io/Writer",
            None,
            vec![field("writeBuffer", "[C")],
        );
        let model = vec![
            field("writeBuffer", "[C"),
            field("_vm1", "Ljava/lang/Object;"),
        ];
        let diff = diff_against_model(&store, cid, &model).expect("diff");
        assert_eq!(diff.slot(1).unwrap().verdict, SlotVerdict::ModelOverruns);
        assert_eq!(diff.disagreement_count(), 0);
    }

    #[test]
    fn model_slots_past_the_real_layout_are_pad_not_disagreements() {
        let mut store = ClassStore::new();
        let cid = add_real(
            &mut store,
            "java/util/Fake",
            None,
            vec![field("map", "Ljava/util/Map;")],
        );
        let diff = diff_against_model(&store, cid, &anon(3)).expect("diff");
        assert_eq!(diff.slot(0).unwrap().verdict, SlotVerdict::Agrees);
        assert_eq!(diff.slot(1).unwrap().verdict, SlotVerdict::ModelOverruns);
        assert_eq!(diff.slot(2).unwrap().verdict, SlotVerdict::ModelOverruns);
        assert_eq!(
            diff.disagreement_count(),
            0,
            "a pad slot belongs to no real field, so writing it corrupts nothing"
        );
    }

    /// The real `java.util.Properties` shape: the inherited `Hashtable` fields
    /// push the interesting slots down, which is the whole reason a model
    /// numbered from 0 lands on the wrong ones.
    #[test]
    fn inherited_fields_are_walked_so_absolute_indices_line_up() {
        let mut store = ClassStore::new();
        let sup = add_real(
            &mut store,
            "java/util/Hashtable",
            None,
            vec![
                field("table", "[Ljava/util/Hashtable$Entry;"),
                field("count", "I"),
                field("threshold", "I"),
                field("loadFactor", "F"),
            ],
        );
        let cid = add_real(
            &mut store,
            "java/util/Properties",
            Some(sup),
            vec![field("defaults", "Ljava/util/Properties;")],
        );
        let diff = diff_against_model(&store, cid, &anon(6)).expect("diff");
        assert_eq!(diff.slot(0).unwrap().real_name.as_deref(), Some("table"));
        assert_eq!(diff.slot(1).unwrap().real_name.as_deref(), Some("count"));
        assert_eq!(
            diff.slot(3).unwrap().real_name.as_deref(),
            Some("loadFactor")
        );
        assert_eq!(diff.slot(4).unwrap().real_name.as_deref(), Some("defaults"));
        // 1, 2 and 3 are primitives under an anonymous reference model.
        assert_eq!(diff.slot(1).unwrap().verdict, SlotVerdict::TypeMismatch);
        assert_eq!(diff.slot(2).unwrap().verdict, SlotVerdict::TypeMismatch);
        assert_eq!(diff.slot(3).unwrap().verdict, SlotVerdict::TypeMismatch);
        assert_eq!(diff.slot(5).unwrap().verdict, SlotVerdict::ModelOverruns);
    }

    /// `Class::fields` interleaves statics with instance fields, so a walker
    /// that indexes the raw vec reports the wrong field. `Matcher` is the
    /// in-tree example (two static constants in the middle of its instance
    /// fields) and it has already caused one silent mis-typing.
    #[test]
    fn static_fields_do_not_shift_the_instance_indices() {
        let mut store = ClassStore::new();
        let cid = add_real(
            &mut store,
            "java/util/Fake",
            None,
            vec![
                static_field("serialVersionUID", "J"),
                field("key", "[B"),
                static_field("NOANCHOR", "I"),
                field("algorithm", "Ljava/lang/String;"),
            ],
        );
        let diff = diff_against_model(&store, cid, &anon(2)).expect("diff");
        assert_eq!(diff.slot(0).unwrap().real_name.as_deref(), Some("key"));
        assert_eq!(
            diff.slot(1).unwrap().real_name.as_deref(),
            Some("algorithm")
        );
        assert_eq!(diff.disagreement_count(), 0);
    }

    #[test]
    fn a_synthetic_stub_has_no_real_layout_to_disagree_with() {
        let mut store = ClassStore::new();
        let id = store.next_id();
        let mut class = make_class(id, "java/util/Fake", None, anon(2), 0, 2);
        class.set_origin(crate::class_origin::ClassOrigin::compatibility_stub(
            "test fixture",
        ));
        store.add(class);
        assert!(diff_against_model(&store, id, &anon(2)).is_none());
    }

    #[test]
    fn an_empty_model_produces_no_diff() {
        let mut store = ClassStore::new();
        let cid = add_real(&mut store, "java/util/Fake", None, vec![field("a", "I")]);
        assert!(diff_against_model(&store, cid, &[]).is_none());
        assert!(diff_against_model(&store, cid, &[static_field("S", "I")]).is_none());
    }

    /// The injected-violation test for gaps 1 and 2, run against the
    /// **production** model table rather than a fixture of one.
    ///
    /// `java/util/Properties` is `instance_fields(16)` — sixteen anonymous
    /// reference slots — and a real `Properties` inherits `Hashtable`'s
    /// primitives at 1..=4. Every one of those is a slot on which a positional
    /// native access is addressing a different field than it thinks, and none
    /// of them has a per-access tell unless the value happens to be a reference.
    ///
    /// A detector that reports nothing on injected input is decoration; this
    /// asserts a floor on what it finds, so a table edit that quietly empties
    /// the model fails here instead of turning the census silently green.
    #[test]
    fn the_production_model_for_properties_disagrees_with_a_real_hashtable_layout() {
        let model = crate::class_manager::synthetic_stub_field_model("java/util/Properties");
        assert!(
            model.iter().filter(|f| !f.is_static()).count() >= 8,
            "the Properties model must still have slots for this test to mean anything"
        );
        let mut store = ClassStore::new();
        // JDK 25 `java.util.Hashtable`, in declaration order.
        let sup = add_real(
            &mut store,
            "java/util/Hashtable",
            None,
            vec![
                field("table", "[Ljava/util/Hashtable$Entry;"),
                field("count", "I"),
                field("threshold", "I"),
                field("loadFactor", "F"),
                field("modCount", "I"),
            ],
        );
        let cid = add_real(
            &mut store,
            "java/util/Properties",
            Some(sup),
            vec![
                field("defaults", "Ljava/util/Properties;"),
                field("map", "Ljava/util/concurrent/ConcurrentHashMap;"),
            ],
        );
        let diff = diff_against_model(&store, cid, &model).expect("Properties has a model");
        for slot in [1usize, 2, 3, 4] {
            assert_eq!(
                diff.slot(slot).unwrap().verdict,
                SlotVerdict::TypeMismatch,
                "slot {slot} is an inherited Hashtable primitive under a reference model",
            );
            assert!(diff.is_disagreeing(slot));
        }
        // Slot 3 is the measured one: `PROPS_FIELD_DEFAULTS` in
        // `native-collections`, `float loadFactor` in the image.
        assert_eq!(
            diff.slot(3).unwrap().real_name.as_deref(),
            Some("loadFactor")
        );
        assert!(diff.disagreement_count() >= 4);
        // And the rendering has to actually name it, or the census is useless.
        let rendered = diff.render();
        assert!(rendered.contains("loadFactor"), "{rendered}");
        assert!(rendered.contains("TYPE"), "{rendered}");
    }

    /// Wave-2 step 4's non-vacuity check: the claim checker must go RED on a
    /// layout that contradicts a claim, and green on the one it was written
    /// against.
    ///
    /// Both halves matter. A checker that never fires is decoration, and one
    /// that fires on the correct layout would have to be switched off the first
    /// time it cried wolf.
    #[test]
    fn a_safe_positional_claim_is_checked_against_the_image() {
        // JDK 9+ `java.lang.String`, in declaration order — what every claim
        // about it asserts.
        let mut store = ClassStore::new();
        let cid = add_real(
            &mut store,
            "java/lang/String",
            None,
            vec![
                field("value", "[B"),
                field("coder", "B"),
                field("hash", "I"),
                field("hashIsZero", "Z"),
            ],
        );
        assert!(
            check_positional_claims(&store, cid).is_empty(),
            "the claims must hold against the layout they were written for"
        );

        // Pre-9 `String`: `char[] value; int hash;`. Slot 1 is `hash`, not
        // `coder` — the exact regression the marker's own text warns about.
        let mut store = ClassStore::new();
        let cid = add_real(
            &mut store,
            "java/lang/String",
            None,
            vec![field("value", "[C"), field("hash", "I")],
        );
        let broken = check_positional_claims(&store, cid);
        assert_eq!(
            broken.len(),
            4,
            "all four String claims should break on a pre-9 layout, got {broken:?}"
        );
        assert!(broken[0].contains("value"), "{broken:?}");
        assert!(broken[1].contains("coder"), "{broken:?}");
        assert!(
            broken[3].contains("does not exist"),
            "slot 3 is past a two-field layout: {broken:?}"
        );

        // A same-name, different-TYPE field must still break the claim: two
        // ints reorder without a name check noticing.
        let mut store = ClassStore::new();
        let cid = add_real(
            &mut store,
            "java/util/concurrent/atomic/AtomicInteger",
            None,
            vec![field("value", "J")],
        );
        assert_eq!(check_positional_claims(&store, cid).len(), 1);
    }

    /// A class with no claim, and a fabricated stub, are both silent — so the
    /// tripwire cannot become background noise on a synthetic boot.
    #[test]
    fn claims_are_silent_for_unclaimed_and_fabricated_classes() {
        let mut store = ClassStore::new();
        let cid = add_real(&mut store, "java/util/Fake", None, vec![field("a", "I")]);
        assert!(check_positional_claims(&store, cid).is_empty());

        let id = store.next_id();
        let mut class = make_class(id, "java/lang/String", None, anon(1), 0, 1);
        class.set_origin(crate::class_origin::ClassOrigin::compatibility_stub(
            "test fixture",
        ));
        store.add(class);
        assert!(
            check_positional_claims(&store, id).is_empty(),
            "a fabricated stub has no image to contradict a claim"
        );
    }

    /// Every claim names a site and a plausible descriptor, and the table is
    /// not empty — the cheapest way for step 4 to become vacuous is for
    /// somebody to empty this table while the checker stays wired in.
    #[test]
    fn every_safe_claim_names_a_real_site() {
        assert!(SAFE_POSITIONAL_CLAIMS.len() >= 6);
        for c in SAFE_POSITIONAL_CLAIMS {
            assert!(!c.class.is_empty() && c.class.contains('/'), "{c:?}");
            assert!(!c.name.is_empty(), "{c:?}");
            assert!(!c.descriptor.is_empty(), "{c:?}");
            assert!(c.site.contains(".rs"), "a claim must name its code: {c:?}");
        }
    }

    #[test]
    fn anonymous_model_field_recognition() {
        assert!(is_anonymous_model_field("_f0"));
        assert!(is_anonymous_model_field("_f17"));
        assert!(!is_anonymous_model_field("_f"));
        assert!(!is_anonymous_model_field("_fx"));
        assert!(!is_anonymous_model_field("loadFactor"));
        assert!(!is_anonymous_model_field("f0"));
    }
}

#[cfg(test)]
mod production_model_order_tests {
    use super::*;
    use crate::class_manager::synthetic_stub_field_model;

    fn instance_names(class: &str) -> Vec<String> {
        synthetic_stub_field_model(class)
            .iter()
            .filter(|f| !f.is_static())
            .map(|f| f.name.to_string())
            .collect()
    }

    /// The real JDK 21–25 declaration order for each class whose model was
    /// rotated. Spelled out so a JDK upgrade falsifies it, and asserted as a
    /// PREFIX so a model may stop short of the real field list but must never
    /// name a field at an index the image uses for something else.
    ///
    /// Anonymous `_fN` entries are the model declining to make a claim, which
    /// is the correct thing for a slot whose meaning differs between the
    /// fabricated and real layouts.
    #[test]
    fn rotated_models_now_name_fields_at_their_real_indices() {
        let cases: &[(&str, &[&str])] = &[
            // AbstractMap contributes keySet, values ahead of k/v.
            (
                "java/util/Collections$SingletonMap",
                &["_f0", "_f1", "k", "v"],
            ),
            // The java.io Reader/Writer chain is spelled differently in the two
            // builds, so it is pinned by `reader_writer_models_match_the_build`
            // below — a row here can only state one of the two.
            //
            // Declaration order, NOT the CodeSource(URL, Certificate[]) ctor.
            (
                "java/security/CodeSource",
                &["location", "signers", "certs"],
            ),
            // Fixed earlier the same day; pinned here so the whole family is
            // covered by one test rather than three.
            (
                "java/security/ProtectionDomain",
                &["codesource", "classloader", "principals", "permissions"],
            ),
            (
                "java/lang/ThreadGroup",
                &["parent", "name", "maxPriority", "daemon"],
            ),
            // AccessibleObject declares TWO fields, and Executable adds two more
            // on top for Method/Constructor. Leaving them out shifted every
            // slot from index 1 down.
            (
                "java/lang/reflect/AccessibleObject",
                &["override", "accessCheckCache"],
            ),
            (
                "java/lang/reflect/Field",
                &[
                    "override",
                    "accessCheckCache",
                    "clazz",
                    "slot",
                    "name",
                    "type",
                    "modifiers",
                    "trustedFinal",
                ],
            ),
            (
                "java/lang/reflect/Method",
                &[
                    "override",
                    "accessCheckCache",
                    "parameterData",
                    "declaredAnnotations",
                    "clazz",
                    "slot",
                    "name",
                    "returnType",
                    "parameterTypes",
                    "exceptionTypes",
                    "modifiers",
                    // `pad_to(.., 15)` keeps the model's width at
                    // `create_method_object`'s legacy floor without naming the
                    // nine real fields between `modifiers` and `callerSensitive`.
                    "_f11",
                    "_f12",
                    "_f13",
                    "_f14",
                ],
            ),
            // Found 2026-08-05 by running the census under three REAL
            // workloads instead of three probes — the L4 record's own standing
            // caveat. `Logger` has no `level` field at all (the effective level
            // lives inside `config`), and `LogManager`'s `loggerRegistry` /
            // `ready` are VM bookkeeping, so all three are `_vmN` anchored PAST
            // the real field count where they land on padding.
            (
                "java/util/logging/Logger",
                &[
                    "config",
                    "manager",
                    "name",
                    "loggerBundle",
                    "anonymous",
                    "catalogRef",
                    "catalogName",
                    "catalogLocale",
                    "parent",
                    "kids",
                    "callerModuleRef",
                    "isSystemLogger",
                    "_vm12",
                ],
            ),
            (
                "java/util/logging/LogManager",
                &[
                    "props",
                    "systemContext",
                    "userContext",
                    "rootLogger",
                    "readPrimordialConfiguration",
                    "globalHandlersState",
                    "configurationLock",
                    "closeOnResetLoggers",
                    "listeners",
                    "initializedCalled",
                    "initializationDone",
                    "loggerRefQueue",
                    "_vm12",
                    "_vm13",
                ],
            ),
            (
                "java/lang/reflect/Constructor",
                &[
                    "override",
                    "accessCheckCache",
                    "parameterData",
                    "declaredAnnotations",
                    "clazz",
                    "slot",
                    "parameterTypes",
                    "exceptionTypes",
                    "modifiers",
                ],
            ),
        ];
        // Accumulate rather than assert per case. A `for` loop of `assert_eq!`
        // stops at the first wrong class, so reverting the whole table to the
        // pre-fix models reports ONE row and says nothing about the other eight
        // — which makes the test look far stronger than it is when it is used
        // (as it was) as the non-vacuity check for a family-wide fix.
        let mut wrong: Vec<String> = Vec::new();
        for (class, want) in cases {
            let got = instance_names(class);
            let got: Vec<&str> = got.iter().map(String::as_str).collect();
            if got != *want {
                wrong.push(format!(
                    "  {class}\n    model: {got:?}\n    real:  {want:?}"
                ));
            }
        }
        assert!(
            wrong.is_empty(),
            "{} of {} fabricated models disagree with the real JDK declaration \
             order (javap -p --module java.base <class>):\n{}",
            wrong.len(),
            cases.len(),
            wrong.join("\n")
        );
    }

    /// The `java.io` Reader/Writer chain is the one family whose model is
    /// `#[cfg]`-split, so it needs a per-build assertion rather than a row in
    /// the table above.
    ///
    /// The split is load-bearing in BOTH directions and this pins both ends:
    ///
    /// * in the default build the four models must name the real JDK fields and
    ///   carry NO `_vmN`. A `_vmN` here would be a claim that CratonVM parks a
    ///   value on a JDK-owned slot, and after `bw_synthetic_fd` went behind the
    ///   `synthetic-jdk` gate there is no writer and no reader left to do it.
    ///   Re-introducing one without moving it off slot 0 fails here.
    /// * under `synthetic-jdk` slot 0 (and, for `InputStreamReader`, slot 1)
    ///   must STAY `_vmN`. That is where `native_isr_init`, `native_br_init`,
    ///   `native_osw_init` and `native_bw_init` park an fd or a wrapped stream,
    ///   and spelling it anonymously is how `Files.newBufferedWriter`'s fd read
    ///   as an innocuous `pad` for a day.
    ///
    /// Slots 1 and 2 are the real names in both builds, which is what makes the
    /// two lists comparable at a glance.
    #[test]
    fn reader_writer_models_match_the_build() {
        #[cfg(not(feature = "synthetic-jdk"))]
        let cases: &[(&str, &[&str])] = &[
            ("java/io/InputStreamReader", &["lock", "skipBuffer", "sd"]),
            ("java/io/BufferedReader", &["lock", "skipBuffer", "in"]),
            ("java/io/OutputStreamWriter", &["writeBuffer", "lock", "se"]),
            ("java/io/BufferedWriter", &["writeBuffer", "lock", "out"]),
        ];
        #[cfg(feature = "synthetic-jdk")]
        let cases: &[(&str, &[&str])] = &[
            ("java/io/InputStreamReader", &["_vm0", "_vm1", "sd"]),
            ("java/io/BufferedReader", &["_vm0", "skipBuffer", "in"]),
            ("java/io/OutputStreamWriter", &["_vm0", "lock", "se"]),
            ("java/io/BufferedWriter", &["_vm0", "lock", "out"]),
        ];

        let mut wrong: Vec<String> = Vec::new();
        for (class, want) in cases {
            let got = instance_names(class);
            let got: Vec<&str> = got.iter().map(String::as_str).collect();
            if got != *want {
                wrong.push(format!(
                    "  {class}\n    model: {got:?}\n    want:  {want:?}"
                ));
            }
        }
        assert!(
            wrong.is_empty(),
            "{} of {} java.io Reader/Writer models are wrong for this build \
             (synthetic-jdk = {}):\n{}",
            wrong.len(),
            cases.len(),
            cfg!(feature = "synthetic-jdk"),
            wrong.join("\n")
        );

        // The non-vacuity half: in the default build the claim is specifically
        // that NOTHING is parked, so no `_vmN` may appear anywhere in these four
        // models — including at an index this test's prefix does not reach.
        #[cfg(not(feature = "synthetic-jdk"))]
        for (class, _) in cases {
            let vm: Vec<String> = instance_names(class)
                .into_iter()
                .filter(|n| is_vm_internal_model_field(n))
                .collect();
            assert!(
                vm.is_empty(),
                "{class} still parks {vm:?} on a JDK-owned slot in the default \
                 build; move the value or gate its writer on `synthetic-jdk`"
            );
        }
    }
}
