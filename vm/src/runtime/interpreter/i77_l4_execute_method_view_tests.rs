use super::{execute_exception_table, ExecuteMethodView};
use cratonvm_reader::attribute::ExceptionTableEntry;
use std::sync::Arc;

#[test]
fn on_the_view_shares_the_class_and_method_names() {
    let class: Arc<str> = Arc::from("java/lang/invoke/MethodType");
    let name: Arc<str> = Arc::from("makeImpl");
    let desc: Arc<str> =
        Arc::from("(Ljava/lang/Class;[Ljava/lang/Class;Z)Ljava/lang/invoke/MethodType;");
    let view = ExecuteMethodView {
        shared: true,
        names: Some((Arc::clone(&name), Arc::clone(&desc))),
    };
    assert!(Arc::ptr_eq(&view.class_name(&class), &class));
    assert!(Arc::ptr_eq(&view.method_name("makeImpl"), &name));
    assert!(Arc::ptr_eq(&view.descriptor(&desc), &desc));
}

#[test]
fn off_the_view_copies_the_same_text() {
    let class: Arc<str> = Arc::from("java/lang/invoke/MethodType");
    let view = ExecuteMethodView {
        shared: false,
        names: None,
    };
    let copy = view.class_name(&class);
    assert!(!Arc::ptr_eq(&copy, &class));
    assert_eq!(&*copy, &*class);
    assert_eq!(&*view.method_name("makeImpl"), "makeImpl");
    assert_eq!(&*view.descriptor("()V"), "()V");
}

#[test]
fn the_exception_table_is_copied_whole() {
    assert!(execute_exception_table(&[]).is_empty());
    let row = ExceptionTableEntry {
        start_pc: 0,
        end_pc: 4,
        handler_pc: 7,
        catch_type: 0,
    };
    let t = execute_exception_table(std::slice::from_ref(&row));
    assert_eq!(&t[..], std::slice::from_ref(&row));
}
