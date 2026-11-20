use super::*;

#[test]
fn varint_matches_the_readers_cap() {
    let mut b = Vec::new();
    put_varint(&mut b, 0);
    assert_eq!(b, [0]);
    let mut b = Vec::new();
    put_varint(&mut b, 128);
    assert_eq!(b, [0x80, 0x01]);
    let mut b = Vec::new();
    put_varint(&mut b, u64::MAX);
    assert_eq!(b.len(), 9);
    assert_eq!(b[8], 0xFF);
    assert_eq!(varint_len(u64::MAX), 9);
    assert_eq!(varint_len(127), 1);
    assert_eq!(varint_len(128), 2);
}

#[test]
fn framing_is_self_inclusive() {
    let body = vec![7u8; 126];
    let framed = frame_event(&body);
    // 126 + 1 = 127 fits one byte
    assert_eq!(framed.len(), 127);
    assert_eq!(framed[0], 127);
    let body = vec![7u8; 127];
    let framed = frame_event(&body);
    // 127 + 1 = 128 needs two bytes, so the size is 129
    assert_eq!(framed.len(), 129);
    assert_eq!(framed[0], 0x81);
    assert_eq!(framed[1], 0x01);
}

#[test]
fn trim_cuts_on_event_boundaries() {
    let mut c = ChunkWriter::new(0, 0);
    for i in 0..10u8 {
        c.append_events(&frame_event(&[i; 9]));
    }
    assert_eq!(c.pending_len(), 100);
    c.trim_pending(35);
    assert_eq!(c.pending_len() % 10, 0);
    assert!(c.pending_len() <= 40);
}

#[test]
fn symbols_are_interned() {
    let mut t = PoolTables::default();
    let a = t.symbol("java/lang/String");
    let b = t.symbol("x");
    assert_ne!(a, b);
    assert_eq!(a, t.symbol("java/lang/String"));
}
