// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Per-thread, per-VM memo of the name-derived facts two hot `java.lang`
//! natives ask about a class id on every call.
//!
//! JIT round 17 wave 2, lane nio2 (FUNNEL17-3; page
//! `docs/feature-designs/native-dispatch-architecture.md`).
//!
//! * `Object.hashCode()` (`native_object_hash_code`) asks "is the receiver's
//!   class one of the three synthetic generic-reflection stubs?" before it
//!   answers the identity hash. It asked by NAME: `class_name_arc_of_id`, a
//!   class-manager `RwLock` read plus an `Arc` refcount bump and drop that every
//!   thread hashing an object of that class shares.
//! * `Class.isArray()` (`lang_class::native_class_is_array`) asked
//!   `mirror_class_name(..).starts_with('[')`: the same lock plus a `String`
//!   allocated and freed per call.
//!
//! Both answers are a pure function of the class NAME, and a class id's name
//! never changes: ids are never reused (`ClassStore::remove` tombstones the
//! slot), and the one in-place replacement, `upgrade_synthetic_class`, keeps the
//! id AND the name. So the first call for a class id computes the facts from
//! the name once and every later call on this thread reads them from a small
//! direct-mapped table: no lock, no allocation, no shared cache line.
//!
//! # Keys
//!
//! An entry is keyed by `(NativeContext::vm_identity, class id)`.
//! `vm_identity` is never reissued, so a thread that serves two VMs (an
//! embedding, sequential `cratonvm-embed` VMs, unit tests) cannot read one VM's
//! facts for another VM's id. A context that reports identity 0 (the trait
//! default: mocks and test contexts) is never memoised and takes the name path,
//! which is exactly the old body. Class id 0 is an ordinary key: it is the first
//! class the store assigned (ids start at 0), and it is also what the heap
//! header of an array reports, so `hashCode` on an array asks it on every call.
//!
//! # Switch
//!
//! `CRATONVM_CLASS_FACTS_MEMO` (default ON, `=0` is the kill switch). It is a
//! pure refactor of the same answer, so it ships ON; the switch exists to
//! bisect. Read once per thread (the variable is process-wide, so the answer is
//! the same on every thread; no static holds it).

use std::cell::Cell;

use cratonvm_native_api::NativeContext;
use cratonvm_types::ClassId;

/// Table size. A power of two; the slot is `class id & (WAYS - 1)`. A
/// collision evicts, which costs one name lookup, never a wrong answer.
const WAYS: usize = 64;

/// A `java.lang` boxed-primitive class, by name (JIT round 19 wave 3, lane
/// funnel3; NIO2-17-5 rung 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BoxKind {
    Integer,
    Long,
    Short,
    Byte,
    Boolean,
    Character,
    Float,
    Double,
}

impl BoxKind {
    const ALL: [BoxKind; 8] = [
        BoxKind::Integer,
        BoxKind::Long,
        BoxKind::Short,
        BoxKind::Byte,
        BoxKind::Boolean,
        BoxKind::Character,
        BoxKind::Float,
        BoxKind::Double,
    ];

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "java/lang/Integer" => BoxKind::Integer,
            "java/lang/Long" => BoxKind::Long,
            "java/lang/Short" => BoxKind::Short,
            "java/lang/Byte" => BoxKind::Byte,
            "java/lang/Boolean" => BoxKind::Boolean,
            "java/lang/Character" => BoxKind::Character,
            "java/lang/Float" => BoxKind::Float,
            "java/lang/Double" => BoxKind::Double,
            _ => return None,
        })
    }

    /// The internal name this kind was classified from.
    #[cfg(test)]
    fn name(self) -> &'static str {
        match self {
            BoxKind::Integer => "java/lang/Integer",
            BoxKind::Long => "java/lang/Long",
            BoxKind::Short => "java/lang/Short",
            BoxKind::Byte => "java/lang/Byte",
            BoxKind::Boolean => "java/lang/Boolean",
            BoxKind::Character => "java/lang/Character",
            BoxKind::Float => "java/lang/Float",
            BoxKind::Double => "java/lang/Double",
        }
    }
}

/// The facts a class NAME decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NameFacts(u32);

impl NameFacts {
    const REFLECT_TYPE_STUB: u32 = 1;
    const ARRAY: u32 = 2;
    /// JIT round 19 wave 2, lane funnel2 (NIO2-17-5): `java/lang/String`.
    const STRING: u32 = 4;
    /// JIT round 19 wave 3, lane funnel3 (NIO2-17-5 rung 2): the boxed
    /// primitive kind, `BoxKind as u32 + 1` in these four bits (0 = none).
    const BOX_SHIFT: u32 = 3;
    const BOX_MASK: u32 = 0xF << Self::BOX_SHIFT;
    /// NIO2-17-5 rung 3: a primitive type's name (`int` .. `void`), the name
    /// a primitive mirror's class carries.
    const PRIMITIVE_NAME: u32 = 1 << 7;
    /// NIO2-17-5 rung 4: the VM's annotation carrier
    /// `java/lang/annotation/AnnotationProxy`.
    const ANNOTATION_PROXY: u32 = 1 << 8;
    /// NIO2-17-5 rung 4: the shared dynamic-proxy class
    /// `java/lang/reflect/Proxy$Instance`.
    const PROXY_INSTANCE: u32 = 1 << 9;
    /// NIO2-17-5 rung 5: `org/apache/tomcat/util/buf/MessageBytes`.
    const MESSAGE_BYTES: u32 = 1 << 10;
    /// NIO2-17-5 rung 5: exactly `java/io/PrintWriter`.
    const PRINT_WRITER: u32 = 1 << 11;
    /// NIO2-17-5 rung 5: `sun/nio/cs/UTF_8`.
    const UTF8_CHARSET: u32 = 1 << 12;
    /// STR619-3 (JIT round 19 proposal wave 8, lane str8): a target name the
    /// VM's by-name `instanceof` fallback can admit
    /// (`cratonvm_types::synthetic_targets::SYNTHETIC_IMPLEMENTS_TARGETS`, the
    /// table that function's rules are proved against).
    const SYNTHETIC_TARGET: u32 = 1 << 13;

    /// Classify an internal (slash-form) class name.
    pub(crate) fn from_name(name: &str) -> Self {
        let mut bits = 0;
        if name == "java/lang/String" {
            bits |= Self::STRING;
        }
        if let Some(kind) = BoxKind::from_name(name) {
            bits |= (kind as u32 + 1) << Self::BOX_SHIFT;
        }
        if matches!(
            name,
            "int" | "long" | "float" | "double" | "boolean" | "char" | "byte" | "short" | "void"
        ) {
            bits |= Self::PRIMITIVE_NAME;
        }
        match name {
            "java/lang/annotation/AnnotationProxy" => bits |= Self::ANNOTATION_PROXY,
            "java/lang/reflect/Proxy$Instance" => bits |= Self::PROXY_INSTANCE,
            "org/apache/tomcat/util/buf/MessageBytes" => bits |= Self::MESSAGE_BYTES,
            "java/io/PrintWriter" => bits |= Self::PRINT_WRITER,
            "sun/nio/cs/UTF_8" => bits |= Self::UTF8_CHARSET,
            _ => {}
        }
        if matches!(
            name,
            "java/lang/reflect/GenericArrayType"
                | "java/lang/reflect/ParameterizedType"
                | "java/lang/reflect/WildcardType"
        ) {
            bits |= Self::REFLECT_TYPE_STUB;
        }
        if name.starts_with('[') {
            bits |= Self::ARRAY;
        }
        if cratonvm_types::synthetic_targets::synthetic_implements_may_admit(name) {
            bits |= Self::SYNTHETIC_TARGET;
        }
        NameFacts(bits)
    }

    /// A name `synthetic_implements` may admit as a TARGET (STR619-3). `false`
    /// proves `NativeContext::synthetic_implements_declared(_, name)` is false
    /// for every object.
    pub(crate) fn is_synthetic_target(self) -> bool {
        self.0 & Self::SYNTHETIC_TARGET != 0
    }

    /// One of the three synthetic generic-reflection stub classes whose
    /// `hashCode` is structural (see `native_object_hash_code`).
    pub(crate) fn is_reflect_type_stub(self) -> bool {
        self.0 & Self::REFLECT_TYPE_STUB != 0
    }

    /// An array class (`[...`).
    pub(crate) fn is_array(self) -> bool {
        self.0 & Self::ARRAY != 0
    }

    /// The class named `java/lang/String` (the name test
    /// `lang_string::is_plain_string` made per call).
    pub(crate) fn is_string(self) -> bool {
        self.0 & Self::STRING != 0
    }

    /// The boxed-primitive class this name is, if any (rung 2).
    pub(crate) fn box_kind(self) -> Option<BoxKind> {
        let code = (self.0 & Self::BOX_MASK) >> Self::BOX_SHIFT;
        (code != 0).then(|| BoxKind::ALL[code as usize - 1])
    }

    /// A primitive type's name, `int` .. `void` (rung 3).
    pub(crate) fn is_primitive_name(self) -> bool {
        self.0 & Self::PRIMITIVE_NAME != 0
    }

    /// `java/lang/annotation/AnnotationProxy` (rung 4).
    pub(crate) fn is_annotation_proxy(self) -> bool {
        self.0 & Self::ANNOTATION_PROXY != 0
    }

    /// `java/lang/reflect/Proxy$Instance` (rung 4).
    pub(crate) fn is_proxy_instance(self) -> bool {
        self.0 & Self::PROXY_INSTANCE != 0
    }

    /// `org/apache/tomcat/util/buf/MessageBytes` (rung 5).
    pub(crate) fn is_message_bytes(self) -> bool {
        self.0 & Self::MESSAGE_BYTES != 0
    }

    /// Exactly `java/io/PrintWriter` (rung 5).
    pub(crate) fn is_print_writer(self) -> bool {
        self.0 & Self::PRINT_WRITER != 0
    }

    /// `sun/nio/cs/UTF_8` (rung 5).
    pub(crate) fn is_utf8_charset(self) -> bool {
        self.0 & Self::UTF8_CHARSET != 0
    }
}

/// One NIO2-17-5 rung past the first (JIT round 19 wave 3, lane funnel3): a
/// group of natives that answer a name compare from the memo under its own
/// opt-in switch (`CRATONVM_CLASS_FACTS_MEMO_<RUNG>=1`, default OFF: flipped ON in the round-19 proposal round, back OFF at its close -- the R19Funnel3Crossing rows that showed the gain are bimodal per process, so the gain was not proven). With the
/// switch off each site keeps its by-name compare unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rung {
    /// `CRATONVM_CLASS_FACTS_MEMO_WRAPPER`: the boxed-wrapper kind of
    /// `String.valueOf(Object)` / builder `append(Object)`'s no-upcall arm and
    /// of `Boolean`'s literal text (`lang_string`).
    Wrapper = 0,
    /// `CRATONVM_CLASS_FACTS_MEMO_MIRROR`: the primitive / array mirror tests
    /// (`lang_class::mirror_is_primitive`, `mirror_is_array`,
    /// `class_mirror_is_primitive`, and `Class.getModifiers`'s array test).
    Mirror = 1,
    /// `CRATONVM_CLASS_FACTS_MEMO_PROXY`: `Class.isInstance` / `cast`'s
    /// `AnnotationProxy` and `Proxy$Instance` target tests.
    Proxy = 2,
    /// `CRATONVM_CLASS_FACTS_MEMO_CARRIERS`: `Object.toString`'s Tomcat
    /// `MessageBytes` test, `PrintWriter.println`'s autoflush carrier test and
    /// `PrintStream`'s `UTF_8` charset test (`lib.rs`).
    Carriers = 3,
    /// `CRATONVM_CLASS_FACTS_MEMO_VALUE_HASH`: `Objects.hashCode` /
    /// `Objects.hash`'s value-hash kind (`lib.rs`).
    ValueHash = 4,
    /// `CRATONVM_CLASS_FACTS_MEMO_ISINSTANCE_NAME` (JIT round 19 proposal
    /// wave 6, lane str6, FUNNEL3-19-3): `Class.isInstance` / `cast`'s false
    /// path reads the mirror's class name as the class store's `Arc<str>` (a
    /// refcount bump) instead of a fresh `String` per call
    /// (`lang_class`). Same name, same answers.
    IsInstanceName = 5,
    /// `CRATONVM_CLASS_FACTS_MEMO_ISINSTANCE_FALSE` (JIT round 19 proposal
    /// wave 8, lane str8, STR619-3): `Class.isInstance`'s false path skips the
    /// two name rules (`loader_aware_reflect_assignable`,
    /// `synthetic_implements_declared`) -- and the name and the lock they need --
    /// when the mirror's class facts prove neither can admit (`lang_class`).
    IsInstanceFalse = 6,
    /// `CRATONVM_CLASS_FACTS_MEMO_ASSIGNABLE_FALSE` (JIT round 20 wave 17,
    /// lane pRefl; `R20PNatAssignable`): `Class.isAssignableFrom`'s resolved
    /// pair (`lang_class::class_is_assignable_resolved_pair`) reads both
    /// classes' array / primitive-name facts and the target's
    /// synthetic-target fact from this memo, and skips the two name rules --
    /// and the two class-name reads under the class-manager lock -- when the
    /// facts prove neither can admit, as `IsInstanceFalse` does for
    /// `isInstance`. Default ON, `=0` restores the name path.
    AssignableFalse = 7,
}

impl Rung {
    const ALL: [Rung; 8] = [
        Rung::Wrapper,
        Rung::Mirror,
        Rung::Proxy,
        Rung::Carriers,
        Rung::ValueHash,
        Rung::IsInstanceName,
        Rung::IsInstanceFalse,
        Rung::AssignableFalse,
    ];

    fn switch(self) -> &'static str {
        match self {
            Rung::Wrapper => "CRATONVM_CLASS_FACTS_MEMO_WRAPPER",
            Rung::Mirror => "CRATONVM_CLASS_FACTS_MEMO_MIRROR",
            Rung::Proxy => "CRATONVM_CLASS_FACTS_MEMO_PROXY",
            Rung::Carriers => "CRATONVM_CLASS_FACTS_MEMO_CARRIERS",
            Rung::ValueHash => "CRATONVM_CLASS_FACTS_MEMO_VALUE_HASH",
            Rung::IsInstanceName => "CRATONVM_CLASS_FACTS_MEMO_ISINSTANCE_NAME",
            Rung::IsInstanceFalse => "CRATONVM_CLASS_FACTS_MEMO_ISINSTANCE_FALSE",
            Rung::AssignableFalse => "CRATONVM_CLASS_FACTS_MEMO_ASSIGNABLE_FALSE",
        }
    }

    /// Rungs default ON since the JIT round 19 proposal round's close
    /// (2026-10-09; `R19Str8Rows` isinstance-false ~450 -> ~320 ns per run in
    /// both modes of the row, facts and batteries equal); `=0` kills each.
    /// `AssignableFalse` is default ON from its landing (JIT round 20 wave 17,
    /// the 2026-10-13 switch policy: new perf rows ship ON, the orchestrator
    /// A/Bs them).
    fn default_on(self) -> bool {
        matches!(
            self,
            Rung::Proxy | Rung::IsInstanceFalse | Rung::AssignableFalse
        )
    }
}

/// Bit 31 of `RUNG_MODE`: the switches were read on this thread.
const RUNGS_READ: u32 = 1 << 31;

/// Is `rung`'s switch on? Read once per thread for all rungs, like
/// [`string_fact_enabled`] (the variables are process-wide; no static holds
/// the answer).
pub(crate) fn rung_enabled(rung: Rung) -> bool {
    RUNG_MODE.with(|mode| {
        let mut bits = mode.get();
        if bits & RUNGS_READ == 0 {
            bits = RUNGS_READ;
            for r in Rung::ALL {
                let on = if r.default_on() {
                    cratonvm_types::flags::runtime_flag_default_on(r.switch())
                } else {
                    cratonvm_types::flags::runtime_flag_on(r.switch())
                };
                if on {
                    bits |= 1 << r as u32;
                }
            }
            mode.set(bits);
        }
        bits & (1 << rung as u32) != 0
    })
}

/// The name facts of `class_id` when `rung` is on, else `None` (the caller's
/// by-name compare runs). The outer `Some` carries [`name_facts`]' answer,
/// whose own `None` is "no class at that id" -- the by-name compares' "no
/// name" arm.
pub(crate) fn rung_facts(
    ctx: &dyn NativeContext,
    rung: Rung,
    class_id: ClassId,
) -> Option<Option<NameFacts>> {
    rung_enabled(rung).then(|| name_facts(ctx, class_id))
}

#[derive(Clone, Copy)]
struct Entry {
    vm: usize,
    class_id: u32,
    facts: NameFacts,
}

impl Entry {
    /// `vm == 0` is never stored (see `lookup_or_fill`), so this entry never
    /// matches.
    const EMPTY: Entry = Entry {
        vm: 0,
        class_id: 0,
        facts: NameFacts(0),
    };
}

thread_local! {
    static MEMO: [Cell<Entry>; WAYS] = const { [const { Cell::new(Entry::EMPTY) }; WAYS] };
    /// 0 = not read yet on this thread, 1 = on, 2 = off.
    static MODE: Cell<u8> = const { Cell::new(0) };
    /// `CRATONVM_CLASS_FACTS_MEMO_STRING`, same encoding as `MODE`.
    static STRING_MODE: Cell<u8> = const { Cell::new(0) };
    /// The NIO2-17-5 rung switches ([`Rung`]): bit `rung` = on, bit 31 =
    /// read (JIT round 19 wave 3, lane funnel3).
    static RUNG_MODE: Cell<u32> = const { Cell::new(0) };
}

/// Does `lang_string::is_plain_string` answer from this memo
/// (`CRATONVM_CLASS_FACTS_MEMO_STRING=1`, default OFF)? JIT round 19 wave 2,
/// lane funnel2 (NIO2-17-5): `String.valueOf(Object)` and the builders'
/// `append(Object)` / `append(CharSequence)` / `insert` asked the class
/// manager for the receiver's name, as an allocated `String`, on every call.
/// Read once per thread, like [`enabled`].
pub(crate) fn string_fact_enabled() -> bool {
    STRING_MODE.with(|mode| match mode.get() {
        1 => true,
        2 => false,
        _ => {
            let on = cratonvm_types::flags::runtime_flag_on("CRATONVM_CLASS_FACTS_MEMO_STRING");
            mode.set(if on { 1 } else { 2 });
            on
        }
    })
}

/// Is the memo on (`CRATONVM_CLASS_FACTS_MEMO`, default ON)?
fn enabled() -> bool {
    MODE.with(|mode| match mode.get() {
        1 => true,
        2 => false,
        _ => {
            let on = cratonvm_types::flags::runtime_flag_default_on("CRATONVM_CLASS_FACTS_MEMO");
            mode.set(if on { 1 } else { 2 });
            on
        }
    })
}

/// The memo itself, separated from the context so it can be tested: answer
/// `(vm, class_id)` from the table, or compute it with `fill` and remember a
/// `Some`. A `None` from `fill` (no such class) is not remembered.
fn lookup_or_fill(
    vm: usize,
    class_id: u32,
    fill: impl FnOnce() -> Option<NameFacts>,
) -> Option<NameFacts> {
    if vm == 0 {
        return fill();
    }
    let slot = class_id as usize & (WAYS - 1);
    let hit = MEMO.with(|memo| {
        let e = memo[slot].get();
        (e.vm == vm && e.class_id == class_id).then_some(e.facts)
    });
    if hit.is_some() {
        return hit;
    }
    let facts = fill()?;
    MEMO.with(|memo| {
        memo[slot].set(Entry {
            vm,
            class_id,
            facts,
        })
    });
    Some(facts)
}

/// The name-derived facts of `class_id` in the asking VM, or `None` when the
/// class manager has no class at that id (the old bodies' "no name" arm).
///
/// With the memo off this is exactly `class_name_arc_of_id` + classify, the
/// old per-call cost.
pub(crate) fn name_facts(ctx: &dyn NativeContext, class_id: ClassId) -> Option<NameFacts> {
    let by_name = || {
        ctx.class_name_arc_of_id(class_id)
            .map(|name| NameFacts::from_name(&name))
    };
    if !enabled() {
        return by_name();
    }
    lookup_or_fill(ctx.vm_identity(), class_id.as_u32(), by_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_classify_as_the_old_bodies_tested_them() {
        for n in [
            "java/lang/reflect/GenericArrayType",
            "java/lang/reflect/ParameterizedType",
            "java/lang/reflect/WildcardType",
        ] {
            let f = NameFacts::from_name(n);
            assert!(f.is_reflect_type_stub(), "{n}");
            assert!(!f.is_array(), "{n}");
        }
        for n in ["[I", "[Ljava/lang/String;", "[[J"] {
            let f = NameFacts::from_name(n);
            assert!(f.is_array(), "{n}");
            assert!(!f.is_reflect_type_stub(), "{n}");
            assert!(!f.is_string(), "{n}");
        }
        let s = NameFacts::from_name("java/lang/String");
        assert!(s.is_string() && !s.is_array() && !s.is_reflect_type_stub());
        assert_eq!(s.box_kind(), None);
        assert!(!NameFacts::from_name("java/lang/StringBuilder").is_string());
        for n in [
            "java/lang/reflect/TypeVariable",
            "sun/reflect/generics/reflectiveObjects/ParameterizedTypeImpl",
            "",
        ] {
            assert_eq!(NameFacts::from_name(n), NameFacts(0), "{n}");
        }
        // STR619-3: `Object` is a synthetic target and nothing else.
        assert_eq!(
            NameFacts::from_name("java/lang/Object"),
            NameFacts(NameFacts::SYNTHETIC_TARGET)
        );
    }

    /// STR619-3: the synthetic-target fact is exactly the shared table.
    #[test]
    fn the_synthetic_target_fact_is_the_shared_table() {
        for t in cratonvm_types::synthetic_targets::SYNTHETIC_IMPLEMENTS_TARGETS {
            assert!(NameFacts::from_name(t).is_synthetic_target(), "{t}");
        }
        for n in [
            "R19Str6Rows$Foo",
            "java/util/HashMap",
            "java/util/Map",
            "java/lang/Runnable",
            "[Ljava/lang/Object;",
            "",
        ] {
            assert!(!NameFacts::from_name(n).is_synthetic_target(), "{n}");
        }
    }

    /// NIO2-17-5 rungs 2-5 (JIT round 19 wave 3, lane funnel3): each fact is
    /// exactly the name compare it replaces, and no two facts overlap.
    #[test]
    fn the_rung_facts_are_the_name_compares_they_replace() {
        for kind in BoxKind::ALL {
            let f = NameFacts::from_name(kind.name());
            assert_eq!(f.box_kind(), Some(kind), "{}", kind.name());
            assert!(!f.is_string() && !f.is_primitive_name() && !f.is_array());
        }
        for n in [
            "java/lang/Number",
            "java/lang/Integer$IntegerCache",
            "Integer",
            "java.lang.Integer",
            "[Ljava/lang/Integer;",
        ] {
            assert_eq!(NameFacts::from_name(n).box_kind(), None, "{n}");
        }
        for n in [
            "int", "long", "float", "double", "boolean", "char", "byte", "short", "void",
        ] {
            let f = NameFacts::from_name(n);
            assert!(
                f.is_primitive_name() && !f.is_array() && f.box_kind().is_none(),
                "{n}"
            );
        }
        for n in ["I", "Int", "java/lang/Void", "[I", "integer"] {
            assert!(!NameFacts::from_name(n).is_primitive_name(), "{n}");
        }
        let carriers = [
            "java/lang/annotation/AnnotationProxy",
            "java/lang/reflect/Proxy$Instance",
            "org/apache/tomcat/util/buf/MessageBytes",
            "java/io/PrintWriter",
            "sun/nio/cs/UTF_8",
        ];
        let tests: [fn(NameFacts) -> bool; 5] = [
            NameFacts::is_annotation_proxy,
            NameFacts::is_proxy_instance,
            NameFacts::is_message_bytes,
            NameFacts::is_print_writer,
            NameFacts::is_utf8_charset,
        ];
        for (i, test) in tests.iter().enumerate() {
            for (j, name) in carriers.iter().enumerate() {
                assert_eq!(test(NameFacts::from_name(name)), i == j, "{i} on {name}");
            }
        }
        // Exact names only: a subclass or a look-alike is not the carrier.
        for n in [
            "java/io/PrintWriterSub",
            "com/example/AnnotationProxy",
            "java/lang/reflect/Proxy",
            "sun/nio/cs/UTF_16",
        ] {
            let f = NameFacts::from_name(n);
            assert!(tests.iter().all(|t| !t(f)), "{n}");
        }
    }

    /// Every rung has its own opt-in switch name, indexed by its bit.
    #[test]
    fn every_rung_switch_is_its_own_opt_in() {
        let names: Vec<_> = Rung::ALL.iter().map(|r| r.switch()).collect();
        let mut dedup = names.clone();
        dedup.sort_unstable();
        dedup.dedup();
        assert_eq!(dedup.len(), names.len());
        for (i, r) in Rung::ALL.iter().enumerate() {
            assert_eq!(*r as usize, i);
            // Spelled in two pieces: a whole `"CRATONVM_…"` literal reads as a
            // flag name to `flag_declaration_guard`.
            assert!(names[i].starts_with(&["CRATONVM", "_CLASS_FACTS_MEMO_"].concat()));
        }
    }

    /// A hit never calls `fill`; a different VM with the same class id misses;
    /// identity 0 is never stored (class id 0 is); a `None` is not remembered.
    #[test]
    fn the_memo_is_keyed_by_vm_and_class_id() {
        let array = NameFacts::from_name("[I");
        let plain = NameFacts::from_name("java/lang/Object");
        // Ids chosen to share a slot with nothing the other tests use.
        let (vm_a, vm_b, cid) = (0x5EED_0001usize, 0x5EED_0002usize, 0x7FFF_FF41u32);
        assert_eq!(lookup_or_fill(vm_a, cid, || Some(array)), Some(array));
        assert_eq!(
            lookup_or_fill(vm_a, cid, || panic!("a hit must not recompute")),
            Some(array)
        );
        // Same class id, another VM: its own answer, which then owns the slot.
        assert_eq!(lookup_or_fill(vm_b, cid, || Some(plain)), Some(plain));
        assert_eq!(lookup_or_fill(vm_a, cid, || Some(array)), Some(array));
        // Never memoised: identity 0 (and the empty slots, whose `vm` is 0, never
        // answer it). Class id 0 is an ordinary key.
        let mut calls = 0;
        for _ in 0..2 {
            let _ = lookup_or_fill(0, cid, || {
                calls += 1;
                Some(plain)
            });
            let _ = lookup_or_fill(vm_a, 0, || {
                calls += 1;
                Some(plain)
            });
        }
        assert_eq!(calls, 3);
        let missing = 0x7FFF_FF82u32;
        assert_eq!(lookup_or_fill(vm_a, missing, || None), None);
        assert_eq!(lookup_or_fill(vm_a, missing, || Some(plain)), Some(plain));
    }
}
