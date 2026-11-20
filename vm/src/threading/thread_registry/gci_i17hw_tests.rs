use super::*;

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: compared and rewritten by address only; never dereferenced.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn addrs(snapshot: &[ObjectRef]) -> Vec<usize> {
    snapshot.iter().map(|r| r.as_ptr() as usize).collect()
}

fn map(pairs: &[(usize, usize)]) -> cratonvm_types::PointerMap {
    pairs.iter().copied().collect()
}

/// The remount applies the composed fixup with ONE lookup per slot
/// (`Frame::update_frame_refs_mapped`); this is that lookup.
fn wake(fixup: &cratonvm_types::PointerMap, frame_word: usize) -> usize {
    fixup.get(&frame_word).copied().unwrap_or(frame_word)
}

#[test]
fn gci_i17hw_a_parked_holder_follows_its_object_through_a_drain_collision() {
    // The parked frame holds the holder P at 0x1000 and another object Q
    // at 0x2000; the deposit's snapshot names both.
    let (p0, q0) = (0x1000usize, 0x2000usize);
    let mut fixup = cratonvm_types::PointerMap::default();
    let mut snapshot = vec![obj(p0), obj(q0)];

    // Pause 1 (mixed): the Old holder moves.
    assert_eq!(
        fold_one_blocked_chain(&mut fixup, &mut snapshot, &map(&[(p0, 0x3000)])),
        (0, 1)
    );
    assert_eq!(addrs(&snapshot), vec![0x3000, q0]);

    // Pause 2 (evacuation failure + drain, the composed map): pass 1
    // copied Q away from 0x2000 and self-forwarded P; the drain copied P
    // INTO 0x2000 (a region pass 1 freed). A pass-2-only key rides along.
    let composed = map(&[(q0, 0x5000), (0x3000, q0), (0x6000, 0x6100)]);
    assert_eq!(
        fold_one_blocked_chain(&mut fixup, &mut snapshot, &composed),
        (1, 1)
    );
    assert_eq!(addrs(&snapshot), vec![q0, 0x5000]);
    assert_eq!(fixup.get(&p0), Some(&q0), "P's chain advanced once");
    assert_eq!(fixup.get(&q0), Some(&0x5000), "Q seeded from its frame word");

    // Pause 3: P moves again (off the recycled address); Q stays.
    assert_eq!(
        fold_one_blocked_chain(&mut fixup, &mut snapshot, &map(&[(q0, 0x7000)])),
        (1, 0)
    );
    assert_eq!(addrs(&snapshot), vec![0x7000, 0x5000]);

    // The remount: each frame word lands on its own object.
    assert_eq!(wake(&fixup, p0), 0x7000, "the holder");
    assert_eq!(wake(&fixup, q0), 0x5000, "the other object");
    // And the snapshot the next pause marks from agrees with the frames.
    assert_eq!(
        addrs(&snapshot),
        vec![wake(&fixup, p0), wake(&fixup, q0)]
    );
}

#[test]
fn gci_i17hw_many_pauses_keep_the_frame_and_snapshot_views_equal() {
    // One holder moved by every pause; from the second pause on, the
    // OTHER frame object moves onto the address the holder vacated two
    // pauses earlier (recycled to-space) -- in pause 2 that is the
    // holder's own FRAME word, the chain key (the ABA shape). The frame
    // words and the snapshot never disagree, which is the property whose
    // failure the page's `weak=split` diagnostic would show.
    let p_at = |k: usize| 0x10_000usize + k * 0x100;
    let other0 = 0x90_000usize;
    let mut fixup = cratonvm_types::PointerMap::default();
    let mut snapshot = vec![obj(p_at(0)), obj(other0)];
    let mut other = other0;
    for k in 1..=20usize {
        let mut pairs = vec![(p_at(k - 1), p_at(k))];
        if k >= 2 {
            pairs.push((other, p_at(k - 2)));
        }
        let _ = fold_one_blocked_chain(&mut fixup, &mut snapshot, &map(&pairs));
        if k >= 2 {
            other = p_at(k - 2);
        }
        assert_eq!(addrs(&snapshot), vec![p_at(k), other], "pause {k}");
        assert_eq!(wake(&fixup, p_at(0)), p_at(k), "pause {k}: holder");
        assert_eq!(wake(&fixup, other0), other, "pause {k}: other");
    }
}
