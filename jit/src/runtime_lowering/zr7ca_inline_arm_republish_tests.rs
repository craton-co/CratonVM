use super::*;

/// A frame-record helper address: never called (the bytes are decoded,
/// not run), but non-zero so the republish is emitted.
const FRAME_RECORD: usize = 0x7FFF_0000_1000;

/// The absolute target of the `rel32` at `patch`.
fn target_of(buf: &ExecutableBuffer, patch: usize) -> usize {
    let b = &buf.as_slice()[patch..patch + 4];
    let rel = i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    (patch as i64 + 4 + i64::from(rel)) as usize // Cast: a small code offset
}

/// `(buffer, returned patch, the inline jump's patch, helper-arm start,
/// end of the republish)` after a stand-in inline arm (`JMP rel32`) and
/// the join.
fn emit(
    inline: bool,
    frame_record: usize,
) -> (ExecutableBuffer, Option<usize>, usize, usize, usize) {
    let mut buf = ExecutableBuffer::new(512).expect("buffer");
    let jmp = emit_jmp(&mut buf);
    let helper_arm = buf.pos();
    let back = emit_zgc_announce_join_republish(&mut buf, inline.then_some(jmp), frame_record);
    let end = buf.pos();
    (buf, back, jmp, helper_arm, end)
}



