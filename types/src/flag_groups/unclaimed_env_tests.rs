use super::*;

/// `CRATONVM_<suffix>`. The made-up names below are built, not spelled:
/// `types/tests/flag_declaration_guard.rs` reads every whole
/// `"CRATONVM_..."` literal under a `src/` as a read site.
fn var(suffix: &str) -> String {
    format!("CRATONVM_{suffix}")
}


#[test]
fn claimed_names_are_never_reported() {
    let knob = INVENTORY
        .iter()
        .find_map(|e| e.on_key)
        .expect("the inventory declares keys");
    let names: Vec<String> = vec![
        knob.to_string(),
        SCALARS[0].to_string(),
        Group::GC.var().to_string(),
        var(UNDECLARED_READS[0]),
        var(HARNESS_VARS[0]),
        var(&format!("{}ANYTHING", HARNESS_PREFIXES[0])),
        "JAVA_HOME".to_string(),
    ];
    assert!(unclaimed_env_names(names.iter().map(String::as_str)).is_empty());
    assert_eq!(unclaimed_env_advisory(&[]), None);
}

#[test]
fn a_name_far_from_every_knob_gets_no_guess() {
    let far = var("QQQQQQQQQQQQQQQQQQQQQQQQQQQ");
    assert_eq!(closest_knob_name(&far), None);
    let unknown = var("NOT_A_REAL_KNOB");
    let got = unclaimed_env_names([unknown.as_str(), unknown.as_str()]);
    assert_eq!(got, [unknown], "one entry per name");
}

#[test]
fn edit_distance_is_levenshtein() {
    assert_eq!(edit_distance("kitten", "sitting"), 3);
    assert_eq!(edit_distance("", "abc"), 3);
    assert_eq!(edit_distance("same", "same"), 0);
}
