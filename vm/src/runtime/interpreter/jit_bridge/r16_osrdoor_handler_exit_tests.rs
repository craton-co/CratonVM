use super::{
    note_osr_optimizing_handler_exit, osr_handler_exit_burst_completes,
    osr_optimizing_already_refused, osr_optimizing_cached, osr_optimizing_refusal_key,
    remember_osr_optimizing_artifact, OsrBuildStamps, OSR_OPTIMIZING_HANDLER_EXIT_BURST,
};
use crate::config::VmConfig;
use crate::vm::SharedVm;
use cratonvm_types::ClassId;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The burst arithmetic: `limit` exits inside `span` complete it; a gap
/// longer than `span` starts a new burst at the late exit.
#[test]
fn a_burst_needs_its_exits_inside_the_span() {
    let t0 = Instant::now();
    let span = Duration::from_millis(250);
    let mut burst = (0, None);
    assert!(!osr_handler_exit_burst_completes(&mut burst, t0, 3, span));
    let soon = t0 + Duration::from_millis(100);
    assert!(!osr_handler_exit_burst_completes(&mut burst, soon, 3, span));
    // Too late: starts over.
    let late = t0 + Duration::from_millis(400);
    assert!(!osr_handler_exit_burst_completes(&mut burst, late, 3, span));
    assert_eq!(burst, (1, Some(late)));
    assert!(!osr_handler_exit_burst_completes(&mut burst, late, 3, span));
    let edge = late + span;
    assert!(osr_handler_exit_burst_completes(&mut burst, edge, 3, span));
}

