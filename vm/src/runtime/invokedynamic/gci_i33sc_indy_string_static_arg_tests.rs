//! gci i33/sc (`gci-i33sc-indy-string-static-args-unrecorded-and-lossy-FIXED-20261016`):
//! a generic bootstrap's `CONSTANT_String` static argument keeps its own
//! constant-pool index (the class's record owns it, as `ldc` of the entry
//! records it) and, when it carries a lone surrogate, its exact UTF-16
//! units (HotSpot passes the exact String; the `Arc<str>` form had U+FFFD).
use super::*;
use std::collections::HashMap;
use std::sync::Arc;

#[test]
fn gci_i33sc_a_string_static_arg_keeps_its_index_and_exact_units() {
    use cratonvm_reader::constant_pool::ConstantPoolEntry as CPE;
    let units: Arc<[u16]> = Arc::from(vec![0xD800u16, 0x0061]);
    let mut wide = HashMap::new();
    wide.insert(3u16, units);
    let cp = ConstantPool::new_with_wide(
        vec![
            CPE::Tombstone,                           // 0
            CPE::Utf8("plain".into()),                // 1
            CPE::StringReference { string_index: 1 }, // 2
            CPE::Utf8("\u{FFFD}a".into()),            // 3 (the lossy text)
            CPE::StringReference { string_index: 3 }, // 4
        ],
        wide,
    );
    assert!(matches!(
        resolve_static_arg_kind(&cp, "p/Caller", 2),
        Ok(StaticArg::Str { cp_index: 2, ref text }) if text == "plain"
    ));
    match resolve_static_arg_kind(&cp, "p/Caller", 4) {
        Ok(StaticArg::WideStr { cp_index, units }) => {
            assert_eq!(cp_index, 4);
            assert_eq!(units, vec![0xD800u16, 0x0061], "the exact units");
        }
        _ => panic!("a surrogate-bearing String static arg must decode losslessly"),
    }
}

/// Source pin: the generic bootstrap's argument loop reads the switch once
/// and routes both String shapes through the recording helpers.
#[test]
fn gci_i33sc_the_indy_args_route_through_the_recording_helpers() {
    let prod = include_str!("object_methods_getter_refusal.rs").replace("\r\n", "\n");
    assert!(prod.contains(
        "let record_strings = crate::runtime::interpreter::constants::condy_string_arg_record_on();"
    ));
    assert!(prod.contains("crate::runtime::interpreter::constants::condy_string_arg(\n"));
    assert!(prod.contains("crate::runtime::interpreter::constants::condy_wide_string_arg(\n"));
    assert!(!prod.contains("StaticArg::Str(s) =>"));
}
