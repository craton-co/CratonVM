use super::*;

fn entry(holder: u32, cp_idx: u16, epoch: u64) -> NewCpSiteMemo {
    NewCpSiteMemo {
        vm_identity: 0x51,
        epoch,
        holder,
        cp_idx,
        target: holder + 1000,
        num_fields: 5,
    }
}

#[test]
fn a_filled_site_answers_its_own_key_only() {
    new_cp_site_memo_fill(entry(4242, 17, 9));
    assert_eq!(new_cp_site_memo_probe(0x51, 9, 4242, 17), Some((5242, 5)));
    assert_eq!(
        new_cp_site_memo_probe(0x52, 9, 4242, 17),
        None,
        "another VM"
    );
    assert_eq!(
        new_cp_site_memo_probe(0x51, 10, 4242, 17),
        None,
        "a class was defined"
    );
    // Another site of the same holder: a miss even if it shares the slot.
    assert_ne!(new_cp_site_memo_probe(0x51, 9, 4242, 18), Some((5242, 5)));
}

#[test]
fn a_colliding_fill_replaces_rather_than_aliases() {
    // Find a second key that maps to the same slot as (7, 3).
    let slot = new_cp_site_memo_index(7, 3);
    let other = (0u16..u16::MAX)
        .map(|cp| (8u32, cp))
        .find(|&(h, cp)| new_cp_site_memo_index(h, cp) == slot)
        .expect("64 slots: some index of holder 8 collides");
    new_cp_site_memo_fill(entry(7, 3, 1));
    new_cp_site_memo_fill(entry(other.0, other.1, 1));
    assert_eq!(new_cp_site_memo_probe(0x51, 1, 7, 3), None);
    assert_eq!(
        new_cp_site_memo_probe(0x51, 1, other.0, other.1),
        Some((other.0 + 1000, 5))
    );
}

#[test]
fn every_index_is_in_range() {
    for holder in [0u32, 1, 4096, u32::MAX] {
        for cp in [0u16, 1, 255, u16::MAX] {
            assert!(new_cp_site_memo_index(holder, cp) < NEW_CP_SITE_MEMO_SLOTS);
        }
    }
}

/// A text pin (the helper cannot be driven without compiled code): the
/// virtual-MIC helper's miss arm throttles a declined probe and hands a
/// body the PIC had no room for to the per-thread entry cache, and its
/// megamorphic arm refuses a superseded or retired way. Round 11 wave 18.
#[test]
fn the_mic_miss_arm_does_not_repeat_its_resolution_per_call() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let start = src
        .find("\nunsafe fn jit_invoke_virtual_mic_body_inner(")
        .expect("the MIC body exists");
    let rest = &src[start + 1..];
    let body = &rest[..rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3)];
    let miss = &body[body
        .find("// --- Cache miss: full resolution + update cache ---")
        .expect("the miss arm exists")..];
    assert!(
        miss.contains("MIC_COMPILE_DECLINED.with("),
        "the miss arm re-probes a declined callee on every call"
    );
    assert!(
        miss.contains(".lookup_megamorphic(receiver_cid)")
            && miss.contains("mic.callee_rerun_pins(),"),
        "a body the PIC has no room for is not handed to the entry cache"
    );
    assert!(
        body.contains("cm.superseded.load(std::sync::atomic::Ordering::Acquire)"),
        "the megamorphic arm calls a superseded way"
    );
}
