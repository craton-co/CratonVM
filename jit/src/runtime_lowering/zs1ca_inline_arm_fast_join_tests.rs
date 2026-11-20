use super::*;

const FRAME_RECORD: usize = 0x7FFF_0000_1000;

fn target_of(buf: &ExecutableBuffer, patch: usize) -> usize {
    let b = &buf.as_slice()[patch..patch + 4];
    let rel = i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    (patch as i64 + 4 + i64::from(rel)) as usize // Cast: a small code offset
}

fn array_plan(announce: usize) -> InlineTlabArrayPlan {
    InlineTlabArrayPlan {
        thread_slot_off: 8,
        thread_tls_disp: 0,
        cursor_off: 0,
        end_off: 8,
        context_off: 16,
        atype: 11, // T_LONG
        length_off: 24,
        announce,
        component_class_id: None,
        const_length: None,
    }
}

fn new_plan(announce: usize) -> InlineTlabPlan {
    InlineTlabPlan {
        thread_slot_off: 8,
        thread_tls_disp: 0,
        cursor_off: 0,
        end_off: 8,
        context_off: 16,
        class_id: 915_160,
        num_fields: 2,
        post_init: 0x3000,
        announce,
        skip_post_init: false,
        register_first: 0,
        post_init_helper_first: false,
    }
}

/// `(plain bytes, fast bytes, handed-back patch)` for one site, with the
/// keys in `edits` overridden on this thread.
fn both(
    edits: &[(&str, Option<&str>)],
    array: bool,
    announce: usize,
    frame_record: usize,
) -> (Vec<u8>, Vec<u8>, Option<usize>) {
    let _g = cratonvm_types::flags::override_thread(
        cratonvm_types::flags::VmFlags::from_env_with_edits(edits),
    );
    let mut plain = ExecutableBuffer::new(4096).expect("buffer");
    let mut fast = ExecutableBuffer::new(4096).expect("buffer");
    let mut join = None;
    if array {
        let plan = array_plan(announce);
        emit_inline_tlab_newarray_ir(&mut plain, &plan, 0x1000, frame_record)
            .expect("the bump emits");
        emit_inline_tlab_newarray_ir_fast_join(
            &mut fast,
            &plan,
            0x1000,
            frame_record,
            &mut join,
        )
        .expect("the bump emits");
    } else {
        let plan = new_plan(announce);
        emit_inline_tlab_new_ir(&mut plain, &plan, 0x1000, frame_record)
            .expect("the bump emits");
        emit_inline_tlab_new_ir_fast_join(&mut fast, &plan, 0x1000, frame_record, &mut join)
            .expect("the bump emits");
    }
    (plain.as_slice().to_vec(), fast.as_slice().to_vec(), join)
}







