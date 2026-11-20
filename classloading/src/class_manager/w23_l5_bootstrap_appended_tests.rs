use super::*;
use std::io::Write;

/// Interpreter round i1 wave 23, lane L5: a jar appended to one VM's
/// bootstrap search makes its classes bootstrap classes in THAT VM only.
/// Round 14 wave 2 (MISC11-2): the census is the appending VM's own
/// `ClassManager` field, so another VM's manager never sees it.
#[test]
fn an_appended_bootstrap_jar_belongs_to_the_vm_that_appended_it() {
    let dir = std::env::temp_dir().join("cratonvm_w23_l5_bootstrap_appended");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let jar_path = dir.join("appended.jar");
    {
        let file = std::fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("w23l5/Appended.class", options).unwrap();
        zip.write_all(b"not parsed").unwrap();
        zip.finish().unwrap();
    }
    let mut appending_vm = ClassManager::new(&[], &[], &[]);
    let other_vm = ClassManager::new(&[], &[], &[]);
    // RV3-4: the lock-free flag is a shared handle, false until an append
    // lands classes, and set only on the appending VM.
    let seen = appending_vm.bootstrap_append_seen();
    assert!(!seen.load(Ordering::Acquire));
    appending_vm.extend_bootstrap_classpath(&[]);
    assert!(
        !seen.load(Ordering::Acquire),
        "an empty append adds no class"
    );
    appending_vm.extend_bootstrap_classpath(&[jar_path.to_str().unwrap().to_string()]);
    assert!(seen.load(Ordering::Acquire));
    assert!(!other_vm.bootstrap_append_seen().load(Ordering::Acquire));
    assert!(appending_vm.is_bootstrap_appended_class("w23l5/Appended"));
    assert!(
        !other_vm.is_bootstrap_appended_class("w23l5/Appended"),
        "another VM's bootstrap search has no such jar"
    );
    assert!(!appending_vm.is_bootstrap_appended_class("w23l5/Absent"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Round 13 wave 10 (lane misc10): an appended DIRECTORY is censused from
/// its class files, nested packages and the default package included.
#[test]
fn an_appended_bootstrap_directory_is_censused() {
    let dir = std::env::temp_dir().join("cratonvm_r13w10_bootstrap_appended_dir");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("w10dir").join("sub")).unwrap();
    std::fs::write(
        dir.join("w10dir").join("sub").join("Dir.class"),
        b"not parsed",
    )
    .unwrap();
    std::fs::write(dir.join("Top$Nested.class"), b"not parsed").unwrap();
    std::fs::write(dir.join("w10dir").join("notes.txt"), b"not a class").unwrap();
    let mut vm = ClassManager::new(&[], &[], &[]);
    vm.extend_bootstrap_classpath(&[dir.to_str().unwrap().to_string()]);
    let other = ClassManager::new(&[], &[], &[]);
    if true {
        assert!(vm.is_bootstrap_appended_class("w10dir/sub/Dir"));
        assert!(vm.is_bootstrap_appended_class("Top$Nested"));
    }
    assert!(!vm.is_bootstrap_appended_class("w10dir/notes.txt"));
    assert!(!other.is_bootstrap_appended_class("w10dir/sub/Dir"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Round 15 wave 5 (lane proxy8): an append files the packages of its
/// classes on THIS VM's store as split boot packages, except a `java/*`
/// package and one a named module claims; a class-path (automatic)
/// module's claim does not count, its classes are in the unnamed module.
#[test]
fn an_append_files_its_unnamed_packages_as_split_boot_packages() {
    let dir = std::env::temp_dir().join("cratonvm_r15w5_proxy8_split_boot_packages");
    let _ = std::fs::remove_dir_all(&dir);
    for (pkg, class) in [
        ("w5p8", "Base"),
        ("w5claimed", "Claimed"),
        ("w5auto", "Auto"),
        ("java/lang", "Evil"),
    ] {
        std::fs::create_dir_all(dir.join(pkg)).unwrap();
        std::fs::write(dir.join(pkg).join(format!("{class}.class")), b"not parsed").unwrap();
    }
    std::fs::write(dir.join("Top.class"), b"not parsed").unwrap();
    let descriptor = |name: &str, automatic: bool| crate::module::ModuleDescriptor {
        name: name.to_string(),
        version: None,
        is_open: false,
        requires: Vec::new(),
        exports: Vec::new(),
        opens: Vec::new(),
        uses: Vec::new(),
        provides: Vec::new(),
        automatic,
        main_class: None,
    };
    let mut vm = ClassManager::new(&[], &[], &[]);
    vm.module_registry.register(
        descriptor("w5.claimed", false),
        vec!["w5claimed".to_string()],
    );
    vm.module_registry
        .register(descriptor("w5.auto", true), vec!["w5auto".to_string()]);
    vm.extend_bootstrap_classpath(&[dir.to_str().unwrap().to_string()]);
    let other = ClassManager::new(&[], &[], &[]);
    if true
    {
        assert!(vm.class_store.is_split_boot_package("w5p8"));
        assert!(vm.class_store.is_split_boot_package("w5auto"));
        assert!(
            vm.class_store.is_split_boot_package(""),
            "the default package"
        );
    }
    assert!(
        !vm.class_store.is_split_boot_package("w5claimed"),
        "a named module's"
    );
    assert!(!vm.class_store.is_split_boot_package("java/lang"));
    assert!(
        !other.class_store.is_split_boot_package("w5p8"),
        "another VM's store"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
