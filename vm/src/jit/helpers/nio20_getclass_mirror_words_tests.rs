use super::*;
use crate::config::VmConfig;

#[test]
fn the_compile_table_names_this_vms_words_and_a_served_class_publishes_its_mirror() {
    if !cfg!(target_arch = "x86_64") || !cratonvm_jit::getclass_inline_enabled() {
        // `CRATONVM_JIT_GETCLASS_INLINE=0`, or a backend with no reader:
        // no table is published, which is the other arm.
        return;
    }
    let shared = SharedVm::new(VmConfig::default());
    let table = direct_helper_table_for(&shared);
    assert_ne!(table.getclass_mirror_words, 0);
    assert_eq!(
        table.getclass_mirror_words,
        shared.classes.getclass_mirror_words.base_addr(),
        "the compile bakes THIS VM's table"
    );
    let other = SharedVm::new(VmConfig::default());
    assert_ne!(
        direct_helper_table_for(&other).getclass_mirror_words,
        table.getclass_mirror_words,
        "per VM, never a process global"
    );

    let cid = ClassId::new(62_001);
    let mirror = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    shared.classes.class_mirrors.write().insert(cid, mirror);
    assert_eq!(
        stamped_class_mirror_publishing_word(&shared, cid),
        Some(mirror)
    );
    // Cast: the mirror's address, as compiled code reads the word.
    assert_eq!(
        shared.classes.getclass_mirror_words.get(cid),
        mirror.as_ptr() as usize
    );

    // No mirror: nothing to answer, nothing published.
    let absent = ClassId::new(62_002);
    assert_eq!(stamped_class_mirror_publishing_word(&shared, absent), None);
    assert_eq!(shared.classes.getclass_mirror_words.get(absent), 0);
}
