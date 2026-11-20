//! Round 16, lane mega: `CRATONVM_JIT_MEGA_STUB_HASHED_FIRST` keeps the
//! megamorphic gate and emits the stub in the round-12-wave-5 order
//! (site's hashed ways, then the class cell, then the shared table),
//! byte for byte the stub `CRATONVM_JIT_MEGA_CELL_FIRST=0` emits; off, the
//! stub is the cell-first one, as before.
use super::*;


fn stub_bytes(edits: &[(&str, Option<&str>)]) -> Vec<u8> {
    cratonvm_types::flags::with_thread_overrides(edits, || {
        let mut buf = ExecutableBuffer::new(4096).expect("buffer");
        emit_hashed_vtable_stub(&mut buf, 0x7fff_0000_0000_2000, 24, &[32, 40], 0, 0, 0, 32);
        assert!(!buf.overflowed());
        buf.as_slice().to_vec()        })    }