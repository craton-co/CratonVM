    //! JIT round 21 wave 4, lane ic, IC21-3 (`CRATONVM_JIT_MEGA_IFACE_COLUMN_SPACE`):
    //! per-selector interface columns from their own range, and the row growth
    //! that lets every receiver reach them.
    use super::*;
    use crate::{ExecutableBuffer, JitCache};

fn ret_body() -> CompiledMethod {
        let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
        buf.emit(&[0xC3]); // RET
        CompiledMethod::new(buf)
    }

    fn published(cache: &JitCache, name: &str) -> Arc<CompiledMethod> {
        let cid = cratonvm_types::ClassId::new(0x2143);
        let class: Arc<str> = Arc::from("R21IcIfaceSpace");
        let method: Arc<str> = Arc::from(name);
        let desc: Arc<str> = Arc::from("()V");
        cache.put(class.clone(), method.clone(), desc.clone(), cid, ret_body());
        cache.get(&class, &method, &desc, cid).expect("published")    }