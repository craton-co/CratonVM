use super::*;
use crate::ir::{IrBuilder, ThreadLocalIrLayout};
use std::collections::HashMap;


fn layout() -> ThreadLocalIrLayout {
    ThreadLocalIrLayout {
        thread_obj_offset: 64,
        thread_local_class: 40,
        supplied_class: 0,
        map_class: 42,
        entry_class: 43,
        thread_locals_index: 5,
        table_index: 1,
        hash_index: 0,
        referent_index: 0,
        value_index: 4,
        thread_locals_row: 0,
        table_row: 0,
        hash_row: 0,
        value_row: 0,
        set_admitted: false,
        inheritable_class: 0,
        inheritable_thread_locals_index: 0,
        inheritable_thread_locals_row: 0,
        referent_row: 0,
    }
}


fn has_arg(hay: &[u8], want: u64) -> bool {
    hay.windows(8).any(|w| {
        let v = u64::from_le_bytes(w.try_into().expect("an 8-byte window"));
        v & !cratonvm_jit_api::GETFIELD_NPE_SITE_MASK == want
    })
}


