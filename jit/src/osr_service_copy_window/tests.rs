use super::*;


#[test]
fn the_term_drops_only_for_an_osr_compile_with_the_switch() {
    assert!(osr_term_for(true, false), "off: an OSR compile still refuses");
    assert!(!osr_term_for(true, true), "on: the OSR term is dropped");
    assert!(!osr_term_for(false, false), "a method-entry compile never refuses on it");
    assert!(!osr_term_for(false, true));    }