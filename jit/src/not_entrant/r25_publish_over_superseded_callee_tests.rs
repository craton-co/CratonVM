    use super::*;
    use crate::ExecutableBuffer;
    use std::sync::atomic::Ordering;

/// `NOP5; MOV EAX, value; RET`, carrying `recorded` as its inlined methods.
    fn body(value: u8, recorded: &[(String, String, String)]) -> CompiledMethod {
        let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit(&ENTRY_PATCH_PAD);
        buf.emit(&[0xB8, value, 0x00, 0x00, 0x00, 0xC3]);
        let mut cm = CompiledMethod::new(buf);
        cm.inlined_methods = recorded.to_vec();
        cm
    }

    /// The not-entrant record of `r25/S.m()I`.
    fn record() -> Box<NotEntrantRecord> {
        Box::new(NotEntrantRecord::new(
            &Arc::from("r25/S"),
            &Arc::from("m"),
            &Arc::from("()I"),
            0x2501,
            true,
            0,
            false,
            0,
        ))
    }

    /// Never reached: no test calls a patched entry.
    extern "C" fn unreached(_record: i64, _regs: i64, _stack_args: i64) -> i64 {
        0
    }

    fn caller() -> crate::JitKey {
        crate::JitKey {
            class_name: Arc::from("r25/Caller"),
            method_name: Arc::from("compile"),
            descriptor: Arc::from("()V"),
            declaring_class_id: cratonvm_types::ClassId::new(0x2502),        }    }