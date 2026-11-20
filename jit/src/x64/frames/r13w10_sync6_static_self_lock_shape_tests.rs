//! Round 13 wave 10 (lane sync6): a `static synchronized` self-locking
//! body locks its class mirror through a scratch word it refreshes from the
//! VM's mirror slot right before every sequence that reads it.

use super::super::emit::tests::test_compiler;

const ENTER: usize = 0x0123_4567_89AB_CDE1;
const EXIT: usize = 0x0123_4567_89AB_CDE7;
const SLOT: usize = 0x0123_4567_89AB_CDF0;
const SCRATCH_OFF: i32 = 24;
/// `XOR R11D, R11D`, the clear's first half.
const CLEAR: [u8; 3] = [0x45, 0x31, 0xDB];

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).rposition(|w| w == needle)
}

/// `MOV R11, imm64 SLOT`: the refresh's first instruction.
fn refresh() -> Vec<u8> {
    let mut bytes = vec![0x49, 0xBB];
    bytes.extend_from_slice(&SLOT.to_le_bytes());
    bytes
}

fn static_self_locking_compiler() -> super::super::Compiler {
    let mut c = test_compiler();
    c.direct_helpers.monitor_enter = ENTER;
    c.direct_helpers.monitor_exit = EXIT;
    c.needs_heap = true;
    c.heap_local_offset = 16;
    c.self_lock_obj_off = SCRATCH_OFF;
    c.self_lock_mirror_slot = SLOT;
    c
}

/// The enter loads the mirror before the helper reads the word, names the
/// word in the helper's map (the staged list is taken), and clears it once
/// entered.
#[test]
fn the_static_enter_refreshes_names_and_clears_its_scratch_word() {
    let mut c = static_self_locking_compiler();
    let start = c.buf.pos();
    assert!(c.emit_self_lock_enter());
    let enter = c.buf.as_slice()[start..].to_vec();
    let refresh_at = find(&enter, &refresh()).expect("the mirror is loaded from its slot");
    let call = find(&enter, &ENTER.to_le_bytes()).expect("the enter helper is called");
    assert!(
        refresh_at < call,
        "refreshed before the helper reads the word"
    );
    let clear = rfind(&enter, &CLEAR).expect("the word is cleared");
    assert!(clear > call, "cleared once entered");
    assert!(
        c.pending_staged_arg_oops.is_empty(),
        "the helper's map took the named word"
    );
    assert!(c.self_lock_release_armed);
    assert!(!c.failed);
}

/// Every armed epilogue refreshes before it releases; an unarmed one (a
/// return arm, which released already) touches neither the slot nor the
/// exit helper.
#[test]
fn a_static_armed_epilogue_refreshes_before_it_releases() {
    let mut c = static_self_locking_compiler();
    c.self_lock_release_armed = true;
    let start = c.buf.pos();
    c.emit_epilogue();
    let armed = c.buf.as_slice()[start..].to_vec();
    let refresh_at = find(&armed, &refresh()).expect("the mirror is loaded from its slot");
    let call = find(&armed, &EXIT.to_le_bytes()).expect("the exit helper is called");
    assert!(refresh_at < call);

    let start = c.buf.pos();
    c.emit_epilogue_after_self_lock_release();
    let unarmed = c.buf.as_slice()[start..].to_vec();
    assert_eq!(find(&unarmed, &refresh()), None);
    assert_eq!(find(&unarmed, &EXIT.to_le_bytes()), None);
    assert!(!c.failed);
}


/// An instance body (no mirror slot) emits no refresh or clear.
#[test]
fn an_instance_body_has_no_scratch_word_traffic() {
    let mut c = static_self_locking_compiler();
    c.self_lock_mirror_slot = 0;
    c.self_lock_obj_off = 8;
    let start = c.buf.pos();
    assert!(c.emit_self_lock_enter());
    let enter = c.buf.as_slice()[start..].to_vec();
    assert_eq!(find(&enter, &SLOT.to_le_bytes()), None);
    assert!(c.pending_staged_arg_oops.is_empty());
}
