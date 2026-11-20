use super::with_nested_inline_sites;
use crate::{InlineSite, NestedInlineSite};

fn site(method: &str, nested: Vec<InlineSite>) -> InlineSite {
    InlineSite {
        method_name: method.to_string(),
        nested_sites: nested
            .into_iter()
            .map(|site| NestedInlineSite {
                callee_pc: 1,
                guard_class_id: 0,
                site,
                bimorphic_second: None,
            })
            .collect(),
        ..Default::default()
    }
}

#[test]
fn every_nested_body_is_listed_at_any_depth() {
    let tops = [
        site("outer", vec![site("step", vec![site("value", Vec::new())])]),
        site("plain", Vec::new()),
    ];
    let names: Vec<&str> = with_nested_inline_sites(tops.iter())
        .iter()
        .map(|s| s.method_name.as_str())
        .collect();
    assert_eq!(names, ["outer", "plain", "step", "value"]);
}
