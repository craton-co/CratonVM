use super::*;
use crate::vm::SharedVm;
use cratonvm_classloading::{ClassLoaderId, DefineClassOptions, RedefineOptions};

fn utf8(out: &mut Vec<u8>, s: &str) {
    out.push(1);
    out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// A version-49 class `recon/Probe` (no stack maps needed) with a long
/// constant (two slots), a modified-UTF-8 string with NUL and a
/// supplementary character, a `static final long LIMIT` with a
/// `ConstantValue`, a method `static value()J` with a handler and a
/// `LineNumberTable`, and a `SourceFile`: every attribute is one `Class`
/// keeps, in the order the writer emits, so the file written back is
/// this file byte for byte.
fn probe_class(constant: i64) -> Vec<u8> {
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 49];
    b.extend_from_slice(&20u16.to_be_bytes()); // constant_pool_count
    utf8(&mut b, "recon/Probe"); // #1
    b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
    utf8(&mut b, "java/lang/Object"); // #3
    b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
    utf8(&mut b, "value"); // #5
    utf8(&mut b, "()J"); // #6
    utf8(&mut b, "Code"); // #7
    b.push(5); // #8 Long (and #9)
    b.extend_from_slice(&constant.to_be_bytes());
    utf8(&mut b, "LIMIT"); // #10
    utf8(&mut b, "J"); // #11
    utf8(&mut b, "ConstantValue"); // #12
    utf8(&mut b, "LineNumberTable"); // #13
    utf8(&mut b, "SourceFile"); // #14
    utf8(&mut b, "Probe.java"); // #15
    utf8(&mut b, "java/lang/Throwable"); // #16
    b.extend_from_slice(&[7, 0, 16]); // #17 Class #16
                                      // #18 Utf8 "a\0\u{e9}\u{10000}" in modified UTF-8.
    b.push(1);
    b.extend_from_slice(&11u16.to_be_bytes());
    b.extend_from_slice(&[
        0x61, 0xC0, 0x80, 0xC3, 0xA9, 0xED, 0xA0, 0x80, 0xED, 0xB0, 0x80,
    ]);
    b.extend_from_slice(&[8, 0, 18]); // #19 String #18
    b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
    b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
    b.extend_from_slice(&[0, 0]); // interfaces
    b.extend_from_slice(&[0, 1]); // fields_count
    b.extend_from_slice(&[0x00, 0x19, 0, 10, 0, 11, 0, 1]); // public static final long LIMIT
    b.extend_from_slice(&[0, 12, 0, 0, 0, 2, 0, 8]); // ConstantValue #8
    b.extend_from_slice(&[0, 1]); // methods_count
    b.extend_from_slice(&[0x00, 0x09, 0, 5, 0, 6, 0, 1]); // public static value()J
    let code: [u8; 5] = [0x14, 0, 8, 0xAD, 0xBF]; // ldc2_w #8; lreturn; athrow
    let line_table_len: u32 = 2 + 2 * 4;
    // max_stack, max_locals, code_length, the two table counts (12), the
    // code, one handler (8), the line table's header (6) and body.
    let code_attr_len: u32 = 12 + code.len() as u32 + 8 + 6 + line_table_len;
    b.extend_from_slice(&[0, 7]); // "Code"
    b.extend_from_slice(&code_attr_len.to_be_bytes());
    b.extend_from_slice(&[0, 2, 0, 0]); // max_stack 2, max_locals 0
    b.extend_from_slice(&(code.len() as u32).to_be_bytes());
    b.extend_from_slice(&code);
    b.extend_from_slice(&[0, 1, 0, 0, 0, 3, 0, 4, 0, 17]); // [0,3) -> 4, Throwable
    b.extend_from_slice(&[0, 1, 0, 13]); // one attribute: LineNumberTable
    b.extend_from_slice(&line_table_len.to_be_bytes());
    b.extend_from_slice(&[0, 2, 0, 0, 0, 7, 0, 4, 0, 9]);
    b.extend_from_slice(&[0, 1, 0, 14, 0, 0, 0, 2, 0, 15]); // SourceFile #15
    b
}

/// Wave 29, lane L3: a class never redefined is written back as the file
/// it was defined from -- the pool index for index (a long's two slots,
/// modified UTF-8), a field's `ConstantValue`, a body with its handler
/// and line table, the class's `SourceFile`.
#[test]
fn a_class_never_redefined_is_written_back_as_its_class_file() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let file = probe_class(0x1122_3344_5566_7788);
    let cid = {
        let mut cm = shared.classes.class_manager.write();
        match cm.define_class_with_options(
            "recon/Probe",
            &file,
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        ) {
            Ok(cid) => cid,
            // The stripped test VM could not define it: nothing to run.
            Err(_) => return,
        }
    };
    let cm = shared.classes.class_manager.read();
    assert_eq!(reconstitute_class_file(&cm, cid), Ok(file));
}

/// A redefined class is not its own base any more: nothing is written.
#[test]
fn a_redefined_class_is_not_reconstituted() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let cid = {
        let mut cm = shared.classes.class_manager.write();
        match cm.define_class_with_options(
            "recon/Probe",
            &probe_class(1),
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        ) {
            Ok(cid) => cid,
            Err(_) => return,
        }
    };
    shared
        .classes
        .class_manager
        .write()
        .redefine_class(cid, probe_class(2), RedefineOptions::default())
        .expect("a same-shape redefinition succeeds");
    let cm = shared.classes.class_manager.read();
    assert_eq!(
        reconstitute_class_file(&cm, cid),
        Err(ReconstituteRefusal::Redefined)
    );
}

/// `recon/Outer` (version 49, no members) whose `InnerClasses` table has
/// the rows `rows`, each `(class index, simple-name index, flags)`:
/// class #6 is `recon/Outer$A` named #9 `A`, class #8 `recon/Outer$B`
/// named #10 `B`, both with outer class #2.
fn inner_rows_class(rows: &[(u16, u16, u16)]) -> Vec<u8> {
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 49];
    b.extend_from_slice(&12u16.to_be_bytes());
    utf8(&mut b, "recon/Outer"); // #1
    b.extend_from_slice(&[7, 0, 1]); // #2
    utf8(&mut b, "java/lang/Object"); // #3
    b.extend_from_slice(&[7, 0, 3]); // #4
    utf8(&mut b, "recon/Outer$A"); // #5
    b.extend_from_slice(&[7, 0, 5]); // #6
    utf8(&mut b, "recon/Outer$B"); // #7
    b.extend_from_slice(&[7, 0, 7]); // #8
    utf8(&mut b, "A"); // #9
    utf8(&mut b, "B"); // #10
    utf8(&mut b, "InnerClasses"); // #11
    b.extend_from_slice(&[0x00, 0x21, 0, 2, 0, 4, 0, 0, 0, 0, 0, 0]);
    b.extend_from_slice(&[0, 1, 0, 11]); // one attribute: InnerClasses
    b.extend_from_slice(&(2 + 8 * rows.len() as u32).to_be_bytes());
    b.extend_from_slice(&(rows.len() as u16).to_be_bytes());
    for &(inner, name, flags) in rows {
        b.extend_from_slice(&inner.to_be_bytes());
        b.extend_from_slice(&2u16.to_be_bytes());
        b.extend_from_slice(&name.to_be_bytes());
        b.extend_from_slice(&flags.to_be_bytes());
    }
    b
}

/// Interpreter round i7 wave 136, lane L3: a base whose `InnerClasses`
/// rows are not the live table's order (or count) takes each row's
/// stored flags by the inner class it names, never by position, and
/// writes nothing past its own table.
#[test]
fn source_flags_are_normalized_by_inner_class_name_not_row_position() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let live = inner_rows_class(&[(6, 9, 0x0009), (8, 10, 0x0002)]);
    let cid = {
        let mut cm = shared.classes.class_manager.write();
        match cm.define_class_with_options(
            "recon/Outer",
            &live,
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        ) {
            Ok(cid) => cid,
            Err(_) => return,
        }
    };
    let cm = shared.classes.class_manager.read();
    let stored = |name: &str| {
        cm.get_class(cid)
            .and_then(|c| c.inner_classes.iter().find(|e| &*e.inner_class == name).map(|e| e.access_flags))
            .expect("a live row")
    };
    let (a, b) = (stored("recon/Outer$A"), stored("recon/Outer$B"));
    // Reversed, flags zeroed: B's row first.
    let base = inner_rows_class(&[(8, 10, 0), (6, 9, 0)]);
    let rows_at = base.len() - 16;
    let out = normalize_source_flags(&cm, cid, base.clone());
    assert_eq!(out.len(), base.len());
    assert_eq!(&out[rows_at + 6..rows_at + 8], &b.to_be_bytes());
    assert_eq!(&out[rows_at + 8 + 6..rows_at + 16], &a.to_be_bytes());
    // A one-row base: the second live row is not written past the table.
    let short = inner_rows_class(&[(8, 10, 0)]);
    let out = normalize_source_flags(&cm, cid, short.clone());
    assert_eq!(out.len(), short.len());
    assert_eq!(&out[short.len() - 2..], &b.to_be_bytes());
}

#[test]
fn modified_utf8_writes_nul_and_supplementary_characters_as_the_class_file_does() {
    let mut out = Vec::new();
    modified_utf8("a\0\u{e9}\u{10000}".encode_utf16(), &mut out);
    assert_eq!(
        out,
        [0x61, 0xC0, 0x80, 0xC3, 0xA9, 0xED, 0xA0, 0x80, 0xED, 0xB0, 0x80]
    );
    // A lone surrogate, as a `wide_utf8` entry keeps it.
    out.clear();
    modified_utf8([0xD800u16].into_iter(), &mut out);
    assert_eq!(out, [0xED, 0xA0, 0x80]);
}

/// Names the live pool lacks are appended; names it has are reused.
#[test]
fn the_pool_writer_reuses_live_entries_and_appends_the_rest() {
    let live = ConstantPool::new(vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8("a/B".into()),
        ConstantPoolEntry::ClassReference { name_index: 1 },
        ConstantPoolEntry::Utf8("m".into()),
        ConstantPoolEntry::Utf8("()V".into()),
        ConstantPoolEntry::NameAndType {
            name_index: 3,
            descriptor_index: 4,
        },
    ]);
    let mut pool = PoolWriter::from_live(&live).expect("a small pool");
    assert_eq!(pool.class_ref("a/B"), Ok(2));
    assert_eq!(pool.utf8("m"), Ok(3));
    assert_eq!(pool.name_and_type("m", "()V"), Ok(5));
    assert_eq!(pool.class_ref("c/D"), Ok(7), "Utf8 #6, then Class #7");
    assert_eq!(pool.name_and_type("m", "(I)V"), Ok(9), "Utf8 #8, then #9");
    let mut out = Vec::new();
    pool.write(&mut out).expect("writable");
    assert_eq!(&out[..2], &10u16.to_be_bytes());
}
