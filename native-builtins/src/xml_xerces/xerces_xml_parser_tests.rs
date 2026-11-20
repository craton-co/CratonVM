use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::FieldMetadata;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ArrayElementType;

const XML_LIMIT_ARRAY_LEN: usize = 16;

fn seed_xmlchar_table(ctx: &mut MockNativeContext) {
    let class_id = ctx
        .ensure_class_initialized(XERCES_XMLCHAR)
        .expect("XMLChar class id");
    ctx.set_declared_fields(
        class_id,
        vec![FieldMetadata {
            name: "CHARS".to_string(),
            descriptor: "[B".to_string(),
            access_flags: 0x0008,
            slot_index: 0,
            declaring_class_id: class_id,
            is_static: true,
        }],
    );
    let chars = ctx.new_array(ArrayElementType::Byte, 128);
    for ch in b'A'..=b'Z' {
        ctx.set_array_element(
            chars,
            ch as usize,
            Value::Int(
                (XMLCHAR_MASK_NAME_START
                    | XMLCHAR_MASK_NAME
                    | XMLCHAR_MASK_NCNAME_START
                    | XMLCHAR_MASK_NCNAME) as i32,
            ),
        );
    }
    for ch in b'a'..=b'z' {
        ctx.set_array_element(
            chars,
            ch as usize,
            Value::Int(
                (XMLCHAR_MASK_NAME_START
                    | XMLCHAR_MASK_NAME
                    | XMLCHAR_MASK_NCNAME_START
                    | XMLCHAR_MASK_NCNAME) as i32,
            ),
        );
    }
    for ch in b'0'..=b'9' {
        ctx.set_array_element(
            chars,
            ch as usize,
            Value::Int((XMLCHAR_MASK_NAME | XMLCHAR_MASK_NCNAME) as i32),
        );
    }
    for (ch, mask) in [
        (
            'A',
            XMLCHAR_MASK_NAME_START
                | XMLCHAR_MASK_NAME
                | XMLCHAR_MASK_NCNAME_START
                | XMLCHAR_MASK_NCNAME,
        ),
        (
            '_',
            XMLCHAR_MASK_NAME_START
                | XMLCHAR_MASK_NAME
                | XMLCHAR_MASK_NCNAME_START
                | XMLCHAR_MASK_NCNAME,
        ),
        (':', XMLCHAR_MASK_NAME_START | XMLCHAR_MASK_NAME),
        ('-', XMLCHAR_MASK_NAME | XMLCHAR_MASK_NCNAME),
        ('.', XMLCHAR_MASK_NAME | XMLCHAR_MASK_NCNAME),
        (' ', XMLCHAR_MASK_SPACE),
    ] {
        ctx.set_array_element(chars, ch as usize, Value::Int((mask as u8 as i8) as i32));
    }
    ctx.set_static_field(class_id, 0, Value::Object(Some(chars)));
}

fn new_xml_limit_analyzer(
    ctx: &mut MockNativeContext,
) -> (ObjectRef, ObjectRef, ObjectRef, ObjectRef, ObjectRef) {
    let class_id = ctx
        .ensure_class_initialized(XML_LIMIT_ANALYZER)
        .expect("XMLLimitAnalyzer class id");
    let analyzer = ctx.alloc_object(class_id, 6);
    let values = ctx.new_array(ArrayElementType::Int, XML_LIMIT_ARRAY_LEN);
    let names = ctx.new_array(ArrayElementType::Reference, XML_LIMIT_ARRAY_LEN);
    let total_value = ctx.new_array(ArrayElementType::Int, XML_LIMIT_ARRAY_LEN);
    let caches = ctx.new_array(ArrayElementType::Reference, XML_LIMIT_ARRAY_LEN);
    for slot in 0..XML_LIMIT_ARRAY_LEN {
        ctx.set_array_element(names, slot, Value::Object(None));
        ctx.set_array_element(caches, slot, Value::Object(None));
    }
    ctx.set_field_by_name(analyzer, "values", Value::Object(Some(values)));
    ctx.set_field_by_name(analyzer, "names", Value::Object(Some(names)));
    ctx.set_field_by_name(analyzer, "totalValue", Value::Object(Some(total_value)));
    ctx.set_field_by_name(analyzer, "caches", Value::Object(Some(caches)));
    (analyzer, values, names, total_value, caches)
}

fn new_xssimple_type_decl(
    ctx: &mut MockNativeContext,
    validation_dv: i32,
    facets_defined: i32,
) -> ObjectRef {
    let class_id = ctx
        .ensure_class_initialized(XERCES_XSSIMPLE_TYPE_DECL)
        .expect("XSSimpleTypeDecl class id");
    let decl = ctx.alloc_object(class_id, 14);
    ctx.set_field_by_name(decl, "fValidationDV", Value::Int(validation_dv));
    ctx.set_field_by_name(decl, "fFacetsDefined", Value::Int(facets_defined));
    decl
}

fn new_string_buffer(ctx: &mut MockNativeContext, text: &str) -> ObjectRef {
    let class_id = ctx
        .ensure_class_initialized("java/lang/StringBuffer")
        .expect("StringBuffer class id");
    let sb = ctx.alloc_object(class_id, 3);
    let units: Vec<u16> = text.encode_utf16().collect();
    let chars = ctx.new_array(ArrayElementType::Char, units.len());
    for (slot, unit) in units.iter().copied().enumerate() {
        ctx.set_array_element(chars, slot, Value::Int(unit as i32));
    }
    ctx.set_field(sb, 0, Value::Object(Some(chars)));
    ctx.set_field(sb, 1, Value::Int(0));
    ctx.set_field(sb, 2, Value::Int(units.len() as i32));
    sb
}

fn new_xsd_key(
    ctx: &mut MockNativeContext,
    system_id: Option<&str>,
    refer_ns: Option<ObjectRef>,
) -> ObjectRef {
    let class_id = ctx
        .ensure_class_initialized(XERCES_XSD_KEY)
        .expect("XSDKey class id");
    let key = ctx.alloc_object(class_id, 3);
    let system_id = system_id.map(|text| ctx.create_string(text));
    ctx.set_field_by_name(key, "systemId", Value::Object(system_id));
    ctx.set_field_by_name(key, "referType", Value::Int(0));
    ctx.set_field_by_name(key, "referNS", Value::Object(refer_ns));
    key
}

fn seed_xmlchar_content_table(ctx: &mut MockNativeContext) {
    seed_xmlchar_table(ctx);
    let class_id = ctx
        .class_id_by_name(XERCES_XMLCHAR)
        .expect("XMLChar class id");
    let chars = match ctx.get_static_field(class_id, 0) {
        Value::Object(Some(chars)) => chars,
        other => panic!("unexpected CHARS value: {other:?}"),
    };
    for ch in ['a', 'b', 'c', 't', 'i', 'l', ' '] {
        let old = ctx
            .get_array_element(chars, ch as usize)
            .as_int()
            .unwrap_or(0);
        ctx.set_array_element(chars, ch as usize, Value::Int(old | XMLCHAR_MASK_CONTENT));
    }
}

fn new_entity_scanner_fixture(
    ctx: &mut MockNativeContext,
    text: &str,
) -> (ObjectRef, ObjectRef, ObjectRef, ObjectRef) {
    let scanner_class = ctx
        .ensure_class_initialized(XERCES_XML_ENTITY_SCANNER)
        .expect("XMLEntityScanner class id");
    let entity_class = ctx
        .ensure_class_initialized(STREAM_SCANNED_ENTITY)
        .expect("ScannedEntity class id");
    let xml_string_class = ctx
        .ensure_class_initialized(XERCES_XML_STRING)
        .expect("XMLString class id");
    let scanner = ctx.alloc_object(scanner_class, 7);
    let entity = ctx.alloc_object(entity_class, 8);
    let xml_string = ctx.alloc_object(xml_string_class, 3);
    let units: Vec<u16> = text.encode_utf16().collect();
    let chars = ctx.new_array(ArrayElementType::Char, units.len());
    for (slot, unit) in units.iter().copied().enumerate() {
        ctx.set_array_element(chars, slot, Value::Int(unit as i32));
    }

    ctx.set_field_by_name(scanner, "fCurrentEntity", Value::Object(Some(entity)));
    ctx.set_field_by_name(scanner, "isExternal", Value::Int(0));
    ctx.set_field_by_name(scanner, "offset", Value::Int(0));
    ctx.set_field_by_name(scanner, "newlines", Value::Int(0));
    ctx.set_field_by_name(scanner, "counted", Value::Int(1));
    ctx.set_field_by_name(scanner, "fLimitAnalyzer", Value::Object(None));
    ctx.set_field_by_name(scanner, "fSymbolTable", Value::Object(None));
    ctx.set_field_by_name(entity, "ch", Value::Object(Some(chars)));
    ctx.set_field_by_name(entity, "position", Value::Int(0));
    ctx.set_field_by_name(entity, "count", Value::Int(units.len() as i32));
    ctx.set_field_by_name(entity, "columnNumber", Value::Int(0));
    ctx.set_field_by_name(entity, "lineNumber", Value::Int(1));
    ctx.set_field_by_name(entity, "isGE", Value::Int(0));
    ctx.set_field_by_name(entity, "name", Value::Object(None));
    ctx.set_field_by_name(entity, "fBufferSize", Value::Int(units.len() as i32));
    ctx.set_field_by_name(xml_string, "ch", Value::Object(None));
    ctx.set_field_by_name(xml_string, "offset", Value::Int(0));
    ctx.set_field_by_name(xml_string, "length", Value::Int(0));
    (scanner, entity, xml_string, chars)
}

fn new_qname(ctx: &mut MockNativeContext) -> ObjectRef {
    let class_id = ctx
        .ensure_class_initialized(XERCES_QNAME)
        .expect("QName class id");
    ctx.alloc_object(class_id, 4)
}

fn new_xerces_opti_node(
    ctx: &mut MockNativeContext,
    class_name: &str,
    prefix: Option<&str>,
    localpart: Option<&str>,
    rawname: Option<&str>,
    uri: Option<&str>,
    node_type: i32,
) -> ObjectRef {
    let class_id = ctx
        .ensure_class_initialized(class_name)
        .expect("opti class id");
    let node = ctx.alloc_object(class_id, 7);
    let prefix = prefix.map(|text| ctx.create_string(text));
    let localpart = localpart.map(|text| ctx.create_string(text));
    let rawname = rawname.map(|text| ctx.create_string(text));
    let uri = uri.map(|text| ctx.create_string(text));
    ctx.set_field_by_name(node, "prefix", Value::Object(prefix));
    ctx.set_field_by_name(node, "localpart", Value::Object(localpart));
    ctx.set_field_by_name(node, "rawname", Value::Object(rawname));
    ctx.set_field_by_name(node, "uri", Value::Object(uri));
    ctx.set_field_by_name(node, "nodeType", Value::Int(node_type));
    ctx.set_field_by_name(node, "hidden", Value::Int(0));
    ctx.set_field_by_name(node, "value", Value::Object(None));
    node
}

fn string_result(ctx: &MockNativeContext, value: Value) -> String {
    match value {
        Value::Object(Some(obj)) => ctx.read_string(obj).expect("string result"),
        other => panic!("unexpected result value: {other:?}"),
    }
}

fn java_string_hash(text: &str) -> i32 {
    text.encode_utf16().fold(0i32, |hash, unit| {
        hash.wrapping_mul(31).wrapping_add(unit as i32)
    })
}

#[test]
fn essential_natives_include_xerces_xml_parser_intrinsics() {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);
    for (class_name, name, descriptor) in [
        (XERCES_XMLCHAR, "isNameStart", "(I)Z"),
        (XERCES_XMLCHAR, "isName", "(I)Z"),
        (XERCES_XMLCHAR, "isNCNameStart", "(I)Z"),
        (XERCES_XMLCHAR, "isNCName", "(I)Z"),
        (XERCES_XMLCHAR, "isSpace", "(I)Z"),
        (XML_LIMIT_ANALYZER, "addValue", "(ILjava/lang/String;I)V"),
        (
            XML_LIMIT_ANALYZER,
            "addValue",
            "(Ljdk/xml/internal/XMLSecurityManager$Limit;Ljava/lang/String;I)V",
        ),
        (XML_LIMIT_ANALYZER, "getValue", "(I)I"),
        (XML_LIMIT_ANALYZER, "getTotalValue", "(I)I"),
        (XML_LIMIT_ANALYZER, "getValueByIndex", "(I)I"),
        (
            XERCES_XSSIMPLE_TYPE_DECL,
            "normalize",
            "(Ljava/lang/String;S)Ljava/lang/String;",
        ),
        (
            XERCES_XSSIMPLE_TYPE_DECL,
            "normalize",
            "(Ljava/lang/Object;S)Ljava/lang/String;",
        ),
        (XERCES_XSD_KEY, "hashCode", "()I"),
        (XERCES_XSD_KEY, "equals", "(Ljava/lang/Object;)Z"),
        (
            XERCES_XML_ENTITY_SCANNER,
            "scanContent",
            "(Lcom/sun/org/apache/xerces/internal/xni/XMLString;)I",
        ),
        (
            XERCES_XML_ENTITY_SCANNER,
            "scanQName",
            "(Lcom/sun/org/apache/xerces/internal/xni/QName;Lcom/sun/org/apache/xerces/internal/impl/XMLScanner$NameType;)Z",
        ),
        (XERCES_XML_ENTITY_SCANNER, "skipSpaces", "()Z"),
        (
            XERCES_XML_ENTITY_SCANNER,
            "normalizeNewlines",
            "(SLcom/sun/org/apache/xerces/internal/xni/XMLString;ZZLcom/sun/org/apache/xerces/internal/impl/XMLScanner$NameType;)Z",
        ),
        (
            XERCES_XML_ENTITY_SCANNER,
            "checkEntityLimit",
            "(Lcom/sun/org/apache/xerces/internal/impl/XMLScanner$NameType;Lcom/sun/xml/internal/stream/Entity$ScannedEntity;II)V",
        ),
        (XERCES_OPTI_NODE_IMPL, "getNodeName", "()Ljava/lang/String;"),
        (
            XERCES_OPTI_NODE_IMPL,
            "getNamespaceURI",
            "()Ljava/lang/String;",
        ),
        (XERCES_OPTI_NODE_IMPL, "getPrefix", "()Ljava/lang/String;"),
        (XERCES_OPTI_NODE_IMPL, "getLocalName", "()Ljava/lang/String;"),
        (XERCES_OPTI_NODE_IMPL, "getNodeType", "()S"),
        (XERCES_OPTI_NODE_IMPL, "getReadOnly", "()Z"),
        (
            XERCES_OPTI_ELEMENT_IMPL,
            "getTagName",
            "()Ljava/lang/String;",
        ),
        (XERCES_OPTI_ATTR_IMPL, "getName", "()Ljava/lang/String;"),
        (XERCES_OPTI_ATTR_IMPL, "getValue", "()Ljava/lang/String;"),
        (
            XERCES_OPTI_ATTR_IMPL,
            "getNodeValue",
            "()Ljava/lang/String;",
        ),
        (XERCES_OPTI_ATTR_IMPL, "getSpecified", "()Z"),
        (XERCES_OPTI_ATTR_IMPL, "isId", "()Z"),
        (XERCES_REGEX_RANGE_TOKEN, "sortRanges", "()V"),
    ] {
        assert!(
            registry.find(class_name, name, descriptor).is_some(),
            "{class_name}.{name}{descriptor} must be in the real-JDK essential registry"
        );
    }
}

#[test]
fn xerces_xmlchar_reads_jdk_chars_masks() {
    let mut ctx = MockNativeContext::new();
    seed_xmlchar_table(&mut ctx);

    assert_eq!(
        native_xerces_xmlchar_is_name_start(&mut ctx, &[Value::Int('A' as i32)])
            .expect("isNameStart")
            .expect("value"),
        Value::Int(1)
    );
    assert_eq!(
        native_xerces_xmlchar_is_name_start(&mut ctx, &[Value::Int('-' as i32)])
            .expect("isNameStart")
            .expect("value"),
        Value::Int(0)
    );
    assert_eq!(
        native_xerces_xmlchar_is_ncname_start(&mut ctx, &[Value::Int(':' as i32)])
            .expect("isNCNameStart")
            .expect("value"),
        Value::Int(0)
    );
    assert_eq!(
        native_xerces_xmlchar_is_name(&mut ctx, &[Value::Int('-' as i32)])
            .expect("isName")
            .expect("value"),
        Value::Int(1)
    );
    assert_eq!(
        native_xerces_xmlchar_is_space(&mut ctx, &[Value::Int(' ' as i32)])
            .expect("isSpace")
            .expect("value"),
        Value::Int(1)
    );
    assert_eq!(
        native_xerces_xmlchar_is_name(&mut ctx, &[Value::Int(0x10000)])
            .expect("isName")
            .expect("value"),
        Value::Int(0)
    );
}

#[test]
fn xml_limit_add_value_max_name_updates_values_and_total() {
    let mut ctx = MockNativeContext::new();
    let (analyzer, values, names, total_value, caches) = new_xml_limit_analyzer(&mut ctx);
    let entity = ctx.create_string("jpa-changelog");

    native_xml_limit_analyzer_add_value_index(
        &mut ctx,
        &[
            Value::Object(Some(analyzer)),
            Value::Int(XML_LIMIT_MAX_NAME),
            Value::Object(Some(entity)),
            Value::Int(23),
        ],
    )
    .expect("addValue");

    let slot = XML_LIMIT_MAX_NAME as usize;
    assert_eq!(ctx.get_array_element(values, slot), Value::Int(23));
    assert_eq!(ctx.get_array_element(total_value, slot), Value::Int(23));
    assert_eq!(ctx.get_array_element(names, slot), Value::Object(None));
    assert_eq!(ctx.get_array_element(caches, slot), Value::Object(None));
    assert_eq!(
        native_xml_limit_analyzer_get_value_index(
            &mut ctx,
            &[
                Value::Object(Some(analyzer)),
                Value::Int(XML_LIMIT_MAX_NAME)
            ],
        )
        .expect("getValue")
        .expect("value"),
        Value::Int(23)
    );
}

#[test]
fn xml_limit_entity_replacement_get_value_reads_total_value() {
    let mut ctx = MockNativeContext::new();
    let (analyzer, values, _names, total_value, _caches) = new_xml_limit_analyzer(&mut ctx);

    native_xml_limit_analyzer_add_value_index(
        &mut ctx,
        &[
            Value::Object(Some(analyzer)),
            Value::Int(XML_LIMIT_ENTITY_REPLACEMENT),
            Value::Object(None),
            Value::Int(7),
        ],
    )
    .expect("addValue first");
    native_xml_limit_analyzer_add_value_index(
        &mut ctx,
        &[
            Value::Object(Some(analyzer)),
            Value::Int(XML_LIMIT_ENTITY_REPLACEMENT),
            Value::Object(None),
            Value::Int(5),
        ],
    )
    .expect("addValue second");

    let slot = XML_LIMIT_ENTITY_REPLACEMENT as usize;
    assert_eq!(ctx.get_array_element(values, slot), Value::Int(0));
    assert_eq!(ctx.get_array_element(total_value, slot), Value::Int(12));
    assert_eq!(
        native_xml_limit_analyzer_get_value_index(
            &mut ctx,
            &[
                Value::Object(Some(analyzer)),
                Value::Int(XML_LIMIT_ENTITY_REPLACEMENT),
            ],
        )
        .expect("getValue")
        .expect("value"),
        Value::Int(12)
    );
}

#[test]
fn xssimple_type_static_normalize_matches_xerces_whitespace_rules() {
    let mut ctx = MockNativeContext::new();
    let content = ctx.create_string(" a\t b\nc ");

    assert_eq!(
        native_xssimple_type_normalize_string(
            &mut ctx,
            &[Value::Object(Some(content)), Value::Int(0)]
        )
        .expect("normalize preserve")
        .expect("preserve value"),
        Value::Object(Some(content))
    );

    let replaced = native_xssimple_type_normalize_string(
        &mut ctx,
        &[Value::Object(Some(content)), Value::Int(1)],
    )
    .expect("normalize replace")
    .expect("replace value");
    // XML Schema's `replace` facet maps TAB/LF/CR to U+0020 and does
    // not collapse pre-existing or newly adjacent spaces.
    assert_eq!(string_result(&ctx, replaced), " a  b c ");

    let collapsed = native_xssimple_type_normalize_string(
        &mut ctx,
        &[Value::Object(Some(content)), Value::Int(2)],
    )
    .expect("normalize collapse")
    .expect("collapse value");
    assert_eq!(string_result(&ctx, collapsed), "a b c");
}

#[test]
fn xssimple_type_object_normalize_honors_datatype_default_without_pattern_facet() {
    let mut ctx = MockNativeContext::new();
    let decl = new_xssimple_type_decl(&mut ctx, 2, 0);
    let value = ctx.create_string(" \t token \n ");

    let trimmed = native_xssimple_type_normalize_object(
        &mut ctx,
        &[
            Value::Object(Some(decl)),
            Value::Object(Some(value)),
            Value::Int(2),
        ],
    )
    .expect("normalize object")
    .expect("object value");
    assert_eq!(string_result(&ctx, trimmed), "token");
}

#[test]
fn xssimple_type_object_normalize_collapses_stringbuffer_in_place() {
    let mut ctx = MockNativeContext::new();
    let decl = new_xssimple_type_decl(&mut ctx, 1, XSSIMPLE_FACET_PATTERN);
    let buffer = new_string_buffer(&mut ctx, "  a\t b\nc  ");

    let normalized = native_xssimple_type_normalize_object(
        &mut ctx,
        &[
            Value::Object(Some(decl)),
            Value::Object(Some(buffer)),
            Value::Int(2),
        ],
    )
    .expect("normalize stringbuffer")
    .expect("stringbuffer value");

    assert_eq!(string_result(&ctx, normalized), "a b c");
    assert_eq!(
        String::from_utf16(&crate::lang_string::sb_read_chars(&ctx, buffer))
            .expect("buffer chars"),
        "a b c"
    );
}

#[test]
fn xsd_key_hash_and_equals_match_xerces_cache_key_semantics() {
    let mut ctx = MockNativeContext::new();
    let refer_ns = ctx.create_string("urn:keycloak:test");
    let same_refer_ns = refer_ns;
    let equal_text_different_refer_ns = ctx.create_string_uninterned("urn:keycloak:test");
    let key = new_xsd_key(&mut ctx, Some("dbchangelog-3.1.xsd"), Some(refer_ns));
    let same = new_xsd_key(&mut ctx, Some("dbchangelog-3.1.xsd"), Some(same_refer_ns));
    let different_ns_identity = new_xsd_key(
        &mut ctx,
        Some("dbchangelog-3.1.xsd"),
        Some(equal_text_different_refer_ns),
    );
    let null_system = new_xsd_key(&mut ctx, None, Some(refer_ns));

    assert_eq!(
        native_xsd_key_hash_code(&mut ctx, &[Value::Object(Some(key))])
            .expect("hashCode")
            .expect("hash value"),
        Value::Int(java_string_hash("urn:keycloak:test"))
    );
    assert_eq!(
        native_xsd_key_equals(
            &mut ctx,
            &[Value::Object(Some(key)), Value::Object(Some(same))]
        )
        .expect("equals same")
        .expect("same value"),
        Value::Int(1)
    );
    assert_eq!(
        native_xsd_key_equals(
            &mut ctx,
            &[
                Value::Object(Some(key)),
                Value::Object(Some(different_ns_identity)),
            ],
        )
        .expect("equals different ns")
        .expect("different ns value"),
        Value::Int(0)
    );
    assert_eq!(
        native_xsd_key_equals(
            &mut ctx,
            &[Value::Object(Some(null_system)), Value::Object(Some(same))]
        )
        .expect("equals null system")
        .expect("null system value"),
        Value::Int(0)
    );
}

#[test]
fn xml_entity_scanner_scan_content_updates_entity_and_xmlstring_slice() {
    let mut ctx = MockNativeContext::new();
    seed_xmlchar_content_table(&mut ctx);
    let (scanner, entity, xml_string, chars) = new_entity_scanner_fixture(&mut ctx, "abc<tail");

    assert_eq!(
        native_xml_entity_scanner_scan_content(
            &mut ctx,
            &[
                Value::Object(Some(scanner)),
                Value::Object(Some(xml_string))
            ]
        )
        .expect("scanContent")
        .expect("scan result"),
        Value::Int('<' as i32)
    );
    assert_eq!(ctx.get_field_by_name(entity, "position"), Value::Int(3));
    assert_eq!(ctx.get_field_by_name(entity, "columnNumber"), Value::Int(3));
    assert_eq!(
        ctx.get_field_by_name(xml_string, "ch"),
        Value::Object(Some(chars))
    );
    assert_eq!(ctx.get_field_by_name(xml_string, "offset"), Value::Int(0));
    assert_eq!(ctx.get_field_by_name(xml_string, "length"), Value::Int(3));
}

#[test]
fn xml_entity_scanner_scan_qname_sets_qname_and_updates_limits() {
    let mut ctx = MockNativeContext::new();
    seed_xmlchar_table(&mut ctx);
    let (analyzer, values, names, total_value, _caches) = new_xml_limit_analyzer(&mut ctx);
    let (scanner, entity, _xml_string, _chars) =
        new_entity_scanner_fixture(&mut ctx, "xsd:element ");
    let qname = new_qname(&mut ctx);
    let entity_name = ctx.create_string("liquibase-entity");
    ctx.set_field_by_name(scanner, "fLimitAnalyzer", Value::Object(Some(analyzer)));
    ctx.set_field_by_name(entity, "isGE", Value::Int(1));
    ctx.set_field_by_name(entity, "name", Value::Object(Some(entity_name)));

    assert_eq!(
        native_xml_entity_scanner_scan_qname(
            &mut ctx,
            &[
                Value::Object(Some(scanner)),
                Value::Object(Some(qname)),
                Value::Object(None),
            ],
        )
        .expect("scanQName")
        .expect("scan result"),
        Value::Int(1)
    );

    assert_eq!(ctx.get_field_by_name(scanner, "offset"), Value::Int(0));
    assert_eq!(ctx.get_field_by_name(entity, "position"), Value::Int(11));
    assert_eq!(
        ctx.get_field_by_name(entity, "columnNumber"),
        Value::Int(11)
    );
    assert_eq!(
        string_result(&ctx, ctx.get_field_by_name(qname, "prefix")),
        "xsd"
    );
    assert_eq!(
        string_result(&ctx, ctx.get_field_by_name(qname, "localpart")),
        "element"
    );
    assert_eq!(
        string_result(&ctx, ctx.get_field_by_name(qname, "rawname")),
        "xsd:element"
    );
    assert_eq!(ctx.get_field_by_name(qname, "uri"), Value::Object(None));
    assert_eq!(
        ctx.get_array_element(values, XML_LIMIT_MAX_NAME as usize),
        Value::Int(7)
    );
    assert_eq!(
        ctx.get_array_element(values, XML_LIMIT_GENERAL_ENTITY_SIZE as usize),
        Value::Int(11)
    );
    assert_eq!(
        ctx.get_array_element(total_value, XML_LIMIT_TOTAL_ENTITY_SIZE as usize),
        Value::Int(11)
    );
    assert_eq!(
        ctx.get_array_element(names, XML_LIMIT_GENERAL_ENTITY_SIZE as usize),
        Value::Object(Some(entity_name))
    );
}

#[test]
fn xml_entity_scanner_skip_spaces_consumes_ascii_whitespace() {
    let mut ctx = MockNativeContext::new();
    let (scanner, entity, _xml_string, _chars) = new_entity_scanner_fixture(&mut ctx, " \tX");

    assert_eq!(
        native_xml_entity_scanner_skip_spaces(&mut ctx, &[Value::Object(Some(scanner))])
            .expect("skipSpaces")
            .expect("skip result"),
        Value::Int(1)
    );
    assert_eq!(ctx.get_field_by_name(entity, "position"), Value::Int(2));
    assert_eq!(ctx.get_field_by_name(entity, "columnNumber"), Value::Int(2));
    assert_eq!(ctx.get_field_by_name(entity, "lineNumber"), Value::Int(1));
    assert_eq!(ctx.get_field_by_name(scanner, "offset"), Value::Int(1));
}

#[test]
fn xml_entity_scanner_normalize_newlines_sets_xmlstring_at_boundary() {
    let mut ctx = MockNativeContext::new();
    let (scanner, entity, xml_string, chars) = new_entity_scanner_fixture(&mut ctx, "\nX");

    assert_eq!(
        native_xml_entity_scanner_normalize_newlines(
            &mut ctx,
            &[
                Value::Object(Some(scanner)),
                Value::Int(1),
                Value::Object(Some(xml_string)),
                Value::Int(0),
                Value::Int(0),
                Value::Object(None),
            ],
        )
        .expect("normalizeNewlines")
        .expect("normalize result"),
        Value::Int(1)
    );
    assert_eq!(ctx.get_field_by_name(scanner, "offset"), Value::Int(0));
    assert_eq!(ctx.get_field_by_name(scanner, "newlines"), Value::Int(1));
    assert_eq!(ctx.get_field_by_name(scanner, "counted"), Value::Int(0));
    assert_eq!(ctx.get_field_by_name(entity, "position"), Value::Int(1));
    assert_eq!(ctx.get_field_by_name(entity, "lineNumber"), Value::Int(2));
    assert_eq!(ctx.get_field_by_name(entity, "columnNumber"), Value::Int(1));
    assert_eq!(ctx.get_array_element(chars, 0), Value::Int('\n' as i32));
    assert_eq!(
        ctx.get_field_by_name(xml_string, "ch"),
        Value::Object(Some(chars))
    );
    assert_eq!(ctx.get_field_by_name(xml_string, "offset"), Value::Int(0));
    assert_eq!(ctx.get_field_by_name(xml_string, "length"), Value::Int(1));
}

#[test]
fn xml_entity_scanner_check_entity_limit_updates_general_entity_accounting() {
    let mut ctx = MockNativeContext::new();
    let (analyzer, values, names, total_value, _caches) = new_xml_limit_analyzer(&mut ctx);
    let (scanner, entity, _xml_string, _chars) = new_entity_scanner_fixture(&mut ctx, "abc");
    let entity_name = ctx.create_string("liquibase-entity");
    ctx.set_field_by_name(scanner, "fLimitAnalyzer", Value::Object(Some(analyzer)));
    ctx.set_field_by_name(entity, "isGE", Value::Int(1));
    ctx.set_field_by_name(entity, "name", Value::Object(Some(entity_name)));

    native_xml_entity_scanner_check_entity_limit(
        &mut ctx,
        &[
            Value::Object(Some(scanner)),
            Value::Object(None),
            Value::Object(Some(entity)),
            Value::Int(0),
            Value::Int(7),
        ],
    )
    .expect("checkEntityLimit");

    assert_eq!(
        ctx.get_array_element(values, XML_LIMIT_GENERAL_ENTITY_SIZE as usize),
        Value::Int(7)
    );
    assert_eq!(
        ctx.get_array_element(total_value, XML_LIMIT_TOTAL_ENTITY_SIZE as usize),
        Value::Int(7)
    );
    assert_eq!(
        ctx.get_array_element(names, XML_LIMIT_GENERAL_ENTITY_SIZE as usize),
        Value::Object(Some(entity_name))
    );
}

#[test]
fn xerces_opti_dom_getters_return_backing_fields() {
    let mut ctx = MockNativeContext::new();
    let node = new_xerces_opti_node(
        &mut ctx,
        XERCES_OPTI_NODE_IMPL,
        Some("xsd"),
        Some("element"),
        Some("xsd:element"),
        Some("http://www.w3.org/2001/XMLSchema"),
        1,
    );
    ctx.set_field_by_name(node, "hidden", Value::Int(1));

    let node_name =
        native_xerces_opti_node_get_node_name(&mut ctx, &[Value::Object(Some(node))])
            .expect("getNodeName")
            .expect("node name");
    assert_eq!(string_result(&ctx, node_name), "xsd:element");
    let namespace =
        native_xerces_opti_node_get_namespace_uri(&mut ctx, &[Value::Object(Some(node))])
            .expect("getNamespaceURI")
            .expect("namespace");
    assert_eq!(
        string_result(&ctx, namespace),
        "http://www.w3.org/2001/XMLSchema"
    );
    let prefix = native_xerces_opti_node_get_prefix(&mut ctx, &[Value::Object(Some(node))])
        .expect("getPrefix")
        .expect("prefix");
    assert_eq!(string_result(&ctx, prefix), "xsd");
    let local_name =
        native_xerces_opti_node_get_local_name(&mut ctx, &[Value::Object(Some(node))])
            .expect("getLocalName")
            .expect("local name");
    assert_eq!(string_result(&ctx, local_name), "element");
    assert_eq!(
        native_xerces_opti_node_get_node_type(&mut ctx, &[Value::Object(Some(node))])
            .expect("getNodeType")
            .expect("node type"),
        Value::Int(1)
    );
    assert_eq!(
        native_xerces_opti_node_get_read_only(&mut ctx, &[Value::Object(Some(node))])
            .expect("getReadOnly")
            .expect("read only"),
        Value::Int(1)
    );

    let attr = new_xerces_opti_node(
        &mut ctx,
        XERCES_OPTI_ATTR_IMPL,
        None,
        Some("name"),
        Some("name"),
        None,
        2,
    );
    let attr_value = ctx.create_string("xs:string");
    ctx.set_field_by_name(attr, "value", Value::Object(Some(attr_value)));

    let attr_name = native_xerces_opti_attr_get_name(&mut ctx, &[Value::Object(Some(attr))])
        .expect("getName")
        .expect("attr name");
    assert_eq!(string_result(&ctx, attr_name), "name");
    let attr_value =
        native_xerces_opti_attr_get_node_value(&mut ctx, &[Value::Object(Some(attr))])
            .expect("getNodeValue")
            .expect("attr value");
    assert_eq!(string_result(&ctx, attr_value), "xs:string");
    assert_eq!(
        native_xerces_opti_attr_get_specified(&mut ctx, &[Value::Object(Some(attr))])
            .expect("getSpecified")
            .expect("specified"),
        Value::Int(1)
    );
    assert_eq!(
        native_xerces_opti_attr_is_id(&mut ctx, &[Value::Object(Some(attr))])
            .expect("isId")
            .expect("id"),
        Value::Int(0)
    );
}

#[test]
fn xerces_regex_range_token_sort_ranges_orders_pairs() {
    let mut ctx = MockNativeContext::new();
    let class_id = ctx
        .ensure_class_initialized(XERCES_REGEX_RANGE_TOKEN)
        .expect("RangeToken class id");
    let token = ctx.alloc_object(class_id, 3);
    let ranges = ctx.new_array(ArrayElementType::Int, 6);
    for (slot, value) in [5, 6, 1, 2, 5, 5].into_iter().enumerate() {
        ctx.set_array_element(ranges, slot, Value::Int(value));
    }
    ctx.set_field_by_name(token, "ranges", Value::Object(Some(ranges)));
    ctx.set_field_by_name(token, "sorted", Value::Int(0));
    ctx.set_field_by_name(token, "compacted", Value::Int(0));

    native_xerces_regex_range_token_sort_ranges(&mut ctx, &[Value::Object(Some(token))])
        .expect("sortRanges");

    let sorted: Vec<i32> = (0..6)
        .map(|slot| ctx.get_array_element(ranges, slot).as_int().unwrap())
        .collect();
    assert_eq!(sorted, vec![1, 2, 5, 5, 5, 6]);
    assert_eq!(ctx.get_field_by_name(token, "sorted"), Value::Int(1));
    assert_eq!(ctx.get_field_by_name(token, "compacted"), Value::Int(0));
}
