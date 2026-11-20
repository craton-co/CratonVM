use super::reflect_new_array_helper;

/// The one triple, the table word, and `=0` kills it.
#[test]
fn reflect_new_array_binds_the_one_triple() {
    let table = crate::DirectHelperTable {
        reflect_new_array: 0x1000,
        ..crate::DirectHelperTable::EMPTY
    };
    let key = concat!("CRATON", "VM_JIT_REFLECT_NEW_ARRAY_DIRECT");
    let arr = "java/lang/reflect/Array";
    let desc = "(Ljava/lang/Class;I)Ljava/lang/Object;";
    let multi = "(Ljava/lang/Class;[I)Ljava/lang/Object;";
    let compat = |t: &crate::DirectHelperTable, c: &str, m: &str, d: &str| {
        reflect_new_array_helper(t, false, None, c, m, d)
    };
    cratonvm_types::flags::with_thread_overrides(&[(key, None)], || {
        assert_eq!(compat(&table, arr, "newArray", desc), 0x1000);
        assert_eq!(compat(&table, arr, "newInstance", desc), 0);
        assert_eq!(compat(&table, arr, "newArray", multi), 0);
        assert_eq!(compat(&table, "java/lang/System", "newArray", desc), 0);
        assert_eq!(compat(&crate::DirectHelperTable::EMPTY, arr, "newArray", desc), 0);
        // `--real-jdk` binds only what the registry admits.
        let refuse = |_: &str, _: &str, _: &str| false;
        let admit = |_: &str, _: &str, _: &str| true;
        let jdk = |adm: &dyn Fn(&str, &str, &str) -> bool| {
            reflect_new_array_helper(&table, true, Some(adm), arr, "newArray", desc)
        };
        assert_eq!(jdk(&refuse), 0);
        assert_eq!(jdk(&admit), 0x1000);
    });
}
