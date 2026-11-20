//! Round 13 wave 4 (lane sync2): the emitted shape of a self-locking body's
//! monitor enter (prologue side) and release (every sentinel exit).

use super::super::emit::tests::test_compiler;

const ENTER: usize = 0x0123_4567_89AB_CDE1;
const EXIT: usize = 0x0123_4567_89AB_CDE7;
/// `MOV RAX, i64::MIN`.
const SENTINEL: [u8; 10] = [0x48, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0x80];

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn self_locking_compiler() -> super::super::Compiler {
    let mut c = test_compiler();
    c.direct_helpers.monitor_enter = ENTER;
    c.direct_helpers.monitor_exit = EXIT;
    c.needs_heap = true;
    c.heap_local_offset = 16;
    c.self_lock_obj_off = 8;
    c
}

/// An armed epilogue calls the exit helper on the receiver's home, THEN
/// re-materialises the sentinel, THEN runs the unchanged epilogue. An
/// unarmed one (a return arm, which released through the walk, or any
/// other compile) is byte-identical to the plain epilogue.
#[test]
fn an_armed_epilogue_releases_before_the_unchanged_teardown() {
    let mut c = self_locking_compiler();
    let start = c.buf.pos();
    c.emit_epilogue();
    let unarmed = c.buf.as_slice()[start..].to_vec();
    assert_eq!(find(&unarmed, &EXIT.to_le_bytes()), None);

    c.self_lock_release_armed = true;
    let start = c.buf.pos();
    c.emit_epilogue();
    let armed = c.buf.as_slice()[start..].to_vec();
    let call = find(&armed, &EXIT.to_le_bytes()).expect("the exit helper is called");
    let sentinel = find(&armed, &SENTINEL).expect("the sentinel is re-materialised");
    assert!(call < sentinel, "the release precedes the sentinel reload");
    assert!(
        armed.ends_with(&unarmed),
        "the teardown itself is unchanged"
    );

    let start = c.buf.pos();
    c.emit_epilogue_after_self_lock_release();
    assert_eq!(c.buf.as_slice()[start..].to_vec(), unarmed);
    assert!(
        c.self_lock_release_armed,
        "the return arms' disarm is scoped"
    );
    assert!(!c.failed);
}

/// The enter calls the enter helper, tests the sentinel, and its failure
/// exit is an epilogue that releases NOTHING; afterwards every epilogue
/// is armed.
#[test]
fn the_enter_arms_every_later_exit_and_its_own_failure_exit_releases_nothing() {
    let mut c = self_locking_compiler();
    let start = c.buf.pos();
    assert!(c.emit_self_lock_enter());
    let enter = c.buf.as_slice()[start..].to_vec();
    let call = find(&enter, &ENTER.to_le_bytes()).expect("the enter helper is called");
    let test =
        find(&enter, &[0x48, 0x83, 0xF8, 0x01, 0x0F, 0x81]).expect("CMP RAX, 1; JNO entered");
    assert!(call < test);
    assert_eq!(
        find(&enter, &EXIT.to_le_bytes()),
        None,
        "the failure exit holds nothing and must not release"
    );
    assert!(c.self_lock_release_armed);
    assert!(c.emitted_monitor_call, "the dispatch-aware entry is forced");
    assert_eq!(c.cur_bc_pc, 0, "the synthetic entry pc is restored");
}

/// Round 13 wave 6 (lane sync4,
/// `r13w6-sync3-self-lock-return-release-failure-releases-twice-FIXED-20260928.md`):
/// the exception-check stub serving a return arm's release (a tagged
/// entry) leaves without a second release; an untagged stub of the same
/// armed body still releases.
#[test]
fn a_return_release_failure_stub_does_not_release_twice() {
    use super::super::deopt_stubs::SELF_LOCK_RELEASED_EXC_BCI_TAG as TAG;
    let mut c = self_locking_compiler();
    c.self_lock_release_armed = true;
    let released_edge = c.emit_jcc_rel32_patch(0x80);
    c.exception_check_stubs.push((released_edge, 7 | TAG));
    let other_edge = c.emit_jcc_rel32_patch(0x80);
    c.exception_check_stubs.push((other_edge, 3));
    c.emit_exception_check_stub();
    assert!(!c.buf.overflowed());
    assert!(!c.failed);
    let code = c.buf.as_slice().to_vec();
    let target = |patch: usize| -> usize {
        let rel = i32::from_le_bytes([
            code[patch],
            code[patch + 1],
            code[patch + 2],
            code[patch + 3],
        ]);
        // Cast: a rel32 target inside the test buffer.
        (patch as i64 + 4 + i64::from(rel)) as usize
    };
    let released_stub = target(released_edge);
    let other_stub = target(other_edge);
    assert!(
        released_stub < other_stub,
        "one stub per entry, in entry order"
    );
    assert_eq!(
        find(&code[released_stub..other_stub], &EXIT.to_le_bytes()),
        None,
        "the return release's failure stub must not release again"
    );
    assert!(
        find(&code[other_stub..], &EXIT.to_le_bytes()).is_some(),
        "every other sentinel exit of the armed body still releases"
    );
    assert!(c.self_lock_release_armed, "the stub's disarm is scoped");
}

/// A sibling tail call would outlive the release; it refuses the compile.
#[test]
fn a_sibling_tail_call_refuses_a_self_locking_compile() {
    let mut c = self_locking_compiler();
    c.emit_epilogue_without_ret();
    assert!(c.failed);
    let mut plain = test_compiler();
    plain.emit_epilogue_without_ret();
    assert!(!plain.failed);
}
