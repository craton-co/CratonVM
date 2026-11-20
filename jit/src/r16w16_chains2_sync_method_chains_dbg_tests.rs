use super::sync_method_chains_dbg_line;

/// The engagement line names the method and the three facts the planner
/// read, in the spelling the orchestrator greps (`ir-sync-method-chains`).
#[test]
fn the_line_names_the_method_and_the_verdict() {
    let on = sync_method_chains_dbg_line("R16Bench", "inc", "()V", true, false, true);
    assert_eq!(
        on,
        "[cratonvm-jitc] ir-sync-method-chains R16Bench.inc()V: switch=on self-lock=false chains=true"
    );
    let off = sync_method_chains_dbg_line(
        "java/util/Hashtable",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        false,
        true,
        false,
    );
    assert!(off.starts_with("[cratonvm-jitc] ir-sync-method-chains java/util/Hashtable.get("));
    assert!(
        off.ends_with("switch=off self-lock=true chains=false"),
        "{off}"
    );
}
