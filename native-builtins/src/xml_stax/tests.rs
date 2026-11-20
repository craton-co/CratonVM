use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn tomcat0807_jasper_stax_reports_utf16be_xml_decl_encoding() {
    let text = "<?xml version=\"1.0\" encoding=\"UTF-16BE\"?><root/>";
    let mut bytes = Vec::new();
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_be_bytes());
    }

    let prepared = prepare_xml_bytes(&bytes);

    assert_eq!(
        prepared.character_encoding_scheme.as_deref(),
        Some("UTF-16BE")
    );
    assert_eq!(String::from_utf8(prepared.bytes).unwrap(), text);
}

#[test]
fn tomcat0807_jasper_stax_reports_utf8_bom_decl_encoding() {
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(b"<?xml version='1.0' encoding='UTF-8'?><root/>");

    let prepared = prepare_xml_bytes(&bytes);

    assert_eq!(prepared.character_encoding_scheme.as_deref(), Some("UTF-8"));
    assert!(String::from_utf8(prepared.bytes)
        .unwrap()
        .starts_with("<?xml"));
}

#[test]
fn tomcat0807_jasper_stax_bom_without_decl_has_no_character_scheme() {
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(b"<root/>");

    let prepared = prepare_xml_bytes(&bytes);

    assert_eq!(prepared.character_encoding_scheme, None);
    assert_eq!(String::from_utf8(prepared.bytes).unwrap(), "<root/>");
}
