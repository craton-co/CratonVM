    //! JIT round 21 wave 4, lane ic, IC21-1 (`CRATONVM_JIT_MEGA_CLASS_ROW_GROW`):
    //! a deeper column replaces a receiver's cell array by a larger copy
    //! instead of being refused for the class's life.
    use super::*;
    use crate::{ExecutableBuffer, JitCache};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

fn ret_body() -> CompiledMethod {
        let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
        buf.emit(&[0xC3]); // RET
        CompiledMethod::new(buf)
    }

    fn published(cache: &JitCache, name: &str) -> Arc<CompiledMethod> {
        let cid = cratonvm_types::ClassId::new(0x2104);
        let class: Arc<str> = Arc::from("R21IcRowGrow");
        let method: Arc<str> = Arc::from(name);
        let desc: Arc<str> = Arc::from("()V");
        cache.put(class.clone(), method.clone(), desc.clone(), cid, ret_body());
        cache.get(&class, &method, &desc, cid).expect("published")
    }

    /// The array address the current directory holds for `class_id`.
    fn row_of(table: &MegaDispatchTable, class_id: u32) -> usize {
        // SAFETY: `class_root_addr` is the table's own boxed root word; the
        // root names a published directory `[len, row..]` whose `len` covers
        // every class id these tests publish (at least `MEGA_CLASS_DIR_MIN`).
        unsafe {
            let root = &*(table.class_root_addr() as *const AtomicUsize);
            let dir = root.load(Ordering::Acquire) as *const AtomicUsize;
            assert!((*dir).load(Ordering::Acquire) > class_id as usize);
            (*dir.add(1 + class_id as usize)).load(Ordering::Acquire)        }    }