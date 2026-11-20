use super::*;
use cratonvm_classloading::{ClassLoaderId, DefineClassOptions, RedefineOptions};

/// `i43l2/CpSite`: `static value()I` is `ldc #8; ireturn`, `#8` being
/// `CONSTANT_Integer constant` (the `i20_l1_cp_site_index_tests` shape).
fn ldc_class(constant: i32) -> Vec<u8> {
    fn utf8(out: &mut Vec<u8>, s: &str) {
        out.push(1);
        out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    b.extend_from_slice(&9u16.to_be_bytes()); // constant_pool_count
    utf8(&mut b, "i43l2/CpSite"); // #1
    b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
    utf8(&mut b, "java/lang/Object"); // #3
    b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
    utf8(&mut b, "value"); // #5
    utf8(&mut b, "()I"); // #6
    utf8(&mut b, "Code"); // #7
    b.push(3); // #8 Integer
    b.extend_from_slice(&constant.to_be_bytes());
    b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
    b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
    b.extend_from_slice(&[0, 0, 0, 0]); // interfaces, fields
    b.extend_from_slice(&[0, 1]); // methods_count
    b.extend_from_slice(&[0x00, 0x09, 0, 5, 0, 6, 0, 1]); // public static value()I
    let code = [0x12, 8, 0xac]; // ldc #8; ireturn
    b.extend_from_slice(&[0, 7]); // "Code"
    b.extend_from_slice(&(12 + code.len() as u32).to_be_bytes());
    b.extend_from_slice(&[0, 1, 0, 0]); // max_stack 1, max_locals 0
    b.extend_from_slice(&(code.len() as u32).to_be_bytes());
    b.extend_from_slice(&code);
    b.extend_from_slice(&[0, 0, 0, 0]); // no handlers, no attributes
    b.extend_from_slice(&[0, 0]); // no class attributes
    b
}

/// The word compiled code bakes for code compiled at `stamp`.
fn word(cid: ClassId, stamp: u64) -> i64 {
    // Cast: the packed word as the helper ABI's i64 bit pattern.
    (u64::from(cid.as_u32()) | ((stamp + 1) << 32)) as i64
}

#[test]
fn a_verdict_taken_before_a_redefinition_is_judged_again_under_the_guard() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let cid = {
        let mut cm = shared.classes.class_manager.write();
        match cm.define_class_with_options(
            "i43l2/CpSite",
            &ldc_class(70_000),
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        ) {
            Ok(cid) => cid,
            // The stripped test VM could not define it: nothing to run.
            Err(_) => return,
        }
    };
    let compiled_at = crate::classloading::class_redefinition_count();
    let site = CpSite::of(word(cid, compiled_at), 8);
    // The helper's lock-free verdict, taken before the redefinition
    // begins: the code is as new as the count, so nothing to translate.
    let (fast, judged_at) = site.index_lock_free(&shared);
    assert_eq!(fast, 8);
    shared
        .classes
        .class_manager
        .write()
        .redefine_class(cid, ldc_class(80_000), RedefineOptions::default())
        .expect("a same-shape redefinition succeeds");
    assert!(
        !CpSite::verdict_still_holds(judged_at),
        "a redefinition that began after the verdict voids it"
    );
    let cm = shared.classes.class_manager.read();
    let pool = &cm.get_class(cid).expect("loaded").constant_pool;
    let judged = site.index_in(&cm);
    assert_ne!(judged, 8, "#8 is the new constant now");
    assert!(matches!(
        pool.get(judged),
        Some(cratonvm_reader::constant_pool::ConstantPoolEntry::Integer(
            70_000
        ))
    ));
    let fresh = CpSite::of(
        word(cid, crate::classloading::class_redefinition_count()),
        8,
    );
    assert_eq!(fresh.index_in(&cm), 8, "code compiled after it reads #8");
    let unstamped = CpSite::of(i64::from(cid.as_u32()), 8);
    assert_eq!(
        unstamped.index_in(&cm),
        8,
        "a word with no stamp reads its index as it is"
    );
}
