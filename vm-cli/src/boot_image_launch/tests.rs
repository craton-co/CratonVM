use super::*;

#[test]
fn the_static_class_list_parses_and_excludes_the_allocator() {
    let l = ClassList::parse(STATIC_CLASSES).expect("boot_image_statics.txt parses");
    assert_eq!(l.classify("heap_main"), Some(StaticClass::Exclude));
    assert_eq!(l.classify("_mi_heap_default"), Some(StaticClass::Exclude));
    assert_eq!(l.classify("a::b::X::__RUST_STD_INTERNAL_VAL"), Some(StaticClass::Keep));
    assert_eq!(l.classify("some::new::STATIC"), None);
}

#[test]
fn the_code_window_reservation_is_not_restored_from_an_image() {
    let classes = ClassList::parse(STATIC_CLASSES).unwrap();
    assert_eq!(
        classes.classify("cratonvm_jit::platform::code_window::WINDOW"),
        Some(StaticClass::Exclude),
    );
}

#[test]
fn pruning_keeps_the_newest_images_and_drops_the_markers_of_the_rest() {
    let dir = std::env::temp_dir().join(format!("cvm-prune-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..5u64 {
        let f = dir.join(format!("{i}.img"));
        std::fs::write(&f, b"x").unwrap();
        std::fs::write(refused_marker(&f), b"why").unwrap();
        let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000 + i);
        std::fs::File::options().write(true).open(&f).unwrap().set_modified(t).unwrap();
    }
    prune_images(&dir, 2);
    let mut left: Vec<String> =
        std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    left.sort();
    // 3.img and 4.img survive with their markers; 0-2 and their markers are gone.
    assert_eq!(left, ["3.img", "3.refused", "4.img", "4.refused"]);
    let _ = std::fs::remove_dir_all(&dir);
}
