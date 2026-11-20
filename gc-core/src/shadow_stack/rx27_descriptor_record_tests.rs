use super::*;

/// A shadow stack holding `[value, record(rbp; homes), value]`, the frame
/// a local array whose words are the homes. Returns (stack, frame words,
/// rbp).
fn fixture(frame: &mut [usize; 8], homes: &[usize]) -> (ShadowStack, usize) {
    let mut s = ShadowStack::empty();
    s.ensure_allocated();
    // rbp = one past the last frame word; home k is word `8 - v`.
    let rbp = frame.as_mut_ptr() as usize + 8 * 8;
    assert!(s.push(0x10_0000));
    assert!(s.push(rbp | DESCRIPTOR_TAG));
    assert!(s.push(descriptor_header(homes.len()) as usize));
    let mut packed = 0u64;
    for (k, &v) in homes.iter().enumerate() {
        packed |= (v as u64) << (16 * (k % 4));
        if k % 4 == 3 || k + 1 == homes.len() {
            assert!(s.push(packed as usize));
            packed = 0;
        }
    }
    assert!(s.push(0x20_0000));
    (s, rbp)
}

fn band_of(frame: &[usize; 8]) -> SlotBand {
    let lo = frame.as_ptr() as usize;
    SlotBand::new(lo, lo + 8 * 8)
}

#[test]
fn rx27_walk_window_expands_a_record_between_values() {
    let mut frame = [0usize; 8];
    frame[7] = 0xA0_0000; // rbp - 8
    frame[2] = 0xB0_0000; // rbp - 48
    frame[0] = 0xC0_0000; // rbp - 64
    let (s, rbp) = fixture(&mut frame, &[1, 6, 8, 2, 3]);
    let band = band_of(&frame);
    let mut seen = Vec::new();
    unsafe { walk_window(s.base, s.top, Some(band), |h| seen.push(h)) };
    assert_eq!(seen.len(), 2 + 5, "two values and five homes, no header word");
    assert_eq!(seen[1], EntryHome::Frame(rbp - 8));
    assert_eq!(seen[2], EntryHome::Frame(rbp - 48));
    assert_eq!(seen[3], EntryHome::Frame(rbp - 64));
    assert!(matches!(seen[0], EntryHome::Shadow(_)));
    assert!(matches!(seen[6], EntryHome::Shadow(_)));
    // No band: every home is unresolved, the values are still read.
    let mut bandless = Vec::new();
    unsafe { walk_window(s.base, s.top, None, |h| bandless.push(h)) };
    assert_eq!(bandless.iter().filter(|h| **h == EntryHome::Unresolved).count(), 5);
}

/// `for_each_value_in` reads the homes' words, never the record's words.
#[test]
fn rx27_for_each_value_reads_every_descriptor_home() {
    let mut frame = [0usize; 8];
    frame[7] = 0xA0_0000;
    frame[2] = 0xB0_0000;
    let (s, _) = fixture(&mut frame, &[1, 6]);
    let mut vals = Vec::new();
    s.for_each_value_in(band_of(&frame), |v| vals.push(v));
    assert_eq!(vals, vec![0x10_0000, 0xA0_0000, 0xB0_0000, 0x20_0000]);
}

/// `indirect_slots_in` names every descriptor home (the frame walk skips
/// exactly these words).
#[test]
fn rx27_indirect_slots_name_every_descriptor_home() {
    let mut frame = [0usize; 8];
    let (s, rbp) = fixture(&mut frame, &[1, 6, 1]);
    let slots = s.indirect_slots_in(band_of(&frame));
    assert_eq!(slots, vec![rbp - 48, rbp - 8], "sorted, deduplicated");
}

/// `remap_in` rewrites every descriptor home once, even when named twice.
#[test]
fn rx27_remap_rewrites_descriptor_homes_once() {
    let mut frame = [0usize; 8];
    frame[7] = 0xA0_0000;
    frame[2] = 0xB0_0000;
    let (s, _) = fixture(&mut frame, &[1, 6, 1]);
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0xA0_0000, 0xB0_0000);
    map.insert(0xB0_0000, 0xD0_0000);
    let n = s.remap_in(&map, band_of(&frame));
    assert_eq!(frame[7], 0xB0_0000, "rbp-8 rewritten once, not chained twice");
    assert_eq!(frame[2], 0xD0_0000);
    assert_eq!(n, 2);
}

/// A home outside the band, a bad magic, and a record running past `top`
/// are not read as records.
#[test]
fn rx27_a_malformed_record_is_not_expanded() {
    assert_eq!(descriptor_record_words(descriptor_header(5) as usize), Some(4));
    assert_eq!(descriptor_record_words(5), None);
    assert_eq!(descriptor_record_words(descriptor_header(DESCRIPTOR_MAX_HOMES + 1) as usize), None);
    let mut frame = [0usize; 8];
    let (s, _) = fixture(&mut frame, &[9]); // rbp - 72: below the frame
    let mut seen = Vec::new();
    unsafe { walk_window(s.base, s.top, Some(band_of(&frame)), |h| seen.push(h)) };
    assert_eq!(seen[1], EntryHome::Unresolved);
    // Cut the window inside the record: the first word reads as a value.
    let mut cut = Vec::new();
    unsafe { walk_window(s.base, s.base + 16, Some(band_of(&frame)), |h| cut.push(h)) };
    assert_eq!(cut.len(), 2);
    assert!(matches!(cut[1], EntryHome::Shadow(_)));
}

/// The record words after `w0` for `homes`: header, then packed offsets.
fn record(homes: &[usize]) -> Vec<u64> {
    let mut out = vec![descriptor_header(homes.len())];
    for chunk in homes.chunks(4) {
        out.push(chunk.iter().enumerate().fold(0, |p, (k, &v)| p | (v as u64) << (16 * k)));
    }
    out
}

/// An interned record is `[rbp | 2, address]`: the same homes as the
/// inline record, from two shadow words.
#[test]
fn an_interned_record_names_the_inline_records_homes() {
    let mut frame = [0usize; 8];
    frame[7] = 0xA0_0000;
    frame[2] = 0xB0_0000;
    frame[0] = 0xC0_0000;
    let homes = [1, 6, 8, 2, 3];
    let addr = intern_descriptor(&record(&homes)).expect("table has room");
    assert_eq!(intern_descriptor(&record(&homes)), Some(addr), "deduplicated");
    let rbp = frame.as_mut_ptr() as usize + 8 * 8;
    let mut s = ShadowStack::empty();
    s.ensure_allocated();
    assert!(s.push(0x10_0000));
    assert!(s.push(rbp | DESCRIPTOR_TAG));
    assert!(s.push(addr));
    assert!(s.push(0x20_0000));
    let mut seen = Vec::new();
    unsafe { walk_window(s.base, s.top, Some(band_of(&frame)), |h| seen.push(h)) };
    assert_eq!(seen.len(), 2 + 5, "two values and five homes, no address word");
    assert_eq!(seen[1], EntryHome::Frame(rbp - 8));
    assert_eq!(seen[2], EntryHome::Frame(rbp - 48));
    assert_eq!(seen[3], EntryHome::Frame(rbp - 64));
    assert!(matches!(seen[6], EntryHome::Shadow(_)));
    let mut vals = Vec::new();
    s.for_each_value_in(band_of(&frame), |v| vals.push(v));
    assert_eq!(vals, vec![0x10_0000, 0xA0_0000, 0xB0_0000, 0xC0_0000, 0, 0, 0x20_0000]);
}

/// A word after a tagged `w0` that is neither a header nor an address in
/// the table is never dereferenced: the old one-word reading applies.
#[test]
fn an_address_outside_the_table_is_not_a_record() {
    let mut frame = [0usize; 8];
    let rbp = frame.as_mut_ptr() as usize + 8 * 8;
    let bogus = frame.as_ptr() as usize; // readable, 8-aligned, not interned
    let mut s = ShadowStack::empty();
    s.ensure_allocated();
    assert!(s.push(rbp | DESCRIPTOR_TAG));
    assert!(s.push(bogus));
    let mut seen = Vec::new();
    unsafe { walk_window(s.base, s.top, Some(band_of(&frame)), |h| seen.push(h)) };
    assert_eq!(seen.len(), 2);
    assert!(seen.iter().all(|h| matches!(h, EntryHome::Shadow(_))));
    assert_eq!(intern_descriptor(&[5]), None, "not a record");
    assert_eq!(intern_descriptor(&record(&[1, 2])[..1]), None, "truncated record");
}
