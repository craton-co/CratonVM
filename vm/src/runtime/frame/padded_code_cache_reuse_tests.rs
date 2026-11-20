use super::*;

#[test]
fn padded_bytecode_memo_detects_reused_input_storage() {
    let cid = ClassId::new(790_008);
    let mut code = vec![0x03, 0xac];
    let mut handlers = [ExceptionTableEntry {
        start_pc: 0,
        end_pc: 1,
        handler_pc: 1,
        catch_type: 0,
    }];
    let first = padded_bytecode_for_method(cid, "reused", "()I", &code, &handlers);
    code[0] = 0x04;
    let second = padded_bytecode_for_method(cid, "reused", "()I", &code, &handlers);
    assert_eq!(second[0], 0x04);
    assert!(!Arc::ptr_eq(&first, &second));
    handlers[0].end_pc = 2;
    let third = padded_bytecode_for_method(cid, "reused", "()I", &code, &handlers);
    assert!(!Arc::ptr_eq(&second, &third));
    let fourth = padded_bytecode_for_method(cid, "reused", "()I", &code, &handlers);
    assert!(Arc::ptr_eq(&third, &fourth));
}
