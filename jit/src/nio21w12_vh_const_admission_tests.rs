use super::*;

#[test]
fn a_site_is_admitted_by_the_nearest_planned_getstatic_of_its_kind() {
    let planned = [
        (0usize, b'L', 7u32, 0usize),
        (10, b'I', 7, 1),
        (30, b'L', 8, 0),
    ];
    let cas = "(LFoo;Ljava/lang/Object;Ljava/lang/Object;)Z";
    if !ir_vh_const_admit_enabled() {
        return;
    }
    let all = |_: u32, _: usize| true;
    let not_8 = |c: u32, _: usize| c != 8;
    assert!(ir_vh_const_site_admitted(
        5,
        "compareAndSet",
        cas,
        &planned,
        &all
    ));
    assert!(
        !ir_vh_const_site_admitted(20, "compareAndSet", cas, &planned, &all),
        "nearest is I"
    );
    assert!(ir_vh_const_site_admitted(
        20,
        "getAndAdd",
        "(LFoo;I)I",
        &planned,
        &all
    ));
    assert!(ir_vh_const_site_admitted(
        40,
        "setRelease",
        "(LFoo;LBar;)V",
        &planned,
        &all
    ));
    assert!(
        !ir_vh_const_site_admitted(40, "setRelease", "(LFoo;LBar;)V", &planned, &not_8),
        "the prefix would not be emitted for that handle"
    );
    assert!(
        !ir_vh_const_site_admitted(0, "compareAndSet", cas, &planned, &all),
        "nothing before"
    );
    assert!(
        !ir_vh_const_site_admitted(40, "get", "(LFoo;)F", &planned, &all),
        "no shape"
    );
    assert!(!ir_vh_const_site_admitted(
        40,
        "compareAndSet",
        cas,
        &[],
        &all
    ));
}

#[test]
fn the_bind_takes_the_ladder_s_slots_and_admission() {
    if !ir_vh_const_admit_enabled()
        || !ir_direct_calls_enabled()
        || !direct_jit_callee_calls_enabled()
    {
        return;
    }
    let table = DirectHelperTable {
        static_base_resolver_ctx: 0x1000,
        varhandle_read: [0x2000; VARHANDLE_READ_SLOTS],
        varhandle_write: [0x3000; VARHANDLE_WRITE_SLOTS],
        varhandle_cas: [0x4000; VARHANDLE_CAS_SLOTS],
        ..DirectHelperTable::EMPTY
    };
    let admits = |_: &str, _: &str, _: &str| true;
    let refuses = |_: &str, _: &str, _: &str| false;
    if varhandle_cas_direct_helpers_enabled() {
        let cas = "(LFoo;Ljava/lang/Object;Ljava/lang/Object;)Z";
        assert_eq!(
            varhandle_ir_const_bind_entry("compareAndSet", cas, &table, false, None),
            Some(0x4000)
        );
        assert_eq!(
            varhandle_ir_const_bind_entry("compareAndSet", cas, &table, true, Some(&admits)),
            Some(0x4000)
        );
        assert_eq!(
            varhandle_ir_const_bind_entry("compareAndSet", cas, &table, true, Some(&refuses)),
            None
        );
        assert_eq!(
            varhandle_ir_const_bind_entry("compareAndSet", cas, &table, true, None),
            None
        );
    }
    if varhandle_write_direct_helpers_enabled() {
        assert_eq!(
            varhandle_ir_const_bind_entry("set", "(LFoo;J)V", &table, false, None),
            Some(0x3000)
        );
    }
    // A reference read keeps its call; a mode with no thin slot has no bind.
    assert_eq!(
        varhandle_ir_const_bind_entry("get", "(LFoo;)Ljava/lang/Object;", &table, false, None),
        None
    );
    assert_eq!(
        varhandle_ir_const_bind_entry("getAndAdd", "(LFoo;I)I", &table, false, None),
        None
    );
    let no_vm = DirectHelperTable {
        static_base_resolver_ctx: 0,
        ..table
    };
    assert_eq!(
        varhandle_ir_const_bind_entry("set", "(LFoo;J)V", &no_vm, false, None),
        None
    );
}
