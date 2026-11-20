    use super::*;
    use crate::lambda_adapter::w10_5_supersede::take_adapters_into;
    use crate::lambda_adapter::{adapters_reaching, lambda_adapter_entry};
    use std::sync::atomic::Ordering;

    fn ret_body() -> CompiledMethod {
        let mut buf = ExecutableBuffer::new(64).expect("an executable buffer");
        buf.emit(&[0xC3]); // ret
        CompiledMethod::new(buf)
    }



