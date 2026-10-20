// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The schema extension HotSpot's `JfrEventClassTransformer` applies to every
//! concrete subclass of `jdk.internal.event.Event` at class-definition time
//! (SBR5 lane `jfr`).
//!
//! `JVMUpcalls.bytesForEagerInstrumentation` / `onRetransform` (the Java half of
//! JFR's event instrumentation, `jdk.jfr.internal.EventInstrumentation`) only
//! rewrite the *bodies* of methods that already exist, and they refuse a class
//! that lacks the static `eventConfiguration` field ("Class missing
//! configuration field"). A JVMTI retransformation cannot add a field or a
//! method, so HotSpot adds the whole schema while it parses the class. Measured
//! on HotSpot 25 (`Sbr5JfrM`, `Sbr5JfrM2`): a concrete event class `Ev extends
//! jdk.jfr.Event` that declares only `int n; String s;` ends up with
//!
//! (all three fields carry ACC_SYNTHETIC, which `Utils.verifyMirror` relies on)
//! * `private static EventConfiguration eventConfiguration` (typed
//!   `jdk.jfr.internal.event.EventConfiguration` for classes of a non-boot
//!   loader, `Object` for a boot-loader class such as
//!   `jdk.internal.event.FileReadEvent`, which cannot name a `jdk.jfr` type),
//! * `private transient long startTime` and `private transient long duration`,
//! * `public void begin()`, `end()`, `commit()` and `public boolean
//!   isEnabled()`, `shouldCommit()`,
//!
//! each added only when the class does not already declare it. The method
//! bodies are the inert ones (`return` / `return false`); the Java upcall
//! replaces them. Abstract classes are left alone.
//!
//! This module does the byte-level half: it appends exactly those members to a
//! class file. It is used twice: by `ClassManager` at definition (so the live
//! class has the members and `class_bytes` caches the extended file) and by
//! nothing else — a retransformation starts from the cached, already extended
//! bytes.

/// Constant-pool tags this walker needs sizes for (JVMS 4.4).
fn cp_entry_size(tag: u8, bytes: &[u8], at: usize) -> Option<(usize, usize)> {
    // returns (total entry size including the tag byte, number of cp slots used)
    match tag {
        1 => {
            let len = u16::from_be_bytes([*bytes.get(at + 1)?, *bytes.get(at + 2)?]) as usize;
            Some((3 + len, 1))
        }
        3 | 4 => Some((5, 1)),
        5 | 6 => Some((9, 2)),
        7 | 8 | 16 | 19 | 20 => Some((3, 1)),
        9 | 10 | 11 | 12 | 17 | 18 => Some((5, 1)),
        15 => Some((4, 1)),
        _ => None,
    }
}

fn be16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*bytes.get(at)?, *bytes.get(at + 1)?]))
}

fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *bytes.get(at)?,
        *bytes.get(at + 1)?,
        *bytes.get(at + 2)?,
        *bytes.get(at + 3)?,
    ]))
}

/// Skip the `attributes` table starting at `at` (count then entries); returns
/// the offset just past it.
fn skip_attributes(bytes: &[u8], at: usize) -> Option<usize> {
    let count = be16(bytes, at)? as usize;
    let mut p = at + 2;
    for _ in 0..count {
        let len = be32(bytes, p + 2)? as usize;
        p = p.checked_add(6)?.checked_add(len)?;
        if p > bytes.len() {
            return None;
        }
    }
    Some(p)
}

fn utf8_entry(s: &str, out: &mut Vec<u8>) {
    out.push(1);
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// Append the `jdk.internal.event.Event` subclass schema to `bytes`.
///
/// `untyped` selects `Object` for the type of the `eventConfiguration` field
/// (boot-loader classes). Returns `None` when the file cannot be walked or
/// nothing needs adding; the caller then keeps the original bytes.
pub fn extend_event_class(bytes: &[u8], untyped: bool) -> Option<Vec<u8>> {
    if bytes.len() < 10 || bytes[0..4] != [0xCA, 0xFE, 0xBA, 0xBE] {
        return None;
    }
    let cp_count = be16(bytes, 8)? as usize;
    // Constant pool walk, remembering the Utf8 value of each slot we may need.
    let mut utf8_ranges: Vec<Option<(usize, usize)>> = vec![None; cp_count];
    let mut p = 10usize;
    let mut idx = 1usize;
    while idx < cp_count {
        let tag = *bytes.get(p)?;
        let (size, slots) = cp_entry_size(tag, bytes, p)?;
        if tag == 1 {
            utf8_ranges[idx] = Some((p + 3, p + size));
        }
        p = p.checked_add(size)?;
        idx += slots;
    }
    let cp_end = p;
    fn utf8_slice<'a>(
        bytes: &'a [u8],
        ranges: &[Option<(usize, usize)>],
        i: u16,
    ) -> Option<&'a [u8]> {
        let (a, b) = (*ranges.get(i as usize)?)?;
        bytes.get(a..b)
    }
    // access_flags, this_class, super_class, interfaces
    let access = be16(bytes, cp_end)?;
    if access & 0x0200 != 0 || access & 0x0400 != 0 {
        // interface or abstract: HotSpot leaves these alone
        return None;
    }
    let interfaces_count = be16(bytes, cp_end + 6)? as usize;
    let fields_count_off = cp_end + 8 + 2 * interfaces_count;
    let fields_count = be16(bytes, fields_count_off)? as usize;
    let mut q = fields_count_off + 2;
    let mut has_config = false;
    let mut has_start = false;
    let mut has_duration = false;
    for _ in 0..fields_count {
        let name = utf8_slice(bytes, &utf8_ranges, be16(bytes, q + 2)?)?;
        match name {
            b"eventConfiguration" => has_config = true,
            b"startTime" => has_start = true,
            b"duration" => has_duration = true,
            _ => {}
        }
        q = skip_attributes(bytes, q + 6)?;
    }
    let fields_end = q;
    let methods_count_off = fields_end;
    let methods_count = be16(bytes, methods_count_off)? as usize;
    let mut q = methods_count_off + 2;
    // (name, descriptor) presence of the five members
    let mut has = [false; 5];
    const MEMBERS: [(&str, &str); 5] = [
        ("begin", "()V"),
        ("end", "()V"),
        ("commit", "()V"),
        ("isEnabled", "()Z"),
        ("shouldCommit", "()Z"),
    ];
    for _ in 0..methods_count {
        let name = utf8_slice(bytes, &utf8_ranges, be16(bytes, q + 2)?)?;
        let desc = utf8_slice(bytes, &utf8_ranges, be16(bytes, q + 4)?)?;
        for (i, (n, d)) in MEMBERS.iter().enumerate() {
            if name == n.as_bytes() && desc == d.as_bytes() {
                has[i] = true;
            }
        }
        q = skip_attributes(bytes, q + 6)?;
    }
    let methods_end = q;
    if has_config && has_start && has_duration && has.iter().all(|h| *h) {
        return None;
    }

    // New constant-pool entries and the members that reference them.
    let mut new_cp: Vec<u8> = Vec::new();
    let mut next_index = cp_count;
    fn add_utf8(s: &str, new_cp: &mut Vec<u8>, next_index: &mut usize) -> u16 {
        utf8_entry(s, new_cp);
        let i = *next_index;
        *next_index += 1;
        i as u16
    }
    let mut new_fields: Vec<u8> = Vec::new();
    let mut new_field_count = 0u16;
    let mut push_field =
        |flags: u16, name: &str, desc: &str, new_cp: &mut Vec<u8>, ni: &mut usize| {
            let n = add_utf8(name, new_cp, ni);
            let d = add_utf8(desc, new_cp, ni);
            new_fields.extend_from_slice(&flags.to_be_bytes());
            new_fields.extend_from_slice(&n.to_be_bytes());
            new_fields.extend_from_slice(&d.to_be_bytes());
            new_fields.extend_from_slice(&0u16.to_be_bytes());
            new_field_count += 1;
        };
    if !has_config {
        let desc = if untyped {
            "Ljava/lang/Object;"
        } else {
            "Ljdk/jfr/internal/event/EventConfiguration;"
        };
        push_field(
            0x100A,
            "eventConfiguration",
            desc,
            &mut new_cp,
            &mut next_index,
        );
    }
    if !has_start {
        push_field(0x1082, "startTime", "J", &mut new_cp, &mut next_index);
    }
    if !has_duration {
        push_field(0x1082, "duration", "J", &mut new_cp, &mut next_index);
    }
    let mut new_methods: Vec<u8> = Vec::new();
    let mut new_method_count = 0u16;
    let code_name = if has.iter().all(|h| *h) {
        0
    } else {
        add_utf8("Code", &mut new_cp, &mut next_index)
    };
    for (i, (name, desc)) in MEMBERS.iter().enumerate() {
        if has[i] {
            continue;
        }
        let n = add_utf8(name, &mut new_cp, &mut next_index);
        let d = add_utf8(desc, &mut new_cp, &mut next_index);
        let (code, max_stack): (&[u8], u16) = if desc.ends_with('Z') {
            (&[0x03, 0xAC], 1) // iconst_0; ireturn
        } else {
            (&[0xB1], 0) // return
        };
        new_methods.extend_from_slice(&0x0001u16.to_be_bytes());
        new_methods.extend_from_slice(&n.to_be_bytes());
        new_methods.extend_from_slice(&d.to_be_bytes());
        new_methods.extend_from_slice(&1u16.to_be_bytes());
        new_methods.extend_from_slice(&code_name.to_be_bytes());
        let attr_len = 2 + 2 + 4 + code.len() + 2 + 2;
        new_methods.extend_from_slice(&(attr_len as u32).to_be_bytes());
        new_methods.extend_from_slice(&max_stack.to_be_bytes());
        new_methods.extend_from_slice(&1u16.to_be_bytes());
        new_methods.extend_from_slice(&(code.len() as u32).to_be_bytes());
        new_methods.extend_from_slice(code);
        new_methods.extend_from_slice(&0u16.to_be_bytes());
        new_methods.extend_from_slice(&0u16.to_be_bytes());
        new_method_count += 1;
    }
    if next_index > 0xFFFF
        || fields_count + new_field_count as usize > 0xFFFF
        || methods_count + new_method_count as usize > 0xFFFF
    {
        return None;
    }

    let mut out: Vec<u8> =
        Vec::with_capacity(bytes.len() + new_cp.len() + new_fields.len() + new_methods.len());
    out.extend_from_slice(&bytes[..8]);
    out.extend_from_slice(&(next_index as u16).to_be_bytes());
    out.extend_from_slice(&bytes[10..cp_end]);
    out.extend_from_slice(&new_cp);
    // access .. fields_count
    out.extend_from_slice(&bytes[cp_end..fields_count_off]);
    out.extend_from_slice(&((fields_count as u16) + new_field_count).to_be_bytes());
    out.extend_from_slice(&bytes[fields_count_off + 2..fields_end]);
    out.extend_from_slice(&new_fields);
    out.extend_from_slice(&((methods_count as u16) + new_method_count).to_be_bytes());
    out.extend_from_slice(&bytes[methods_count_off + 2..methods_end]);
    out.extend_from_slice(&new_methods);
    out.extend_from_slice(&bytes[methods_end..]);
    Some(out)
}

#[cfg(test)]
mod tests {
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
}
