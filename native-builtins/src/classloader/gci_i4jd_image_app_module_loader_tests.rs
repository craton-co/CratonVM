use super::*;

fn fake(addr: usize) -> ObjectRef {
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn seed(vm: usize, adopted: bool) {
    *detached_loader_tables(vm).module_sets.lock() = Some(Arc::new(BuiltinModuleSets {
        boot: ["java.base", "java.desktop"].iter().map(|m| m.to_string()).collect(),
        platform: ["java.sql", "java.compiler"].iter().map(|m| m.to_string()).collect(),
    }));
    with_loader_singletons(detached_loader_tables(vm), |row| {
        row.app = Some(fake(0x4A_D000));
        row.platform = Some(fake(0x4A_E000));
        row.adopted = adopted;
    });
}

fn addr(r: Option<ObjectRef>) -> Option<usize> {
    r.map(|r| r.as_ptr() as usize)
}

/// An image module in neither of the JDK's boot and platform sets
/// (`jdk.compiler`) names the adopted application loader; boot and
/// platform modules do not (the platform ones are the tag arm's).
#[test]
fn an_application_image_module_names_the_app_loader() {
    const VM: usize = 0x4A_1000;
    seed(VM, true);
    let t = detached_loader_tables(VM);
    let f = |m: &str, c: &str, on: bool| adopted_app_loader_for_image_module(t, m, c, on);
    assert_eq!(
        addr(f("jdk.compiler", "com/sun/tools/javac/main/JavaCompiler", true)),
        Some(0x4A_D000)
    );
    assert_eq!(addr(f("java.base", "java/lang/String", true)), None);
    assert_eq!(addr(f("java.sql", "java/sql/Connection", true)), None);
    assert_eq!(
        addr(f("jdk.compiler", "com/sun/tools/javac/main/JavaCompiler", false)),
        None
    );
    assert_eq!(addr(f("app.mod", "org/example/Main", true)), None);
    assert_eq!(addr(f("jdk.proxy1", "jdk/proxy1/$Proxy3", true)), None);
    forget_vm_loader_singletons(VM);
}

/// Nothing adopted (`--compatible`, or `CRATONVM_REAL_BUILTIN_LOADERS=0`)
/// or cold module tables: no answer, the mirror keeps today's null.
#[test]
fn nothing_adopted_or_cold_tables_answer_none() {
    const VM: usize = 0x4A_2000;
    seed(VM, false);
    assert!(adopted_app_loader_for_image_module(
        detached_loader_tables(VM),
        "jdk.compiler",
        "com/sun/tools/javac/main/JavaCompiler",
        true
    )
    .is_none());
    forget_vm_loader_singletons(VM);
    const COLD: usize = 0x4A_3000;
    with_loader_singletons(detached_loader_tables(COLD), |row| {
        row.app = Some(fake(0x4A_D000));
        row.adopted = true;
    });
    assert!(adopted_app_loader_for_image_module(
        detached_loader_tables(COLD),
        "jdk.compiler",
        "com/sun/tools/javac/main/JavaCompiler",
        true
    )
    .is_none());
    forget_vm_loader_singletons(COLD);
}
