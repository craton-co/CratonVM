use super::*;

fn vm() -> SharedVm {
    SharedVm::new(crate::config::VmConfig::default())
}

/// FUZZTOOL21-1: the exit line `jit-stress-battery.sh` parses, its
/// `refused` total the sum of the reasons, one census per VM.
#[test]
fn the_census_line_sums_refusals_and_is_per_vm() {
    let shared = vm();
    let other = vm();
    let c = &shared.jit.bridge.poll_stress_census;
    for _ in 0..5 {
        PollStressCensus::bump(&c.arrivals);
    }
    PollStressCensus::bump(&c.granted);
    PollStressCensus::bump(&c.refused_unnamed);
    PollStressCensus::bump(&c.refused_not_polling_body);
    PollStressCensus::bump(&c.refused_not_polling_body);
    PollStressCensus::bump(&c.refused_chain);
    PollStressCensus::bump(&c.collections);
    let line = c.census_line();
    assert!(
        line.starts_with("[cratonvm-deopt] FUZZ21-1 forced arrivals=5 granted=1 refused=4 "),
        "{line}"
    );
    assert!(line.contains(" refused-not-polling-body=2 "), "{line}");
    assert!(line.contains(" refused-chain=1 "), "{line}");
    assert!(line.contains(" collections=1 "), "{line}");
    assert!(
        other
            .jit
            .bridge
            .poll_stress_census
            .census_line()
            .starts_with("[cratonvm-deopt] FUZZ21-1 forced arrivals=0 granted=0 refused=0 "),
        "another VM's census is its own"
    );
}
