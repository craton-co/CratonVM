use super::*;

fn lifts(line: &str) -> Instr {
    let i = Instr::lift(format!("{line}\n"));
    assert!(
        !matches!(i, Instr::Raw(_)),
        "`{line}` should lift but stayed Raw"
    );
    let mut out = String::new();
    render_ptx_instr(&i, &mut out);
    assert_eq!(out, format!("{line}\n"), "round trip");
    i
}

fn stays_raw(line: &str) {
    assert!(
        matches!(Instr::lift(format!("{line}\n")), Instr::Raw(_)),
        "`{line}` must not lift"
    );
}

/// Every mnemonic family the emitter writes, as the emitter writes it.
/// The round trip is asserted inside `lifts`; the goldens assert it for
/// every line of every real kernel.
#[test]
fn every_emitted_family_lifts_and_round_trips() {
    for l in [
        "    add.s32 %r1, %r2, %r3;",
        "    add.s64 %rl5, %rl0, %rl4;",
        "    sub.s32 %r1, %r2, 7;",
        "    mul.lo.s64 %rl4, %rl2, %rl3;",
        "    and.b32 %r8, %r6, 31;",
        "    and.b32 %r8, %r6, 0xFFFF;",
        "    or.b32 %r1, %r2, %r3;",
        "    xor.b64 %rl1, %rl2, %rl3;",
        "    shl.b32 %r1, %r2, %r3;",
        "    shr.s32 %r1, %r2, 3;",
        "    shr.u64 %rl1, %rl2, %r3;",
        "    mov.s32 %r1, %r2;",
        "    mov.s32 %r1, -7;",
        "    mov.f32 %f1, 0f406CCCCD;",
        "    mov.f64 %fd1, 0d4005BF0A8B145769;",
        "    mov.b32 %f1, %r2;",
        "    mov.b64 %rl1, %rd2;",
        "    mov.u32 %ru24, %laneid;",
        "    add.rn.f32 %f2, %f0, %f1;",
        "    sub.rn.f64 %fd2, %fd0, 0d3FF0000000000000;",
        "    mul.rn.f32 %f2, %f0, %f1;",
        "    div.rn.f64 %fd2, %fd0, %fd1;",
        "    div.s32 %r3, %r1, %r2;",
        "    rem.s64 %rl3, %rl1, %rl2;",
        "    neg.f32 %f1, %f2;",
        "    abs.f64 %fd1, %fd2;",
        "    sqrt.rn.f32 %f1, %f2;",
        "    ex2.approx.f32 %f1, %f2;",
        "    fma.rn.f64 %fd1, %fd2, %fd3, %fd4;",
        "    cvt.s64.s32 %rl2, %r6;",
        "    cvt.rn.f32.s32 %f1, %r2;",
        "    cvt.rzi.s32.f32 %r1, %f2;",
        "    cvt.rzi.f32.f32 %f1, %f2;",
        "    cvt.rn.f32.f64 %f1, %fd2;",
        "    cvt.s32.s8 %r1, %r2;",
        "    cvt.u16.u32 %rs1, %ru2;",
        "    cvt.f32.f16 %f1, %rs2;",
        "    cvt.rn.f16.f32 %rs1, %f2;",
        "    selp.s32 %r1, %r2, %r3, %p4;",
        "    selp.f64 %fd1, %fd2, 0d0000000000000000, %p4;",
        "    setp.ge.u32 %p0, %r4, %r0;",
        "    setp.nan.f32 %p1, %f2, %f2;",
        "    setp.ne.s64 %p3, %rl1, 0;",
        "    and.pred %p4, %p2, %p3;",
        "    or.pred %p4, %p2, %p3;",
        "    ld.param.u64 %rd4, [ret_ptr];",
        "    ld.param.s32 %r0, [p0_len];",
        "    ld.param.f64 %fd0, [p3];",
        "    ld.global.f32 %f1, [%rd2];",
        "    st.global.s64 [%rd2], %rl1;",
        "    st.global.u32 [%rd2], 0;",
        "    red.global.add.u64 [%rd4], %rl1;",
        "    mad.lo.u32 %ru3, %ru0, %ru1, %ru2;",
        "    mad.wide.s32 %rd2, %r4, 4, %rd0;",
        "    @%p0 bra L_reduce_zero;",
        "    @!%p1 bra L_done;",
        "    @%p4 red.global.add.u64 [%rd4], %rl1;",
        "    @%p9 mov.s32 %r1, %r2;",
        "    bra L_reduce;",
        "    ret;",
        "L_reduce_zero:",
    ] {
        lifts(l);
    }
}

#[test]
fn what_the_grammar_does_not_cover_stays_raw() {
    for l in [
        // vector operands and warp shuffles are WarpFold's business
        "    mov.b64 {%ru4, %ru5}, %rl1;",
        "    shfl.sync.down.b32 %ru6, %ru4, 16, 0x1f, 0xffffffff;",
        // a rounding mode the IR has no name for
        "    cvt.rni.s32.f32 %r1, %f2;",
        // a spelling the renderer would normalise away
        "    mov.s32 %r1, +5;",
        "    mov.s32 %r1, 05;",
        "      add.s32 %r1, %r2, %r3;",
        "    add.s32  %r1, %r2, %r3;",
        "    add.s32 %r1, %r2, %r3",
        // unknown mnemonics
        "    ex2.approx.f64 %fd1, %fd2;",
        "    bar.sync 0;",
    ] {
        stays_raw(l);
    }
    // no trailing newline: not a complete line
    assert!(matches!(Instr::lift("    ret;".to_string()), Instr::Raw(_)));
}

#[test]
fn a_guard_is_kept_around_what_it_guards() {
    let Instr::Guarded { pred, inner } = lifts("    @!%p4 red.global.add.u64 [%rd4], %rl1;")
    else {
        panic!("not a guarded instruction")
    };
    assert!(pred.negated);
    assert_eq!(pred.reg, "%p4");
    assert!(matches!(*inner, Instr::RedAdd { ty: Ty::U64, .. }));
}

#[test]
fn a_branch_stays_a_branch_whether_or_not_it_is_predicated() {
    assert!(matches!(
        lifts("    @%p1 bra L_bounds_fail;"),
        Instr::Bra { pred: Some(_), .. }
    ));
    assert!(matches!(
        lifts("    bra L_bounds_fail;"),
        Instr::Bra { pred: None, .. }
    ));
}
