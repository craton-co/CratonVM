// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 5 (lane DROPGAP): page
//! `docs/internal/jit/j27-rescued-body-drop-frees-maps-early-FIXED-20261029.md`.
//!
//! A body entered by a FRAMELESS hand-off (a lambda thunk's `JMP r11`, a
//! not-entrant stub's forward, a tail `JMP`) has no frame of the jumper on the
//! stack, so the retirement queue may release the jumper while a thread is
//! still inside the target. That is sound only because the jumper owns its
//! target through a [`RetainedCode`] (`_direct_callee_roots`,
//! `NotEntrantForward::target`), whose last drop queues the target WHOLE for a
//! proof about the target's own buffer. These tests pin that link: if it were
//! a bare `Arc`, the target would die as a cascade of the jumper's release and
//! `CompiledMethod::drop` would withdraw its code range, compile id and GC maps
//! under a thread still running it.

use super::*;

fn published_body() -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]); // RET
    let mut cm = CompiledMethod::new(buf);
    let _ = cm._buffer.mark_published();
    cm
}

/// A published jumper whose ONLY owner of `target` is a callee root (the
/// lambda thunk's shape), dropped by `release` while this thread is inside
/// compiled code. Answers whether the target is still a whole artifact.
fn target_survives(release: impl FnOnce(CompiledMethod)) -> bool {
    let token = jit_execution_enter();
    let target = Arc::new(published_body());
    let weak = Arc::downgrade(&target);
    let mut jumper = published_body();
    jumper._direct_callee_roots.push(RetainedCode::new(target));
    release(jumper);
    let alive = weak.upgrade().is_some();
    jit_execution_leave(token);
    alive
}

/// The queue releases the jumper on a proof about the JUMPER's buffer (no
/// frame of it is on any stack: the hand-off was a jump). Its root on the
/// target is the last owner, and it must leave through the queue.
#[test]
fn a_frameless_target_outlives_its_jumpers_queue_release() {
    assert!(target_survives(|jumper| {
        let base = jumper.entry_ptr() as usize;
        let _proof = AuthorisedReclaim::enter_for(base);
        drop(jumper);
    }));
}

/// Same with the holder-count kill switch: one handle, so the `strong_count`
/// shortcut also routes it.
#[test]
fn a_frameless_target_outlives_its_jumper_without_the_holder_count() {
    cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_JIT_RETAINED_HOLDER_COUNT", Some("0"))],
        || {
            assert!(target_survives(|jumper| {
                let base = jumper.entry_ptr() as usize;
                let _proof = AuthorisedReclaim::enter_for(base);
                drop(jumper);
            }));
        },
    );
}

/// A stray (unauthorised) last drop of the jumper is RESCUED: its callee roots
/// wait in the queue with its mapping (`RescuedSideTables`), so the target
/// stays owned too.
#[test]
fn a_frameless_target_outlives_its_jumpers_stray_drop() {
    assert!(target_survives(|jumper| {
        assert!(jumper._buffer.release_would_be_rescued());
        drop(jumper);
    }));
}
