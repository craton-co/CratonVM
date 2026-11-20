use super::*;

fn put_int(out: &mut Vec<u8>, v: u32) {
    let mut v = v;
    for _ in 0..3 {
        if v < 0x80 {
            out.push(v as u8);
            return;
        }
        out.push((v as u8 & 0x7F) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.push(4);
    put_int(out, s.len() as u32);
    for u in s.encode_utf16() {
        put_int(out, u32::from(u));
    }
}

#[test]
fn parses_a_small_descriptor() {
    // strings: 0 root 1 metadata 2 class 3 name 4 id 5 field 6 class 7 x 8 long 9 231 10 T 11 99 12 superType 13 jdk.jfr.Event
    let strings = [
        "root",
        "metadata",
        "class",
        "name",
        "id",
        "field",
        "x",
        "231",
        "T",
        "99",
        "superType",
        "jdk.jfr.Event",
    ];
    let mut blob = Vec::new();
    put_int(&mut blob, strings.len() as u32);
    for s in strings {
        put_str(&mut blob, s);
    }
    // root(1 child: metadata(1 child: class T id=99 super=Event, 1 field x : 231))
    put_int(&mut blob, 0); // root
    put_int(&mut blob, 0); // attrs
    put_int(&mut blob, 1); // children
    put_int(&mut blob, 1); // metadata
    put_int(&mut blob, 0);
    put_int(&mut blob, 1);
    put_int(&mut blob, 2); // class
    put_int(&mut blob, 3); // attrs: name=T, id=99, superType
    put_int(&mut blob, 3);
    put_int(&mut blob, 8);
    put_int(&mut blob, 4);
    put_int(&mut blob, 9);
    put_int(&mut blob, 10);
    put_int(&mut blob, 11);
    put_int(&mut blob, 1); // one field
    put_int(&mut blob, 5); // field
    put_int(&mut blob, 2); // attrs name=x class=231
    put_int(&mut blob, 3);
    put_int(&mut blob, 6);
    put_int(&mut blob, 2); // "class" attribute name is string 2
    put_int(&mut blob, 7);
    put_int(&mut blob, 0);
    let meta = Metadata::parse(&blob).expect("parses");
    let t = meta.by_name("T").expect("type T");
    assert_eq!(t.id, 99);
    assert!(t.is_event());
    assert_eq!(t.fields.len(), 1);
    assert_eq!(t.fields[0].name, "x");
    assert_eq!(t.fields[0].type_id, 231);
    assert!(meta.by_id(99).is_some());
}
