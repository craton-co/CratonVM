use super::*;

#[test]
fn a_sticky_site_trap_ban_holds_under_every_acceptance_policy() {
    let memo = IrRefusalMemo::default();
    let sticky = 0x5EED_0021_0001_0001u64;
    let stamped = 0x5EED_0021_0001_0002u64;
    let absent = 0x5EED_0021_0001_0003u64;
    memo.note_refused(sticky);
    memo.note_refused_for_inputs(stamped);
    {
        let _always = AcceptAlways::on();
        assert!(
            memo.already_refused(sticky),
            "C2_ACCEPT=always rebuilt the trapping body"
        );
        assert!(
            !memo.already_refused(stamped),
            "a gate verdict is not read off the evidence policy"
        );
        assert!(!memo.already_refused(absent));
    }
    assert!(memo.holds_sticky_ban(sticky));
    assert!(!memo.holds_sticky_ban(stamped));
    assert!(!memo.holds_sticky_ban(absent));
}

#[test]
fn the_ban_is_read_back_under_the_policys_own_key() {
    const C: &str = "cratonvm/test/LambdaTrapBan";
    let verdicts = crate::JitVerdictRegistry::default();
    let id = cratonvm_types::ClassId::new(2121);
    assert!(!verdicts.ir_method_ban_holds(C, "m", "()V", id));
    verdicts.note_ir_method_refused(C, "m", "()V", id);
    assert!(verdicts.ir_method_ban_holds(C, "m", "()V", id));
    assert!(!verdicts.ir_method_ban_holds(C, "m", "()V", cratonvm_types::ClassId::new(2122)));
    verdicts.note_redefinition();
    assert!(
        !verdicts.ir_method_ban_holds(C, "m", "()V", id),
        "a redefined version is decided afresh"
    );
}
