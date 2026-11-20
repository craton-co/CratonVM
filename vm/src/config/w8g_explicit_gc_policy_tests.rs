//! gc-common w8-g: `-XX:±DisableExplicitGC` / `-XX:±ExplicitGCInvokesConcurrent`.

use super::*;

/// Every entry point keeps the pre-flag behaviour unless a flag is given.
#[test]
fn every_entry_point_defaults_to_a_full_explicit_gc() {
    assert_eq!(ExplicitGcPolicy::default(), ExplicitGcPolicy::Full);
    assert_eq!(VmConfig::default().explicit_gc, ExplicitGcPolicy::Full);
    assert_eq!(VmConfig::for_launcher().explicit_gc, ExplicitGcPolicy::Full);
    assert_eq!(
        VmConfig::with_host_jdk_default().explicit_gc,
        ExplicitGcPolicy::Full
    );
}

/// HotSpot's precedence: `DisableExplicitGC` wins; a `-XX:-` form is off.
#[test]
fn disable_wins_over_concurrent_and_minus_forms_are_off() {
    use ExplicitGcPolicy::{Concurrent, Disabled, Full};
    let p = ExplicitGcPolicy::from_hotspot_flags;
    assert_eq!(p(None, None), Full);
    assert_eq!(p(Some(false), Some(false)), Full);
    assert_eq!(p(Some(true), None), Disabled);
    assert_eq!(p(Some(true), Some(true)), Disabled);
    assert_eq!(p(Some(true), Some(false)), Disabled);
    assert_eq!(p(None, Some(true)), Concurrent);
    assert_eq!(p(Some(false), Some(true)), Concurrent);
    assert_eq!(p(None, Some(false)), Full);
}
