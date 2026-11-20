use super::*;

#[test]
fn demangles_legacy_names_and_drops_the_hash() {
    assert_eq!(
        demangle("_ZN3std2io5stdio6STDOUT17h0123456789abcdefE"),
        "std::io::stdio::STDOUT"
    );
    let seg = "drop_in_place$LT$std..rt..lang_start$u7b$$u7b$closure$u7d$$u7d$$GT$";
    assert_eq!(
        demangle(&format!("_ZN4core3ptr{}{seg}17h0123456789abcdefE", seg.len())),
        "core::ptr::drop_in_place<std::rt::lang_start{{closure}}>"
    );
    assert_eq!(demangle("_mi_heap_main"), "_mi_heap_main");
    assert_eq!(
        demangle("_RNvCs9rY60KznRNe_8cratonvm15VM_FOR_SHUTDOWN"),
        "cratonvm::VM_FOR_SHUTDOWN"
    );
    assert_eq!(
        demangle("_RNvNvNtNtCs1Ccr239EO5T_11cratonvm_vm7runtime9env_cache17tier_osr_backedge5CACHE"),
        "cratonvm_vm::runtime::env_cache::tier_osr_backedge::CACHE"
    );
    // A release build's shrunk copy of a static carries LLVM's `.0`; it must
    // name the same owner as the debug build's symbol.
    assert_eq!(
        demangle("_RNvNtCsiEWYkvuVel1_21cratonvm_classloading13class_manager18CLASS_HOOKS_ACTIVE.0"),
        "cratonvm_classloading::class_manager::CLASS_HOOKS_ACTIVE"
    );
    assert_eq!(
        demangle("_RNvNtCs2leKwoOEIJO_21cratonvm_classloading13class_manager18CLASS_HOOKS_ACTIVE"),
        "cratonvm_classloading::class_manager::CLASS_HOOKS_ACTIVE"
    );
    assert_eq!(
        demangle("_ZN3std2io5stdio6STDOUT17h0123456789abcdefE.llvm.42"),
        "std::io::stdio::STDOUT"
    );
}

#[test]
fn the_first_matching_rule_wins_and_unlisted_names_are_none() {
    let l = ClassList::parse(
        "exclude std::io::stdio::STDOUT\nkeep std::*\n# c\nrefuse x\nkeep *::CALLSITE\nexclude *mid*",
    )
    .unwrap();
    assert_eq!(l.classify("std::io::stdio::STDOUT"), Some(StaticClass::Exclude));
    assert_eq!(l.classify("std::thread::ID"), Some(StaticClass::Keep));
    assert_eq!(l.classify("a::b::CALLSITE"), Some(StaticClass::Keep));
    assert_eq!(l.classify("a::middle::z"), Some(StaticClass::Exclude));
    assert_eq!(l.classify("x"), Some(StaticClass::Refuse));
    assert_eq!(l.classify("y"), None);
}

#[test]
fn resolve_attributes_bytes_to_owners() {
    let tab = SymTab {
        data: vec![
            Sym { addr: 100, size: 8, name: "a".into() },
            Sym { addr: 108, size: 8, name: "b".into() },
        ],
        tls: Vec::new(),
    };
    let list = ClassList::parse("keep a").unwrap();
    let r = resolve(&[102..112, 200..201], &tab, &list, false);
    assert_eq!(r.len(), 3);
    assert_eq!((r[0].owner.as_str(), r[0].bytes, r[0].class), ("a", 6, Some(StaticClass::Keep)));
    assert_eq!((r[1].owner.as_str(), r[1].bytes, r[1].class), ("b", 4, None));
    assert_eq!(r[2].owner, "?0xc8");
}
