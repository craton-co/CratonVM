use super::*;

/// tc21b TYPECHECK21-2: the hint kind. A synthetic target is kind 1
/// (misses declined) only with the switch off; arrays, primitive names and
/// a fact miss never get a hint.
#[test]
fn tc21b_mirror_hint_kind_drops_the_synthetic_target_screen_only_when_asked() {
    for t in cratonvm_types::synthetic_targets::SYNTHETIC_IMPLEMENTS_TARGETS {
        let f = Some(NameFacts::from_name(t));
        assert_eq!(mirror_hint_kind(f, true), 2, "{t}: miss answered");
        assert_eq!(mirror_hint_kind(f, false), 1, "{t}: miss declined");
    }
    for n in ["java/util/HashMap", "java/lang/Runnable", "R19Str6Rows$Foo"] {
        let f = Some(NameFacts::from_name(n));
        assert_eq!(mirror_hint_kind(f, true), 2, "{n}");
        assert_eq!(mirror_hint_kind(f, false), 2, "{n}");
    }
    for n in ["[I", "[Ljava/util/List;", "int", "void", "boolean"] {
        let f = Some(NameFacts::from_name(n));
        assert_eq!(mirror_hint_kind(f, true), 0, "{n}");
        assert_eq!(mirror_hint_kind(f, false), 0, "{n}");
    }
    assert_eq!(mirror_hint_kind(None, true), 0, "no class at the id");
    assert_eq!(mirror_hint_kind(None, false), 0, "no class at the id");
}

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

#[test]
fn string_fact_memo_is_enabled_by_default() {
    assert!(string_fact_enabled());
}
