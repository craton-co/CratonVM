use super::*;
use cratonvm_classloading::{ClassLoaderId, DefineClassOptions, RedefineOptions};

/// A version-52 class `i20l1/CpStamp` whose one method `static value()I`
/// is `ldc #8; ireturn`, `#8` being `CONSTANT_Integer constant` (the
/// shape `interpreter::obsolete_frames`' tests use).
fn ldc_class(constant: i32) -> Vec<u8> {
    fn utf8(out: &mut Vec<u8>, s: &str) {
        out.push(1);
        out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    b.extend_from_slice(&9u16.to_be_bytes()); // constant_pool_count
    utf8(&mut b, "i20l1/CpStamp"); // #1
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

/// The word compiled code bakes (`cratonvm_jit::cp_holder_word`) for
/// code compiled at `stamp`.
fn word(cid: ClassId, stamp: u64) -> i64 {
    // Cast: the packed word as the helper ABI's i64 bit pattern.
    (u64::from(cid.as_u32()) | ((stamp + 1) << 32)) as i64
}

#[test]
fn a_site_compiled_before_its_class_was_redefined_reads_its_own_constant() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let cid = {
        let mut cm = shared.classes.class_manager.write();
        match cm.define_class_with_options(
            "i20l1/CpStamp",
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
    assert_eq!(cp_site_index(&shared, word(cid, compiled_at), 8), (cid, 8));
    assert_eq!(
        cp_site_index(&shared, i64::from(cid.as_u32()), 8),
        (cid, 8),
        "a word with no stamp reads its index as it is"
    );
    shared
        .classes
        .class_manager
        .write()
        .redefine_class(cid, ldc_class(80_000), RedefineOptions::default())
        .expect("a same-shape redefinition succeeds");

    let (holder, idx) = cp_site_index(&shared, word(cid, compiled_at), 8);
    assert_eq!(holder, cid);
    assert_ne!(idx, 8, "#8 is the new constant now");
    {
        let cm = shared.classes.class_manager.read();
        let pool = &cm.get_class(cid).expect("loaded").constant_pool;
        assert!(matches!(
            pool.get(idx),
            Some(cratonvm_reader::constant_pool::ConstantPoolEntry::Integer(
                70_000
            ))
        ));
        assert!(matches!(
            pool.get(8),
            Some(cratonvm_reader::constant_pool::ConstantPoolEntry::Integer(
                80_000
            ))
        ));
    }
    let fresh = crate::classloading::class_redefinition_count();
    assert_eq!(
        cp_site_index(&shared, word(cid, fresh), 8),
        (cid, 8),
        "code compiled after the redefinition indexes the current pool"
    );
}

/// Interpreter round i1 wave 21, lane L1
/// (`interpreter-L1-other-cp-indexed-jit-helpers-carry-no-cp-stamp-FIXED`): a
/// `multianewarray` site word carries a truncated stamp beside its cp
/// index, and `jit_multianewarray_n` translates the index the same way:
/// a site packed before the redefinition reads its own constant, one
/// packed after it or with no stamp reads `#8`.
#[test]
fn a_multianewarray_site_packed_before_a_redefinition_reads_its_own_constant() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let cid = {
        let mut cm = shared.classes.class_manager.write();
        // Its own VM, so the name the test above defines is free here.
        match cm.define_class_with_options(
            "i20l1/CpStamp",
            &ldc_class(70_000),
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        ) {
            Ok(cid) => cid,
            // The stripped test VM could not define it: nothing to run.
            Err(_) => return,
        }
    };
    let site_at = |stamp: u64| -> i64 {
        // Cast: the packed word as the helper ABI's i64 bit pattern.
        (u64::from(cid.as_u32())
            | (8_u64 << 32)
            | ((stamp % cratonvm_jit::MULTIANEWARRAY_STAMP_MODULUS + 1) << 48))
            as i64
    };
    let unstamped = (u64::from(cid.as_u32()) | (8_u64 << 32)) as i64;
    let compiled_at = crate::classloading::class_redefinition_count();
    let before = site_at(compiled_at);
    assert_eq!(
        cratonvm_jit::unpack_multianewarray_site(before),
        (cid.as_u32(), 8)
    );
    assert_eq!(multianewarray_site(&shared, before).1, 8);
    shared
        .classes
        .class_manager
        .write()
        .redefine_class(cid, ldc_class(80_000), RedefineOptions::default())
        .expect("a same-shape redefinition succeeds");

    let idx = multianewarray_site(&shared, before).1;
    assert_ne!(idx, 8, "#8 is the new constant now");
    {
        let cm = shared.classes.class_manager.read();
        let pool = &cm.get_class(cid).expect("loaded").constant_pool;
        assert!(matches!(
            pool.get(idx),
            Some(cratonvm_reader::constant_pool::ConstantPoolEntry::Integer(
                70_000
            ))
        ));
    }
    let fresh = crate::classloading::class_redefinition_count();
    assert_eq!(multianewarray_site(&shared, site_at(fresh)).1, 8);
    assert_eq!(
        multianewarray_site(&shared, unstamped).1,
        8,
        "a word with no stamp reads its index as it is"
    );
}

/// Interpreter round i1 wave 44, lane L2
/// (`interpreter-L2-multianewarray-and-type-check-sites-judge-their-index-before-the-pool-read-FIXED-20261008`):
/// a `multianewarray` site judged lock-free just before a redefinition
/// that renumbers its entry -- the redefinition landing between the
/// helper's count read and its pool read -- no longer trusts the probe:
/// the verdict fails, and the index judged under the guard the pool is
/// read under (`CompiledMultiANewArraySite::index_in`) names the site's
/// own constant, where the probe index names the new one.
#[test]
fn a_multianewarray_site_judged_before_a_redefinition_reads_its_constant_at_the_guarded_index()
{
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let cid = {
        let mut cm = shared.classes.class_manager.write();
        // Its own VM, so the name the tests above define is free here.
        match cm.define_class_with_options(
            "i20l1/CpStamp",
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
    // Cast: the packed word as the helper ABI's i64 bit pattern.
    let site = (u64::from(cid.as_u32())
        | (8_u64 << 32)
        | ((compiled_at % cratonvm_jit::MULTIANEWARRAY_STAMP_MODULUS + 1) << 48))
        as i64;
    let (cp_site, probe, judged_at) = multianewarray_site(&shared, site);
    assert_eq!(probe, 8, "no redefinition yet: the baked index");
    shared
        .classes
        .class_manager
        .write()
        .redefine_class(cid, ldc_class(80_000), RedefineOptions::default())
        .expect("a same-shape redefinition succeeds");
    assert!(
        !CpSite::verdict_still_holds(judged_at),
        "a probe judged before the redefinition is not kept"
    );
    let cm = shared.classes.class_manager.read();
    let guarded = cp_site.index_in(&cm);
    assert_ne!(guarded, probe, "#8 is the new constant now");
    let pool = &cm.get_class(cid).expect("loaded").constant_pool;
    assert!(matches!(
        pool.get(guarded),
        Some(cratonvm_reader::constant_pool::ConstantPoolEntry::Integer(
            70_000
        ))
    ));
    assert!(matches!(
        pool.get(probe),
        Some(cratonvm_reader::constant_pool::ConstantPoolEntry::Integer(
            80_000
        ))
    ));
}

/// `ldc_class` with `#8` a `CONSTANT_Class` naming `target` (`#9` its
/// name) and `value()Ljava/lang/Object;` returning it.
fn class_constant_class(target: &str) -> Vec<u8> {
    fn utf8(out: &mut Vec<u8>, s: &str) {
        out.push(1);
        out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    b.extend_from_slice(&10u16.to_be_bytes()); // constant_pool_count
    utf8(&mut b, "i20l1/CpStamp"); // #1
    b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
    utf8(&mut b, "java/lang/Object"); // #3
    b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
    utf8(&mut b, "value"); // #5
    utf8(&mut b, "()Ljava/lang/Object;"); // #6
    utf8(&mut b, "Code"); // #7
    b.extend_from_slice(&[7, 0, 9]); // #8 Class #9
    utf8(&mut b, target); // #9
    b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
    b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
    b.extend_from_slice(&[0, 0, 0, 0]); // interfaces, fields
    b.extend_from_slice(&[0, 1]); // methods_count
    b.extend_from_slice(&[0x00, 0x09, 0, 5, 0, 6, 0, 1]); // public static value()
    let code = [0x12, 8, 0xb0]; // ldc #8; areturn
    b.extend_from_slice(&[0, 7]); // "Code"
    b.extend_from_slice(&(12 + code.len() as u32).to_be_bytes());
    b.extend_from_slice(&[0, 1, 0, 0]); // max_stack 1, max_locals 0
    b.extend_from_slice(&(code.len() as u32).to_be_bytes());
    b.extend_from_slice(&code);
    b.extend_from_slice(&[0, 0, 0, 0]); // no handlers, no attributes
    b.extend_from_slice(&[0, 0]); // no class attributes
    b
}

/// Interpreter round i1 wave 21, lane L1: a holder-keyed type-check site
/// of code compiled before its holder's redefinition keys its resolution
/// record by the entry that still names its class; a translated entry
/// naming another class, a site compiled after the redefinition, and a
/// word with no stamp keep the index as it is.
#[test]
fn a_typecheck_site_compiled_before_a_redefinition_keys_its_own_class_entry() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let cid = {
        let mut cm = shared.classes.class_manager.write();
        match cm.define_class_with_options(
            "i20l1/CpStamp",
            &class_constant_class("i21l1/AaA"),
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        ) {
            Ok(cid) => cid,
            // The stripped test VM could not define it: nothing to run.
            Err(_) => return,
        }
    };
    let compiled_at = crate::classloading::class_redefinition_count();
    let before = u64::from(cid.as_u32()) | ((compiled_at + 1) << 32);
    // Judged under the guard, as the helper judges it (wave 44).
    let judge = |word: u64, name: &str| {
        typecheck_site_index_in(&shared.classes.class_manager.read(), word, 8, name)
    };
    assert_eq!(judge(before, "i21l1/AaA"), (cid, 8));
    if shared
        .classes
        .class_manager
        .write()
        .redefine_class(
            cid,
            class_constant_class("i21l1/BbB"),
            RedefineOptions::default(),
        )
        .is_err()
    {
        // A redefinition this stripped VM refuses: nothing to translate.
        return;
    }
    let (holder, idx) = judge(before, "i21l1/AaA");
    assert_eq!(holder, cid);
    assert_ne!(idx, 8, "#8 names the new class now");
    {
        let cm = shared.classes.class_manager.read();
        let pool = &cm.get_class(cid).expect("loaded").constant_pool;
        assert_eq!(pool.get_class_name(idx), Some("i21l1/AaA"));
        assert_eq!(pool.get_class_name(8), Some("i21l1/BbB"));
    }
    assert_eq!(
        judge(before, "i21l1/BbB"),
        (cid, 8),
        "a translation naming another class is not taken"
    );
    let fresh = crate::classloading::class_redefinition_count();
    let after = u64::from(cid.as_u32()) | ((fresh + 1) << 32);
    assert_eq!(judge(after, "i21l1/BbB"), (cid, 8));
    assert_eq!(
        judge(u64::from(cid.as_u32()), "i21l1/AaA"),
        (cid, 8),
        "a word with no stamp reads its index as it is"
    );
}
