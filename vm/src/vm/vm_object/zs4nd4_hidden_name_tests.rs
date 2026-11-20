use super::{
    create_java_string_uninterned, get_or_create_class_mirror, heap_is_zgc,
    hidden_mirror_name_unpooled, loader_class_name_unpooled, mirror_name_unpooled, SharedVm,
};
use crate::config::VmConfig;

/// The minimal class file `this_name extends java/lang/Object`.
fn zs4nd4_class_bytes(this_name: &str) -> Vec<u8> {
    let super_name = "java/lang/Object";
    let mut b: Vec<u8> = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x34];
    b.extend_from_slice(&[0x00, 0x05]);
    b.push(7);
    b.extend_from_slice(&[0x00, 0x02]);
    b.push(1);
    b.extend_from_slice(&u16::try_from(this_name.len()).unwrap().to_be_bytes());
    b.extend_from_slice(this_name.as_bytes());
    b.push(7);
    b.extend_from_slice(&[0x00, 0x04]);
    b.push(1);
    b.extend_from_slice(&u16::try_from(super_name.len()).unwrap().to_be_bytes());
    b.extend_from_slice(super_name.as_bytes());
    b.extend_from_slice(&[0x00, 0x21, 0x00, 0x01, 0x00, 0x03]);
    b.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    b
}

/// zs s4/nd4: on ZGC a hidden class's mirror name is NOT pooled (the
/// pool is a strong root there, so a pooled name outlived its unloaded
/// class: `Zs2Nd2FfmMints define`), while a plain class's still is (the
/// positive control: it proves the mirror writes a pooled name at all in
/// this VM, so the hidden arm's absence is not vacuous). Kill arm
/// (`CRATONVM_ZGC_HIDDEN_NAME_UNPOOLED=0`, read once per process): the
/// test then asserts the name IS pooled.
#[test]
fn zs4nd4_a_hidden_class_mirror_name_stays_out_of_the_pool_on_zgc() {
    use crate::classloading::ClassLoaderId;
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: crate::config::GcAlgorithm::Zgc,
        max_heap_size: 64 << 20,
        ..VmConfig::default()
    });
    if !heap_is_zgc(&shared) {
        eprintln!("skipped: no ZGC heap in this build");
        return;
    }
    let define = |name: &str, hidden: bool| {
        shared
            .classes
            .class_manager_write()
            .define_class_with_options(
                name,
                &zs4nd4_class_bytes(if hidden { "zs4nd4/Hid" } else { name }),
                ClassLoaderId::Application,
                cratonvm_classloading::DefineClassOptions {
                    override_name: Some(name.to_string()),
                    hidden,
                    skip_verification: true,
                    ..Default::default()
                },
            )
            .ok()
    };
    let (Some(plain), Some(hidden)) = (
        define("zs4nd4/Plain", false),
        define("zs4nd4/Hid/0x1", true),
    ) else {
        eprintln!("skipped: this unit-test VM cannot define an application class");
        return;
    };
    let pooled = |text: &str| shared.mem.string_pool.read().contains_key(text);
    let plain_mirror = get_or_create_class_mirror(&shared, plain);
    if !pooled("zs4nd4/Plain") {
        eprintln!("skipped: this VM's java/lang/Class has no `name` slot (nothing to pool)");
        return;
    }
    let hidden_mirror = get_or_create_class_mirror(&shared, hidden);
    assert_ne!(plain_mirror, hidden_mirror);
    assert!(
        shared
            .classes
            .class_manager
            .read()
            .get_class(hidden)
            .is_some_and(|c| c.is_hidden()),
        "the fixture defined a hidden class"
    );
    let enabled = hidden_mirror_name_unpooled(&shared);
    eprintln!(
        "zs4nd4 hidden name: unpooled={enabled} pooled_after_mint={}",
        pooled("zs4nd4/Hid/0x1")
    );
    assert_eq!(
        pooled("zs4nd4/Hid/0x1"),
        !enabled,
        "a hidden class's mirror name is pooled exactly when the switch is off"
    );
}

/// zt t7/cl: on ZGC the mirror name of a class with a defining-loader row
/// (a loader object defined it: the classes that unload) is NOT pooled,
/// while a class without a row still pools it (the positive control: the
/// mirror writes a pooled name at all in this VM). zt t8/cl4: the switch is
/// opt-in; zt t9/cl5: default ON again. The test asserts `pooled ==
/// !enabled` with the switch as read once per process (by default the
/// row's name is NOT pooled; with `=0` it is).
#[test]
fn zt7cl_a_loader_defined_class_mirror_name_stays_out_of_the_pool_on_zgc() {
    use crate::classloading::ClassLoaderId;
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: crate::config::GcAlgorithm::Zgc,
        max_heap_size: 64 << 20,
        ..VmConfig::default()
    });
    if !heap_is_zgc(&shared) {
        eprintln!("skipped: no ZGC heap in this build");
        return;
    }
    let define = |name: &str| {
        shared
            .classes
            .class_manager_write()
            .define_class_with_options(
                name,
                &zs4nd4_class_bytes(name),
                ClassLoaderId::Application,
                cratonvm_classloading::DefineClassOptions {
                    override_name: Some(name.to_string()),
                    skip_verification: true,
                    ..Default::default()
                },
            )
            .ok()
    };
    let (Some(plain), Some(loaded)) = (define("zt7cl/Plain"), define("zt7cl/Loaded")) else {
        eprintln!("skipped: this unit-test VM cannot define an application class");
        return;
    };
    // Any heap object stands for the loader: only the row's presence is
    // asked.
    let loader = create_java_string_uninterned(&shared, "zt7cl loader stand-in");
    cratonvm_native_builtins::classloader::register_defining_loader(
        &shared.loader_identity,
        loaded.as_u32(),
        loader,
    );
    let pooled = |text: &str| shared.mem.string_pool.read().contains_key(text);
    let _ = get_or_create_class_mirror(&shared, plain);
    if !pooled("zt7cl/Plain") {
        eprintln!("skipped: this VM's java/lang/Class has no `name` slot (nothing to pool)");
        return;
    }
    assert!(
        !mirror_name_unpooled(&shared, plain, false),
        "no row: the name is pooled as before"
    );
    let _ = get_or_create_class_mirror(&shared, loaded);
    let enabled = loader_class_name_unpooled(&shared);
    eprintln!(
        "zt7cl loader-defined name: unpooled={enabled} pooled_after_mint={}",
        pooled("zt7cl/Loaded")
    );
    assert_eq!(mirror_name_unpooled(&shared, loaded, false), enabled);
    assert_eq!(
        pooled("zt7cl/Loaded"),
        !enabled,
        "a loader-defined class's mirror name is pooled exactly when the switch is off"
    );
}
