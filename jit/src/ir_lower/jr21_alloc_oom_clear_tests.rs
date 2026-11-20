use super::*;

fn no_helpers() -> JitRuntimeHelpers {
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; nothing here runs the code.
    unsafe { std::mem::zeroed() }
}

/// A `Lowerer` over `iload_0; ireturn`, enough to emit into.
fn lowerer() -> Lowerer<'static> {
    let code: &'static [u8] = &[0x1A, 0xAC];
    let graph: &'static Graph = Box::leak(Box::new(
        crate::ir::IrBuilder::new(1, 1)
            .build(code, code.len())
            .expect("IR build"),
    ));
    let schedule: &'static Schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
    let plan: &'static SlotPlan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    let empty: &'static HashMap<usize, bool> = Box::leak(Box::new(HashMap::new()));
    let no_direct: &'static HashMap<usize, (usize, bool)> = Box::leak(Box::new(HashMap::new()));
    let no_ic: &'static HashMap<usize, (usize, usize)> = Box::leak(Box::new(HashMap::new()));
    let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
    let no_frame_sites: &'static crate::ir::IrInlineFrameSites =
        Box::leak(Box::new(crate::ir::IrInlineFrameSites::default()));
    let helpers: &'static JitRuntimeHelpers = Box::leak(Box::new(no_helpers()));
    let buf = ExecutableBuffer::new(8192).expect("executable buffer");
    let mut lo = Lowerer::new(
        graph,
        schedule,
        buf,
        1,
        1,
        plan,
        helpers,
        empty,
        &[],
        None,
        no_direct,
        no_ic,
        &no_compact,
        no_frame_sites,
    );
    lo.prologue_zero_rax = false;
    lo.prologue_zero_stretch = false;
    lo.frame_prezeroed = false;
    lo
}

/// `MOV qword [rbp - off], 0`, the clear's encoding outside the prologue.
fn clear_bytes(off: i32) -> Vec<u8> {
    let mut b = vec![0x48, 0xC7, 0x85];
    b.extend_from_slice(&(-off).to_le_bytes());
    b.extend_from_slice(&0i32.to_le_bytes());
    b
}

fn count(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len()).filter(|w| *w == needle).count()
}

#[test]
fn the_new_site_rule_covers_every_switch_combination() {
    // (sink taken, oom switch) for a post-init bump that is offered.
    for (sunk, oom, want) in [
        (true, true, NewSiteClears::SlowArms),
        (true, false, NewSiteClears::SlowArms),
        (false, true, NewSiteClears::FrontOfMap),
        (false, false, NewSiteClears::Disarm),
    ] {
        assert_eq!(
            new_site_clear_placement(sunk, true, oom),
            want,
            "sunk={sunk} oom={oom}"
        );
    }
    // A stub-only route (no bump offered) clears in front of its map
    // whatever the oom switch says.
    for oom in [false, true] {
        assert_eq!(
            new_site_clear_placement(false, false, oom),
            NewSiteClears::FrontOfMap
        );
    }
}
