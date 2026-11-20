use crate::vm::SharedVm;
use crate::config::VmConfig;
use cratonvm_types::ClassId;

#[test]
fn source_decoding_keeps_the_mirrors_first_file_until_an_unavailable_version_clears_it() {
    let vm = SharedVm::new(VmConfig::default());
    let cid = ClassId::new(7);
    let file = |captured| vm.classes.stack_trace_source_file(cid, captured);
    assert_eq!(file(Some("Old.java")).as_deref(), Some("Old.java"));
    assert_eq!(file(Some("New.java")).as_deref(), Some("Old.java"));
    assert_eq!(file(None), None);
    assert_eq!(file(Some("New.java")).as_deref(), Some("New.java"));
    assert_eq!(vm.classes.stack_trace_source_file(ClassId::new(8), Some("Other.java")).as_deref(), Some("Other.java"));
    let other = SharedVm::new(VmConfig::default());
    assert_eq!(other.classes.stack_trace_source_file(cid, Some("OtherVM.java")).as_deref(), Some("OtherVM.java"));
}
