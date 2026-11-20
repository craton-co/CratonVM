use super::*;
use crate::config::VmConfig;

/// The first word decodes to the context and no site when untagged (every
/// unjudged bind), and to the record's `SharedVm` and its own info when
/// tagged; the cold arm then dispatches with that info, not the stand-in.
#[test]
pub fn the_tagged_word_is_the_site_and_the_plain_word_is_the_vm() {
    let record = cratonvm_jit::VarHandleJudgedSite::new(
        0x7f00_1000,
        0x7f00_2000,
        "java/lang/invoke/VarHandle",
        "get",
        "(LHolder;)I",
        2,
        b'I',
        0,
        0,
        0,
    );
    let addr = &*record as *const cratonvm_jit::VarHandleJudgedSite as usize;
    // SAFETY: an untagged word is returned as it is, never read.
    let (vm, site) = unsafe { varhandle_thin_call_site(0x7f00_1000) };
    assert_eq!((vm, site.is_null()), (0x7f00_1000, true));
    let tagged = (addr | cratonvm_jit::VARHANDLE_JUDGED_SITE_TAG) as i64;
    // SAFETY: `tagged` is `record`'s address with the tag, alive here.
    let (vm, site) = unsafe { varhandle_thin_call_site(tagged) };
    assert_eq!(vm, 0x7f00_1000);
    assert_eq!(site, &record.info as *const JitInvokeInfo);
    let stand_in = &VARHANDLE_READ_INFOS[4];
    assert_eq!(varhandle_thin_cold_arm_info(site, stand_in), site as i64);
    assert_eq!(
        varhandle_thin_cold_arm_info(std::ptr::null(), stand_in),
        stand_in as *const JitInvokeInfo as i64
    );
    assert!(
        !is_varhandle_stand_in_info(&record.info),
        "the real site is judged"
    );
}

/// The fast arm stays open for an unjudged bind, before any exact mint,
/// and for a null handle (which it declines itself).
#[test]
pub fn the_fast_arm_closes_only_for_a_judged_exact_receiver() {
    let shared = SharedVm::new(VmConfig::default());
    let info = &VARHANDLE_WRITE_INFOS[0];
    // SAFETY: no handle is read: the gate is closed, or the word is null.
    unsafe {
        assert!(varhandle_thin_fast_arm_admitted(
            &shared,
            0x1000,
            std::ptr::null()
        ));
        assert!(varhandle_thin_fast_arm_admitted(&shared, 0x1000, info));
        shared
            .natives
            .var_handle_exact_slot
            .store(1, std::sync::atomic::Ordering::Relaxed);
        assert!(varhandle_thin_fast_arm_admitted(&shared, 0, info));
        assert!(varhandle_thin_fast_arm_admitted(
            &shared,
            0x1000,
            std::ptr::null()
        ));
    }
}
