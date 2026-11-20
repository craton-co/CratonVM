use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// The six tests that used to live here exercised a SECOND parser that
/// this file no longer has: `parse_dn_string`, `parse_grouped_rdns`,
/// `encode_rdns_to_der`, `decode_rdns` and `render_canonical` were the old
/// keyword-and-text model, and every claim they made is now made against
/// the real grammar in `x500_name.rs`'s own tests — where the expected
/// values are HotSpot 25.0.4+7's rather than this VM's.
///
/// The mapping, so the deletion is auditable rather than a disappearance:
///
/// | old test | now |
/// |---|---|
/// | `parse_dn_simple` | `x500_name::tests::a_dotted_oid_attribute_renders_hex_in_rfc2253_and_text_in_rfc1779` and the round-trip test |
/// | `parse_dn_with_escaped_comma` | `a_quoted_value_keeps_its_comma` |
/// | `round_trip_probe_input` | `the_der_round_trips_through_decode` |
/// | `unknown_oid_round_trip` | `the_keyword_map_is_keyed_by_keyword_not_by_oid` + the round-trip test |
/// | `canonical_uppercases_keys` | `canonical_collapses_spaces_but_not_tabs` (canonical LOWERCASES; the old test asserted the opposite and passed, because the old renderer did that) |
/// | `grouped_rdn_preserves_each_attribute_in_der` | `a_multi_valued_rdn_keeps_both_attributes_and_sorts_only_in_canonical` |
///
/// The fifth row is the one worth reading twice. `canonical_uppercases_keys`
/// asserted `canon.starts_with("CN=test")`, and the JDK's CANONICAL form is
/// `cn=test` — the test was written from the implementation, agreed with
/// it, and pinned the defect.
#[test]
fn the_dn_grammar_is_tested_in_x500_name() {
    // A live assertion rather than a comment: the formats this file's
    // natives hand out come from that module, and this is the seam.
    let rdns = x500_name::parse_dn("CN=Test,O=Acme,C=SE", None).expect("parses");
    assert_eq!(
        x500_name::render(&rdns, x500_name::Format::Rfc2253),
        "CN=Test,O=Acme,C=SE"
    );
    assert_eq!(
        x500_name::render(&rdns, x500_name::Format::Canonical),
        "cn=test,o=acme,c=se"
    );
}
