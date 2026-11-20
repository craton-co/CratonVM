use super::ExecuteReentryRecord;
use crate::classloading::resolution::CachedBytecodeMethod;
use crate::classloading::ClassId;
use cratonvm_reader::ByteView;
use std::sync::Arc;

fn record(code: &ByteView) -> ExecuteReentryRecord {
    let cached = CachedBytecodeMethod::from_parts(cratonvm_jit_api::CachedMethodParts {
        declaring_class_id: ClassId::new(790_101),
        class_name: Arc::from("i79/Reentry"),
        method_name: Arc::from("m"),
        method_descriptor: Arc::from("()V"),
        source_file: None,
        code: crate::runtime::frame::padded_bytecode(code),
        exception_table: Arc::from(Vec::new().into_boxed_slice()),
        max_stack: 1,
        max_locals: 0,
        num_params: 0,
        is_synchronized: false,
        is_static: true,
    });
    ExecuteReentryRecord {
        code_view: code.clone(),
        cached: Arc::new(cached),
    }
}

#[test]
fn a_record_answers_for_its_own_code_view_only() {
    let code = ByteView::from_vec(vec![0xb1]);
    let row = record(&code);
    // The class's own view (a clone shares the allocation): a hit.
    assert!(row.built_from(&code.clone()));
    // Equal bytes in another allocation (a redefinition, an EMCP reload,
    // another class under the id): a miss.
    assert!(!row.built_from(&ByteView::from_vec(vec![0xb1])));
    assert!(!row.built_from(&ByteView::from_vec(vec![0x04, 0xac])));
}
