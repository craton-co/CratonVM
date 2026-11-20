use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn test_eval_simple_expression() {
    assert_eq!(eval_simple_expression("1 + 1"), Some(2.0));
    assert_eq!(eval_simple_expression("3 * 4 + 2"), Some(14.0));
    assert_eq!(eval_simple_expression("(2 + 3) * 4"), Some(20.0));
    assert_eq!(eval_simple_expression("10 / 3"), Some(10.0 / 3.0));
    assert_eq!(eval_simple_expression("-5 + 3"), Some(-2.0));
    assert_eq!(eval_simple_expression("2.5 * 4"), Some(10.0));
    assert_eq!(eval_simple_expression(""), None);
    assert_eq!(eval_simple_expression("hello"), None);
}

#[test]
fn test_stax_parse_events() {
    let xml = r#"<root><child>text</child></root>"#;
    let events = stax_parse_events(xml);
    assert_eq!(events.len(), 5); // start root, start child, text, end child, end root
    assert_eq!(events[0].event_type, STAX_START_ELEMENT);
    assert_eq!(events[0].name.as_deref(), Some("root"));
    assert_eq!(events[1].event_type, STAX_START_ELEMENT);
    assert_eq!(events[1].name.as_deref(), Some("child"));
    assert_eq!(events[2].event_type, STAX_CHARACTERS);
    assert_eq!(events[2].text.as_deref(), Some("text"));
    assert_eq!(events[3].event_type, STAX_END_ELEMENT);
    assert_eq!(events[4].event_type, STAX_END_ELEMENT);
}

#[test]
fn test_stax_self_closing() {
    let xml = r#"<root><br/></root>"#;
    let events = stax_parse_events(xml);
    assert_eq!(events.len(), 4); // start root, start br, end br, end root
    assert_eq!(events[1].name.as_deref(), Some("br"));
    assert_eq!(events[2].event_type, STAX_END_ELEMENT);
}

#[test]
fn test_stax_cdata() {
    let xml = r#"<root><![CDATA[some data]]></root>"#;
    let events = stax_parse_events(xml);
    assert!(events
        .iter()
        .any(|e| e.event_type == STAX_CHARACTERS && e.text.as_deref() == Some("some data")));
}

#[test]
fn test_xml_well_formedness_valid() {
    assert!(validate_xml_well_formedness("<root><child>text</child></root>").is_ok());
    assert!(validate_xml_well_formedness("<br/>").is_ok());
    assert!(validate_xml_well_formedness("<?xml version=\"1.0\"?><root/>").is_ok());
    assert!(validate_xml_well_formedness("<!-- comment --><root/>").is_ok());
    assert!(validate_xml_well_formedness("<root><![CDATA[data]]></root>").is_ok());
}

#[test]
fn test_xml_well_formedness_invalid() {
    assert!(validate_xml_well_formedness("<root><child></root>").is_err());
    assert!(validate_xml_well_formedness("<root>").is_err());
    assert!(validate_xml_well_formedness("</root>").is_err());
    assert!(validate_xml_well_formedness("<root><a></b></root>").is_err());
}

#[test]
fn test_jshell_evaluate_arithmetic() {
    assert_eq!(jshell_evaluate("1 + 1"), "2");
    assert_eq!(jshell_evaluate("3 * 4 + 2"), "14");
    assert_eq!(jshell_evaluate("(2 + 3) * 4"), "20");
}

#[test]
fn test_jshell_evaluate_strings() {
    assert_eq!(jshell_evaluate("\"hello\""), "hello");
    assert_eq!(jshell_evaluate("\"hello\" + \" world\""), "hello world");
    assert_eq!(jshell_evaluate("\"value: \" + 42"), "value: 42");
}

#[test]
fn test_jshell_evaluate_var_decl() {
    assert_eq!(jshell_evaluate("int x = 5"), "5");
    assert_eq!(jshell_evaluate("String s = \"hi\""), "hi");
    assert_eq!(jshell_evaluate("double d = 3.14"), "3.14");
    assert_eq!(jshell_evaluate("var x = 10"), "10");
}

#[test]
fn test_jshell_evaluate_booleans() {
    assert_eq!(jshell_evaluate("true"), "true");
    assert_eq!(jshell_evaluate("false"), "false");
    assert_eq!(jshell_evaluate("3 > 2"), "true");
    assert_eq!(jshell_evaluate("1 >= 1"), "true");
    assert_eq!(jshell_evaluate("5 < 3"), "false");
    assert_eq!(jshell_evaluate("5 != 3"), "true");
}

#[test]
fn test_jshell_evaluate_ternary() {
    assert_eq!(jshell_evaluate("true ? 1 : 2"), "1");
    assert_eq!(jshell_evaluate("false ? 1 : 2"), "2");
    assert_eq!(jshell_evaluate("3 > 2 ? 10 : 20"), "10");
}
