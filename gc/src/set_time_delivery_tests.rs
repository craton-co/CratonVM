// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
use super::*;

#[test]
fn g1_set_time_delivery_does_not_inherit_zgc_switches() {
    for switches in [ZgcBeanShapeSwitches::KILLED, ZgcBeanShapeSwitches::DEFAULT] {
        let beans = BackendGcBeans::with_switches(BackendBeanShape::G1, true, switches);
        assert!(beans.threshold_set_drain_enabled());
    }
    let zgc = |switches| BackendGcBeans::with_switches(BackendBeanShape::Zgc, true, switches);
    assert!(zgc(ZgcBeanShapeSwitches::DEFAULT).threshold_set_drain_enabled());
    assert!(!zgc(ZgcBeanShapeSwitches::KILLED).threshold_set_drain_enabled());
}
