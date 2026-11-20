use super::*;
#[test]
fn six_plain_byte_array_view_carriers_bind_with_two_coordinates() {
    for (slot, ret) in VARHANDLE_BYTE_ARRAY_VIEW_RETURNS.iter().enumerate() {
        assert_eq!(
            varhandle_byte_array_view_read_slot("get", &format!("([BI){}", *ret as char)),
            Some(slot)
        );
    }
    if varhandle_judged_binds_enabled() {
        for (slot, ret) in VARHANDLE_BYTE_ARRAY_VIEW_RETURNS.iter().enumerate() {
            assert_eq!(
                varhandle_byte_array_view_read_slot(
                    "get",
                    &format!("(Ljava/nio/ByteBuffer;I){}", *ret as char)
                ),
                Some(slot)
            );
        }
    }
    for desc in [
        "([BI)B",
        "([BI)Z",
        "([II)I",
        "([BJ)J",
        "([BI)Ljava/lang/Object;",
        "(Ljava/nio/ByteBuffer;J)I",
        "(Ljava/nio/ByteBuffer;I)B",
        "([BI)Igarbage",
    ] {
        assert_eq!(
            varhandle_byte_array_view_read_slot("get", desc),
            None,
            "{desc}"
        );
    }
    for mode in ["getVolatile", "getOpaque", "getAcquire", "set"] {
        assert_eq!(varhandle_byte_array_view_read_slot(mode, "([BI)I"), None);
    }
}
#[test]
fn plain_byte_array_view_writes_share_the_judged_two_coordinate_abi() {
    if !varhandle_judged_binds_enabled() {
        return;
    }
    for prefix in ["([BI", "(Ljava/nio/ByteBuffer;I"] {
        for (slot, kind) in VARHANDLE_BYTE_ARRAY_VIEW_RETURNS.iter().enumerate() {
            let descriptor = format!("{prefix}{})V", *kind as char);
            assert_eq!(
                varhandle_byte_buffer_view_write_slot("set", &descriptor),
                Some(slot),
                "{descriptor}"
            );
        }
    }
    for descriptor in [
        "([BIB)V",
        "([BIZ)V",
        "([IIJ)V",
        "([BJJ)V",
        "([BIJ)J",
        "([BIJ)Vjunk",
        "([BI)J",
        "([BIIJ)V",
        "(Ljava/lang/Object;IJ)V",
    ] {
        assert_eq!(
            varhandle_byte_buffer_view_write_slot("set", descriptor),
            None,
            "{descriptor}"
        );
    }
    for mode in [
        "setVolatile",
        "setOpaque",
        "setRelease",
        "compareAndSet",
        "getAndSet",
    ] {
        assert_eq!(varhandle_byte_buffer_view_write_slot(mode, "([BIJ)V"), None);
    }
}
#[test]
fn view_writes_keep_native_admission_and_vm_context_guards() {
    if !varhandle_ir_access_shape("set", "([BIJ)V") {
        return;
    }
    let table = DirectHelperTable {
        static_base_resolver_ctx: 0x1000,
        varhandle_byte_buffer_view_write: [0x4000; 6],
        ..DirectHelperTable::EMPTY
    };
    let no_vm = DirectHelperTable {
        static_base_resolver_ctx: 0,
        ..table
    };
    let admits = |class: &str, name: &str, descriptor: &str| {
        class == "java/lang/invoke/VarHandle"
            && name == "set"
            && descriptor == "([Ljava/lang/Object;)V"
    };
    let refuses = |_: &str, _: &str, _: &str| false;
    for descriptor in ["([BIJ)V", "(Ljava/nio/ByteBuffer;IJ)V"] {
        assert_eq!(
            varhandle_ir_access_entry("set", descriptor, &table, true, Some(&admits)),
            Some(0x4000)
        );
        assert_eq!(
            varhandle_ir_access_entry("set", descriptor, &table, true, Some(&refuses)),
            None
        );
        assert_eq!(
            varhandle_ir_access_entry("set", descriptor, &no_vm, true, Some(&admits)),
            None
        );
    }
}
