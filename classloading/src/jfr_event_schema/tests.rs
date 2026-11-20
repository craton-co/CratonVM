use super::*;

/// A hand-built class file: `class E extends Object { int n; <init>()V
/// begin()V }`, enough to exercise the field and method splice points.
fn tiny_class() -> Vec<u8> {
    let mut b: Vec<u8> = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    // cp: 1 Utf8 "E", 2 Class #1, 3 Utf8 "java/lang/Object", 4 Class #3,
    //     5 Utf8 "n", 6 Utf8 "I", 7 Utf8 "begin", 8 Utf8 "()V", 9 Utf8 "Code"
    let mut cp: Vec<u8> = Vec::new();
    utf8_entry("E", &mut cp);
    cp.extend_from_slice(&[7, 0, 1]);
    utf8_entry("java/lang/Object", &mut cp);
    cp.extend_from_slice(&[7, 0, 3]);
    utf8_entry("n", &mut cp);
    utf8_entry("I", &mut cp);
    utf8_entry("begin", &mut cp);
    utf8_entry("()V", &mut cp);
    utf8_entry("Code", &mut cp);
    b.extend_from_slice(&10u16.to_be_bytes());
    b.extend_from_slice(&cp);
    b.extend_from_slice(&[0x00, 0x21, 0, 2, 0, 4, 0, 0]); // access, this, super, 0 interfaces
    b.extend_from_slice(&[0, 1, 0, 0, 0, 5, 0, 6, 0, 0]); // 1 field n:I
                                                          // 1 method begin()V with Code { return }
    b.extend_from_slice(&[0, 1]);
    b.extend_from_slice(&[0, 1, 0, 7, 0, 8, 0, 1, 0, 9]);
    let code = [0xB1u8];
    b.extend_from_slice(&((2 + 2 + 4 + code.len() + 2 + 2) as u32).to_be_bytes());
    b.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 1]);
    b.extend_from_slice(&code);
    b.extend_from_slice(&[0, 0, 0, 0]);
    b.extend_from_slice(&[0, 0]); // class attributes
    b
}

#[test]
fn extends_a_concrete_class_once() {
    let original = tiny_class();
    let extended = extend_event_class(&original, false).expect("extends");
    assert!(extended.len() > original.len());
    // The extension is idempotent: the second pass finds every member.
    assert!(extend_event_class(&extended, false).is_none());
    let text = String::from_utf8_lossy(&extended).to_string();
    assert!(text.contains("eventConfiguration"));
    assert!(text.contains("Ljdk/jfr/internal/event/EventConfiguration;"));
    assert!(text.contains("startTime") && text.contains("shouldCommit"));
    let untyped = extend_event_class(&original, true).expect("extends");
    assert!(String::from_utf8_lossy(&untyped).contains("Ljava/lang/Object;"));
}

#[test]
fn leaves_abstract_classes_alone() {
    let mut b = tiny_class();
    // flip ACC_ABSTRACT into the class access flags
    let cp_end = {
        let mut p = 10usize;
        let mut idx = 1usize;
        while idx < 10 {
            let (size, slots) = cp_entry_size(b[p], &b, p).unwrap();
            p += size;
            idx += slots;
        }
        p
    };
    b[cp_end] |= 0x04;
    assert!(extend_event_class(&b, false).is_none());
}
