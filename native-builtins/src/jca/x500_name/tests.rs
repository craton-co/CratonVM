use super::*;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn dn(input: &str) -> Vec<Rdn> {
    parse_dn(input, None).expect("parses")
}

#[test]
fn a_dotted_oid_attribute_renders_hex_in_rfc2253_and_text_in_rfc1779() {
    // MEASURED on HotSpot 25.0.4+7, `L6X500Sweep` rows 244-248.
    let name = dn("SERIALNUMBER=12345,CN=Alice");
    assert_eq!(
        render(&name, Format::Rfc2253),
        "2.5.4.5=#13053132333435,CN=Alice"
    );
    assert_eq!(
        render(&name, Format::Rfc1779),
        "OID.2.5.4.5=12345, CN=Alice"
    );
    assert_eq!(
        render(&name, Format::Display),
        "SERIALNUMBER=12345, CN=Alice"
    );
}

#[test]
fn an_ia5string_is_text_in_rfc2253_and_hex_in_canonical() {
    // `DC` is the case that separates the two rules: rows 218 and 220.
    let name = dn("DC=example,DC=com");
    assert_eq!(render(&name, Format::Rfc2253), "DC=example,DC=com");
    assert_eq!(
        render(&name, Format::Canonical),
        "dc=#16076578616d706c65,dc=#1603636f6d"
    );
}

#[test]
fn a_hex_escape_pins_the_value_to_utf8string() {
    // `CN=\41lice` and `CN=Alice` are the same TEXT and not the same DER:
    // rows 163-169.
    let escaped = dn("CN=\\41lice");
    let plain = dn("CN=Alice");
    assert_eq!(render(&escaped, Format::Rfc2253), "CN=Alice");
    assert_eq!(
        hex(&encode_der(&escaped)),
        "3010310e300c06035504030c05416c696365"
    );
    assert_eq!(
        hex(&encode_der(&plain)),
        "3010310e300c06035504031305416c696365"
    );
}

#[test]
fn utf8_hex_escapes_join_into_one_character() {
    let name = dn("CN=Ren\\C3\\A9");
    assert_eq!(render(&name, Format::Rfc2253), "CN=Ren\u{e9}");
    assert_eq!(
        hex(&encode_der(&name)),
        "3010310e300c06035504030c0552656ec3a9"
    );
}

#[test]
fn a_hash_value_is_raw_der_and_stays_raw() {
    let name = dn("CN=#04024869");
    assert_eq!(render(&name, Format::Rfc2253), "CN=#04024869");
    assert_eq!(render(&name, Format::Rfc1779), "CN=#04024869");
    assert_eq!(hex(&encode_der(&name)), "300d310b3009060355040304024869");
}

#[test]
fn a_quoted_value_keeps_its_comma() {
    let name = dn("CN=\"Smith, Alice\",O=Example");
    assert_eq!(
        render(&name, Format::Rfc2253),
        "CN=Smith\\, Alice,O=Example"
    );
    assert_eq!(
        render(&name, Format::Rfc1779),
        "CN=\"Smith, Alice\", O=Example"
    );
}

#[test]
fn a_semicolon_separates_rdns() {
    let name = dn("CN=Alice;O=Example");
    assert_eq!(render(&name, Format::Rfc2253), "CN=Alice,O=Example");
}

#[test]
fn an_escaped_trailing_space_survives_every_format_but_canonical() {
    let name = dn("CN=Alice\\ ");
    assert_eq!(render(&name, Format::Rfc2253), "CN=Alice\\ ");
    assert_eq!(render(&name, Format::Rfc1779), "CN=\"Alice \"");
    assert_eq!(render(&name, Format::Canonical), "cn=alice");
    assert_eq!(
        hex(&encode_der(&name)),
        "3011310f300d06035504031306416c69636520"
    );
}

#[test]
fn canonical_collapses_spaces_but_not_tabs() {
    assert_eq!(render(&dn("CN=a    b"), Format::Canonical), "cn=a b");
    assert_eq!(render(&dn("CN=a\tb"), Format::Canonical), "cn=a\tb");
    assert_eq!(render(&dn("CN=a\tb"), Format::Rfc1779), "CN=a\tb");
}

#[test]
fn the_malformed_names_the_jdk_rejects_are_rejected() {
    for bad in [
        "CN",
        "=Alice",
        "CN=Alice,",
        ",CN=Alice",
        "CN=Alice,,O=x",
        "CN=Alice+",
        "+CN=Alice",
        "CN=a\\",
        "CN=#0",
        "CN=#zz",
        "CN=#0402",
        "NoSuchKeyword=x",
        "1.2.3.=x",
        "1..2=x",
        "CN=\"unterminated",
        "CN=a\\q",
        "CN=Alice,CN",
        "   ",
    ] {
        assert!(parse_dn(bad, None).is_err(), "should reject {bad:?}");
    }
}

#[test]
fn an_equals_inside_a_value_is_kept_and_escaped_on_the_way_out() {
    // `CN=Alice O=x` is NOT malformed: the JDK reads the whole tail as the
    // value and escapes the `=` when it renders it (row 344). This one was
    // in the sweep's "bad" list and is the one member of it that parses.
    let name = dn("CN=Alice O=x");
    assert_eq!(render(&name, Format::Rfc2253), "CN=Alice O\\=x");
}

#[test]
fn an_empty_name_is_legal_and_encodes_to_an_empty_sequence() {
    let name = parse_dn("", None).expect("empty parses");
    assert!(name.is_empty());
    assert_eq!(render(&name, Format::Rfc2253), "");
    assert_eq!(hex(&encode_der(&name)), "3000");
}

#[test]
fn a_multi_valued_rdn_keeps_both_attributes_and_sorts_only_in_canonical() {
    let one = dn("CN=Alice+OU=Eng,O=Example");
    let other = dn("OU=Eng+CN=Alice,O=Example");
    assert_eq!(render(&one, Format::Rfc2253), "CN=Alice+OU=Eng,O=Example");
    assert_eq!(
        render(&one, Format::Rfc1779),
        "CN=Alice + OU=Eng, O=Example"
    );
    assert_eq!(
        render(&one, Format::Canonical),
        render(&other, Format::Canonical)
    );
    assert_eq!(hex(&encode_der(&one)), "302e3110300e060355040a13074578616d706c65311a300a060355040b1303456e67300c06035504031305416c696365");
}

#[test]
fn the_der_round_trips_through_decode() {
    for input in [
        "CN=Alice",
        "CN=Alice,OU=Eng,O=Example,C=US",
        "1.2.840.113549.1.9.1=alice@example.com,CN=Alice",
        "CN=#04024869",
        "CN=Smith\\, Alice,O=Example",
        "DC=example,DC=com",
    ] {
        let parsed = dn(input);
        let der = encode_der(&parsed);
        let back = decode_der(&der).expect("decodes");
        assert_eq!(
            render(&parsed, Format::Rfc2253),
            render(&back, Format::Rfc2253),
            "round trip of {input:?}"
        );
        assert_eq!(hex(&der), hex(&encode_der(&back)));
    }
}

#[test]
fn the_keyword_map_is_keyed_by_keyword_not_by_oid() {
    // `X500Principal(String, Map)`'s javadoc: keyword -> OID. The sweep
    // passes it the other way round and HotSpot raises, which is the row
    // this asserts.
    let mut backwards = HashMap::new();
    backwards.insert("1.3.6.1.4.1.99999.1".to_string(), "MYOID".to_string());
    assert!(parse_dn("MYOID=x", Some(&backwards)).is_err());

    let mut correct = HashMap::new();
    correct.insert("MYOID".to_string(), "1.3.6.1.4.1.99999.1".to_string());
    let name = parse_dn("MYOID=x", Some(&correct)).expect("parses");
    assert_eq!(
        render(&name, Format::Rfc2253),
        "1.3.6.1.4.1.99999.1=#130178"
    );
}
