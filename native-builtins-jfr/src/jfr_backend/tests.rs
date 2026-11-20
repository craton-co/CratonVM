use super::*;

#[test]
fn known_type_ids_are_the_hotspot_25_ones() {
    assert_eq!(known_type_id("long"), Some(231));
    assert_eq!(known_type_id("java.lang.String"), Some(239));
    assert_eq!(known_type_id("jdk.types.StackTrace"), Some(212));
    assert_eq!(known_type_id("com.example.Foo"), None);
}

fn metadata_bin(with_level: bool) -> Vec<u8> {
    fn utf(v: &mut Vec<u8>, s: &str) {
        v.extend_from_slice(&(s.len() as u16).to_be_bytes());
        v.extend_from_slice(s.as_bytes());
    }
    let mut v = Vec::new();
    v.extend_from_slice(&1i32.to_be_bytes()); // one type
    v.extend_from_slice(&0i32.to_be_bytes()); // no fields
    utf(&mut v, "jdk.types.X");
    for _ in 0..3 {
        utf(&mut v, ""); // label, description, category
    }
    v.extend_from_slice(&[0, 0, 0]); // thread, stackTrace, startTime
    utf(&mut v, ""); // period
    v.extend_from_slice(&[0, 0]); // cutoff, throttle
    if with_level {
        utf(&mut v, "");
    }
    v.extend_from_slice(&[0, 0]); // experimental, internal
    v.extend_from_slice(&200i64.to_be_bytes());
    v.extend_from_slice(&[0, 0]);
    v
}

#[test]
fn image_type_ids_parse_both_layouts() {
    for with_level in [false, true] {
        let ids = parse_image_type_ids(&metadata_bin(with_level)).expect("parses");
        assert_eq!(ids.get("jdk.types.X"), Some(&200));
    }
    assert!(parse_image_type_ids(&[0, 0, 0]).is_none());
}

#[test]
fn params_drops_a_receiver() {
    let args = [Value::Int(7), Value::Long(1)];
    assert_eq!(params(&args, 1).len(), 1);
    assert_eq!(params(&args, 2).len(), 2);
}
