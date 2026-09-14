// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! XML and JSON natives: `javax.xml` DocumentBuilder/SAXParser, and the reflection-based Jackson/Gson shims.
//!
//! Pure code move out of `phases_late.rs` (no logic, signature or ordering
//! changes). Every registration call site is untouched and the per-phase
//! dispatchers stay in the parent module, so the native registration SEQUENCE
//! is byte-identical to before the split.

use super::*;

// =============================================================================
// javax.xml — DocumentBuilderFactory, DocumentBuilder, SAXParserFactory — real XML parsing
// =============================================================================

// DOM Node types (per W3C DOM Level 2)
pub(crate) const NODE_ELEMENT: i32 = 1;

pub(crate) const NODE_ATTRIBUTE: i32 = 2;

pub(crate) const NODE_TEXT: i32 = 3;

pub(crate) const NODE_CDATA: i32 = 4;

pub(crate) const NODE_COMMENT: i32 = 8;

pub(crate) const NODE_DOCUMENT: i32 = 9;

// DOM synthetic field layouts:
// Document = 2-field (root_element=0, doc_type=1)
// Element = 5-field (tag_name=0, attributes_arr=1, children_arr=2, child_count=3, parent=4)
// Text = 2-field (text_content=0, parent=1)
// Attr = 2-field (name=0, value=1)
// NodeList = 2-field (array=0, length=1) — reused for both children and attributes
// NamedNodeMap = 2-field (array=0, length=1)

/// Simple recursive-descent XML parser. Returns (tag, attributes, children, tail).
/// Supports elements, text, CDATA, comments, processing instructions, and XML declarations.
pub(crate) fn xml_parse(input: &str) -> Option<XmlNode> {
    xml_parse_with_ids(input).map(|(node, _)| node)
}

/// Parse, and also report the `(element, attribute)` pairs the DOCTYPE's
/// internal subset declares with attribute type `ID`.
///
/// DOM Level 2 matches `getElementById` only against attributes whose DECLARED
/// type is `ID`, and there are exactly two ways an attribute acquires that type:
/// an `<!ATTLIST elem attr ID …>` declaration, or `Element.setIdAttribute*`.
/// `skip_prolog` used to brace-count over the internal subset and throw it away,
/// so no declaration survived the parse in any form and `getElementById` could
/// only ever return null. It is retained now.
pub(crate) fn xml_parse_with_ids(input: &str) -> Option<(XmlNode, Vec<(String, String)>)> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parser = XmlParser {
        input: trimmed,
        pos: 0,
        id_attributes: Vec::new(),
    };
    // Skip the XML declaration and DOCTYPE, keeping the internal subset's ID
    // declarations.
    parser.skip_prolog();
    let ids = std::mem::take(&mut parser.id_attributes);
    parser.parse_node().map(|node| (node, ids))
}

/// Extract `(element, attribute)` ID declarations from a DTD internal subset.
///
/// Handles the two spellings the XML spec allows for an ID-typed attribute in
/// an `<!ATTLIST>`: the bare `ID` type, and `ID` followed by a default
/// declaration (`#REQUIRED`, `#IMPLIED`, `#FIXED "v"`, or a literal). One
/// `<!ATTLIST>` may declare several attributes for the same element, so the
/// scan walks the whole declaration rather than stopping at the first type.
///
/// `IDREF`/`IDREFS`/`NMTOKEN`… are deliberately NOT matched: only `ID` makes an
/// attribute a `getElementById` key, and matching a prefix would make every
/// `IDREF` attribute a false key.
fn parse_attlist_id_declarations(subset: &str, out: &mut Vec<(String, String)>) {
    let mut rest = subset;
    while let Some(start) = rest.find("<!ATTLIST") {
        let after = &rest[start + "<!ATTLIST".len()..];
        let Some(end) = after.find('>') else {
            return;
        };
        let body = &after[..end];
        rest = &after[end + 1..];
        let mut tokens = body.split_whitespace();
        let Some(element) = tokens.next() else {
            continue;
        };
        // Each attribute definition is `name type default…`; walk them in
        // triples, treating a `#FIXED` default as consuming one extra token.
        let tokens: Vec<&str> = tokens.collect();
        let mut i = 0usize;
        while i + 1 < tokens.len() {
            let attr = tokens[i];
            let att_type = tokens[i + 1];
            let mut consumed = 2;
            if let Some(default) = tokens.get(i + 2) {
                if default.eq_ignore_ascii_case("#FIXED") {
                    consumed += 2;
                } else {
                    consumed += 1;
                }
            }
            if att_type == "ID" {
                out.push((element.to_string(), attr.to_string()));
            }
            i += consumed;
        }
    }
}

pub(crate) struct XmlParser<'a> {
    input: &'a str,
    pos: usize,
    /// `(element, attribute)` pairs the DOCTYPE internal subset declares with
    /// attribute type `ID`. Filled by `skip_prolog`; drained by
    /// [`xml_parse_with_ids`].
    id_attributes: Vec<(String, String)>,
}

#[derive(Debug)]
pub(crate) enum XmlNode {
    Element {
        tag: String,
        attributes: Vec<(String, String)>,
        children: Vec<XmlNode>,
    },
    Text(String),
    CData(String),
    Comment(String),
}

impl<'a> XmlParser<'a> {
    fn remaining(&self) -> &str {
        &self.input[self.pos..]
    }

    fn skip_ws(&mut self) {
        while self.pos < self.input.len() && self.input.as_bytes()[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn skip_prolog(&mut self) {
        loop {
            self.skip_ws();
            let rem = self.remaining();
            if rem.starts_with("<?") {
                // Processing instruction — skip to ?>
                if let Some(end) = rem.find("?>") {
                    self.pos += end + 2;
                } else {
                    break;
                }
            } else if rem.starts_with("<!DOCTYPE") {
                // DOCTYPE — skip to the '>' that closes it, brace-counting over
                // the internal subset. The subset is HARVESTED on the way past
                // (`<!ATTLIST … ID …>`) rather than discarded: those
                // declarations are what makes `Document.getElementById` able to
                // match anything at all.
                let mut depth = 0;
                let mut subset_start = None;
                let mut subset: Option<String> = None;
                let mut consumed = None;
                for (i, b) in rem.bytes().enumerate() {
                    if b == b'[' {
                        if depth == 0 {
                            subset_start = Some(i + 1);
                        }
                        depth += 1;
                    }
                    if b == b']' {
                        depth -= 1;
                        if depth == 0 {
                            if let Some(start) = subset_start.take() {
                                subset = Some(rem[start..i].to_string());
                            }
                        }
                    }
                    if b == b'>' && depth <= 0 {
                        consumed = Some(i + 1);
                        break;
                    }
                }
                if let Some(subset) = subset {
                    parse_attlist_id_declarations(&subset, &mut self.id_attributes);
                }
                if let Some(consumed) = consumed {
                    self.pos += consumed;
                }
            } else if rem.starts_with("<!--") {
                if let Some(end) = rem.find("-->") {
                    self.pos += end + 3;
                } else {
                    break;
                }
            } else {
                break;
            }
        }
    }

    fn parse_node(&mut self) -> Option<XmlNode> {
        self.skip_ws();
        let rem = self.remaining();
        if rem.is_empty() {
            return None;
        }
        if rem.starts_with("<![CDATA[") {
            self.pos += 9;
            let end = self.remaining().find("]]>")?;
            let text = self.input[self.pos..self.pos + end].to_string();
            self.pos += end + 3;
            Some(XmlNode::CData(text))
        } else if rem.starts_with("<!--") {
            self.pos += 4;
            let end = self.remaining().find("-->")?;
            let text = self.input[self.pos..self.pos + end].to_string();
            self.pos += end + 3;
            Some(XmlNode::Comment(text))
        } else if rem.starts_with("<?") {
            // Processing instruction — skip
            let end = self.remaining().find("?>")?;
            self.pos += end + 2;
            self.parse_node() // skip and try next
        } else if rem.starts_with('<') {
            self.parse_element()
        } else {
            self.parse_text()
        }
    }

    fn parse_text(&mut self) -> Option<XmlNode> {
        let start = self.pos;
        while self.pos < self.input.len() && self.input.as_bytes()[self.pos] != b'<' {
            self.pos += 1;
        }
        if self.pos > start {
            let text = xml_unescape(&self.input[start..self.pos]);
            Some(XmlNode::Text(text))
        } else {
            None
        }
    }

    fn parse_element(&mut self) -> Option<XmlNode> {
        if self.remaining().as_bytes().first() != Some(&b'<') {
            return None;
        }
        self.pos += 1; // skip '<'

        // Parse tag name
        let tag_start = self.pos;
        while self.pos < self.input.len() {
            let b = self.input.as_bytes()[self.pos];
            if b.is_ascii_whitespace() || b == b'>' || b == b'/' {
                break;
            }
            self.pos += 1;
        }
        let tag = self.input[tag_start..self.pos].to_string();

        // Parse attributes
        let mut attributes = Vec::new();
        loop {
            self.skip_ws();
            let rem = self.remaining();
            if rem.starts_with("/>") {
                self.pos += 2;
                return Some(XmlNode::Element {
                    tag,
                    attributes,
                    children: Vec::new(),
                });
            }
            if rem.starts_with('>') {
                self.pos += 1;
                break;
            }
            if rem.is_empty() {
                return None;
            }
            // Parse attribute: name="value" or name='value'
            let attr_start = self.pos;
            while self.pos < self.input.len() {
                let b = self.input.as_bytes()[self.pos];
                if b == b'=' || b.is_ascii_whitespace() || b == b'>' || b == b'/' {
                    break;
                }
                self.pos += 1;
            }
            let attr_name = self.input[attr_start..self.pos].to_string();
            self.skip_ws();
            if self.remaining().starts_with('=') {
                self.pos += 1;
                self.skip_ws();
                let quote = self.input.as_bytes().get(self.pos).copied().unwrap_or(b'"');
                if quote == b'"' || quote == b'\'' {
                    self.pos += 1;
                    let val_start = self.pos;
                    while self.pos < self.input.len() && self.input.as_bytes()[self.pos] != quote {
                        self.pos += 1;
                    }
                    let attr_val = xml_unescape(&self.input[val_start..self.pos]);
                    if self.pos < self.input.len() {
                        self.pos += 1; // skip closing quote
                    }
                    attributes.push((attr_name, attr_val));
                } else {
                    // Unquoted value — read to whitespace or >
                    let val_start = self.pos;
                    while self.pos < self.input.len() {
                        let b = self.input.as_bytes()[self.pos];
                        if b.is_ascii_whitespace() || b == b'>' || b == b'/' {
                            break;
                        }
                        self.pos += 1;
                    }
                    let attr_val = self.input[val_start..self.pos].to_string();
                    attributes.push((attr_name, attr_val));
                }
            }
        }

        // Parse children
        let mut children = Vec::new();
        let close_tag = format!("</{}>", tag);
        loop {
            self.skip_ws();
            let rem = self.remaining();
            if rem.is_empty() || rem.starts_with(&close_tag) {
                break;
            }
            // Also check for close tag with whitespace
            if rem.starts_with("</") {
                break;
            }
            if let Some(child) = self.parse_node() {
                children.push(child);
            } else {
                break;
            }
        }

        // Skip closing tag
        if self.remaining().starts_with("</") {
            if let Some(gt) = self.remaining().find('>') {
                self.pos += gt + 1;
            }
        }

        Some(XmlNode::Element {
            tag,
            attributes,
            children,
        })
    }
}

pub(crate) fn xml_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

/// Build a DOM tree of synthetic objects from a parsed XML tree
pub(crate) fn xml_build_dom(
    ctx: &mut dyn NativeContext,
    node: &XmlNode,
) -> Result<ObjectRef, MethodCallFailed> {
    match node {
        XmlNode::Element {
            tag,
            attributes,
            children,
        } => {
            let elem = try_alloc_concurrent_synthetic(ctx, "org/w3c/dom/Element", 5)?;
            // Pin across the string/attr/child allocs below — a moving young
            // GC there would relocate the fresh element/arrays (native
            // stale-local family).
            let elem_pin = ctx.pin_native_root(elem);
            let tag_s = ctx.create_string(tag);
            let elem_cur = ctx.read_native_pin(elem_pin, elem);
            ctx.set_field(elem_cur, 0, Value::Object(Some(tag_s))); // tag_name

            // Build attributes array
            let attrs_arr = ctx.new_array(
                cratonvm_types::ArrayElementType::Reference,
                attributes.len(),
            );
            let attrs_pin = ctx.pin_native_root(attrs_arr);
            for (i, (name, value)) in attributes.iter().enumerate() {
                let attr = try_alloc_concurrent_synthetic(ctx, "org/w3c/dom/Attr", 3)?;
                let attr_pin = ctx.pin_native_root(attr);
                let n = ctx.create_string(name);
                let n_pin = ctx.pin_native_root(n);
                let v = ctx.create_string(value);
                let attr = ctx.read_native_pin(attr_pin, attr);
                let n = ctx.read_native_pin(n_pin, n);
                ctx.set_field(attr, 0, Value::Object(Some(n)));
                ctx.set_field(attr, 1, Value::Object(Some(v)));
                let attrs_arr = ctx.read_native_pin(attrs_pin, attrs_arr);
                ctx.set_array_element(attrs_arr, i, Value::Object(Some(attr)));
                ctx.unpin_native_roots(attr_pin);
            }
            let elem_cur = ctx.read_native_pin(elem_pin, elem);
            let attrs_arr = ctx.read_native_pin(attrs_pin, attrs_arr);
            ctx.set_field(elem_cur, 1, Value::Object(Some(attrs_arr))); // attributes

            // Build children array
            let children_arr =
                ctx.new_array(cratonvm_types::ArrayElementType::Reference, children.len());
            let children_pin = ctx.pin_native_root(children_arr);
            for (i, child) in children.iter().enumerate() {
                let child_obj = xml_build_dom(ctx, child);
                let children_arr = ctx.read_native_pin(children_pin, children_arr);
                ctx.set_array_element(children_arr, i, Value::Object(Some(child_obj?)));
            }
            let elem_cur = ctx.read_native_pin(elem_pin, elem);
            let children_arr = ctx.read_native_pin(children_pin, children_arr);
            ctx.set_field(elem_cur, 2, Value::Object(Some(children_arr))); // children
            ctx.set_field(elem_cur, 3, Value::Int(children.len() as i32)); // child_count
            ctx.set_field(elem_cur, 4, Value::Object(None)); // parent (set later if needed)
            ctx.unpin_native_roots(elem_pin);
            Ok(elem_cur)
        }
        XmlNode::Text(text) | XmlNode::CData(text) => {
            let t = try_alloc_concurrent_synthetic(ctx, "org/w3c/dom/Text", 2)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh node (native stale-local family).
            let t_pin = ctx.pin_native_root(t);
            let s = ctx.create_string(text);
            let t = ctx.read_native_pin(t_pin, t);
            ctx.set_field(t, 0, Value::Object(Some(s)));
            ctx.set_field(t, 1, Value::Object(None)); // parent
            ctx.unpin_native_roots(t_pin);
            Ok(t)
        }
        XmlNode::Comment(text) => {
            let c = try_alloc_concurrent_synthetic(ctx, "org/w3c/dom/Comment", 2)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh node (native stale-local family).
            let c_pin = ctx.pin_native_root(c);
            let s = ctx.create_string(text);
            let c = ctx.read_native_pin(c_pin, c);
            ctx.set_field(c, 0, Value::Object(Some(s)));
            ctx.set_field(c, 1, Value::Object(None));
            ctx.unpin_native_roots(c_pin);
            Ok(c)
        }
    }
}

/// Read all bytes from an InputStream and return as a String
pub(crate) fn xml_read_input_stream(ctx: &mut dyn NativeContext, is: ObjectRef) -> String {
    // Pin across the read callbacks below — a moving young GC there would
    // relocate the stream (native stale-local family).
    let is_pin = ctx.pin_native_root(is);
    let mut bytes = Vec::new();
    loop {
        let is = ctx.read_native_pin(is_pin, is);
        match ctx.invoke_virtual(is, "read", "()I", &[]) {
            Ok(Some(Value::Int(b))) if b >= 0 => bytes.push(b as u8),
            _ => break,
        }
    }
    ctx.unpin_native_roots(is_pin);
    String::from_utf8_lossy(&bytes).to_string()
}

/// Parse XML string and build DOM document
pub(crate) fn xml_parse_to_document(
    ctx: &mut dyn NativeContext,
    xml_text: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    // Slot 2 carries the DTD-declared ID attributes — see `DOC_ID_ATTRS`.
    let mut doc = try_alloc_concurrent_synthetic(ctx, "org/w3c/dom/Document", 3)?;
    // Pin across the DOM build below — a moving young GC there would relocate
    // the fresh Document (native stale-local family).
    let doc_pin = ctx.pin_native_root(doc);
    let mut id_attributes = Vec::new();
    if let Some((root_node, ids)) = xml_parse_with_ids(xml_text) {
        id_attributes = ids;
        let root_obj = xml_build_dom(ctx, &root_node);
        doc = ctx.read_native_pin(doc_pin, doc);
        ctx.set_field(doc, 0, Value::Object(Some(root_obj?)));
    } else {
        ctx.set_field(doc, 0, Value::Object(None));
    }
    ctx.set_field(doc, 1, Value::Object(None)); // doc_type
    if id_attributes.is_empty() {
        ctx.set_field(doc, DOC_ID_ATTRS, Value::Object(None));
    } else {
        let encoded = encode_id_attributes(&id_attributes);
        let s = ctx.create_string(&encoded);
        // `create_string` allocates, so re-read the document through its pin
        // before the (allocation-free) field write.
        doc = ctx.read_native_pin(doc_pin, doc);
        ctx.set_field(doc, DOC_ID_ATTRS, Value::Object(Some(s)));
    }
    ctx.unpin_native_roots(doc_pin);
    Ok(doc)
}

/// Document slot 2: the DTD-declared ID attributes, or null when the document
/// declared none.
///
/// Stored as one String (`element\tattribute\n` per declaration) rather than as
/// a Java collection: it is written once at parse time and read only by
/// `getElementById`, and a String is a single reference the GC already tracks.
pub(crate) const DOC_ID_ATTRS: usize = 2;

/// `Attr` slot 2: set by `Element.setIdAttribute*`, the second of DOM Level 2's
/// two routes to an ID-typed attribute (the first being a DTD `<!ATTLIST>`).
pub(crate) const ATTR_IS_ID: usize = 2;

fn encode_id_attributes(pairs: &[(String, String)]) -> String {
    let mut out = String::new();
    for (element, attribute) in pairs {
        out.push_str(element);
        out.push('\t');
        out.push_str(attribute);
        out.push('\n');
    }
    out
}

fn decode_id_attributes(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let (element, attribute) = line.split_once('\t')?;
            (!attribute.is_empty()).then(|| (element.to_string(), attribute.to_string()))
        })
        .collect()
}

/// Depth-first search for the element whose DTD-declared ID attribute equals
/// `id`. Returns the FIRST match in document order, which is what a valid
/// document (where IDs are unique) makes unambiguous anyway.
fn dom_find_by_id(
    ctx: &mut dyn NativeContext,
    elem: ObjectRef,
    declarations: &[(String, String)],
    id: &str,
) -> Option<ObjectRef> {
    let tag = match ctx.get_field(elem, 0) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    // Attributes are an array of `org/w3c/dom/Attr` (name = 0, value = 1,
    // ATTR_IS_ID = 2), built by `xml_build_dom`.
    if let Value::Object(Some(attrs)) = ctx.get_field(elem, 1) {
        for i in 0..ctx.array_length(attrs) {
            let Value::Object(Some(attr)) = ctx.get_array_element(attrs, i) else {
                continue;
            };
            let name = match ctx.get_field(attr, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => continue,
            };
            // Two ways an attribute becomes an ID key, per DOM Level 2: a DTD
            // `<!ATTLIST … ID …>` declaration, or `Element.setIdAttribute*`.
            let declared = declarations
                .iter()
                .any(|(e, a)| a == &name && (e == "*" || e == &tag))
                || matches!(ctx.get_field(attr, ATTR_IS_ID), Value::Int(1));
            if !declared {
                continue;
            }
            let value = match ctx.get_field(attr, 1) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            if value == id {
                return Some(elem);
            }
        }
    }
    let count = ctx.get_field(elem, 3).as_int().unwrap_or(0) as usize;
    if let Value::Object(Some(children)) = ctx.get_field(elem, 2) {
        for i in 0..count {
            if let Value::Object(Some(child)) = ctx.get_array_element(children, i) {
                // Elements are the children that themselves carry a child array
                // (text/comment nodes do not) — same discriminator
                // `dom_collect_by_tag` uses.
                if matches!(ctx.get_field(child, 2), Value::Object(Some(_))) {
                    if let Some(found) = dom_find_by_id(ctx, child, declarations, id) {
                        return Some(found);
                    }
                }
            }
        }
    }
    None
}

/// Walk a DOM tree for SAX callbacks
pub(crate) fn sax_walk(
    ctx: &mut dyn NativeContext,
    handler: ObjectRef,
    node: &XmlNode,
) -> Result<(), MethodCallFailed> {
    // Pin across the string/attr allocs and SAX callbacks below — a moving
    // young GC there would relocate `handler` (native stale-local family).
    let handler_pin = ctx.pin_native_root(handler);
    match node {
        XmlNode::Element {
            tag,
            attributes,
            children,
        } => {
            // Build Attributes object for startElement
            let uri = ctx.create_string("");
            let uri_pin = ctx.pin_native_root(uri);
            let tag_s = ctx.create_string(tag);
            let tag_pin = ctx.pin_native_root(tag_s);
            let qname = ctx.create_string(tag);
            let qname_pin = ctx.pin_native_root(qname);
            // SAX Attributes = synthetic with attr data
            let sax_attrs =
                try_alloc_concurrent_synthetic(ctx, "org/xml/sax/helpers/AttributesImpl", 1)?;
            let sax_attrs_pin = ctx.pin_native_root(sax_attrs);
            let attrs_arr = ctx.new_array(
                cratonvm_types::ArrayElementType::Reference,
                attributes.len() * 2,
            );
            let attrs_arr_pin = ctx.pin_native_root(attrs_arr);
            for (i, (name, value)) in attributes.iter().enumerate() {
                let n = ctx.create_string(name);
                let n_pin = ctx.pin_native_root(n);
                let v = ctx.create_string(value);
                let n = ctx.read_native_pin(n_pin, n);
                let attrs_arr = ctx.read_native_pin(attrs_arr_pin, attrs_arr);
                ctx.set_array_element(attrs_arr, i * 2, Value::Object(Some(n)));
                ctx.set_array_element(attrs_arr, i * 2 + 1, Value::Object(Some(v)));
                ctx.unpin_native_roots(n_pin);
            }
            let sax_attrs = ctx.read_native_pin(sax_attrs_pin, sax_attrs);
            let attrs_arr = ctx.read_native_pin(attrs_arr_pin, attrs_arr);
            ctx.set_field(sax_attrs, 0, Value::Object(Some(attrs_arr)));

            let handler_cur = ctx.read_native_pin(handler_pin, handler);
            let uri = ctx.read_native_pin(uri_pin, uri);
            let tag_s = ctx.read_native_pin(tag_pin, tag_s);
            let qname = ctx.read_native_pin(qname_pin, qname);
            let _ = ctx.invoke_virtual(
                handler_cur,
                "startElement",
                "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Lorg/xml/sax/Attributes;)V",
                &[
                    Value::Object(Some(uri)),
                    Value::Object(Some(tag_s)),
                    Value::Object(Some(qname)),
                    Value::Object(Some(sax_attrs)),
                ],
            );
            ctx.unpin_native_roots(uri_pin);

            for child in children {
                let handler_cur = ctx.read_native_pin(handler_pin, handler);
                sax_walk(ctx, handler_cur, child)?;
            }

            let uri2 = ctx.create_string("");
            let uri2_pin = ctx.pin_native_root(uri2);
            let tag_s2 = ctx.create_string(tag);
            let tag2_pin = ctx.pin_native_root(tag_s2);
            let qname2 = ctx.create_string(tag);
            let handler_cur = ctx.read_native_pin(handler_pin, handler);
            let uri2 = ctx.read_native_pin(uri2_pin, uri2);
            let tag_s2 = ctx.read_native_pin(tag2_pin, tag_s2);
            let _ = ctx.invoke_virtual(
                handler_cur,
                "endElement",
                "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
                &[
                    Value::Object(Some(uri2)),
                    Value::Object(Some(tag_s2)),
                    Value::Object(Some(qname2)),
                ],
            );
        }
        XmlNode::Text(text) | XmlNode::CData(text) => {
            // characters(char[], int, int)
            let char_arr = ctx.new_array(cratonvm_types::ArrayElementType::Char, text.len());
            for (i, ch) in text.chars().enumerate() {
                if i < text.len() {
                    ctx.set_array_element(char_arr, i, Value::Int(ch as i32));
                }
            }
            let handler_cur = ctx.read_native_pin(handler_pin, handler);
            let _ = ctx.invoke_virtual(
                handler_cur,
                "characters",
                "([CII)V",
                &[
                    Value::Object(Some(char_arr)),
                    Value::Int(0),
                    Value::Int(text.len() as i32),
                ],
            );
        }
        XmlNode::Comment(_) => {} // SAX doesn't have a default comment handler
    }
    ctx.unpin_native_roots(handler_pin);
    Ok(())
}

/// Recursively collect elements matching a tag name
pub(crate) fn dom_get_elements_by_tag(
    ctx: &mut dyn NativeContext,
    elem: ObjectRef,
    tag_name: &str,
) -> MethodCallResult {
    let mut results = Vec::new();
    dom_collect_by_tag(ctx, elem, tag_name, &mut results);
    // Pin across the NodeList/array allocs below — a moving young GC there
    // would relocate the collected nodes (native stale-local family).
    let result_pins: Vec<usize> = results.iter().map(|o| ctx.pin_native_root(*o)).collect();
    let nl = try_alloc_concurrent_synthetic(ctx, "org/w3c/dom/NodeList", 2)?;
    let nl_pin = ctx.pin_native_root(nl);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, results.len());
    let nl = ctx.read_native_pin(nl_pin, nl);
    for (i, obj) in results.iter().enumerate() {
        let obj = ctx.read_native_pin(result_pins[i], *obj);
        ctx.set_array_element(arr, i, Value::Object(Some(obj)));
    }
    ctx.set_field(nl, 0, Value::Object(Some(arr)));
    ctx.set_field(nl, 1, Value::Int(results.len() as i32));
    ctx.unpin_native_roots(result_pins.first().copied().unwrap_or(nl_pin));
    Ok(Some(Value::Object(Some(nl))))
}

pub(crate) fn dom_collect_by_tag(
    ctx: &mut dyn NativeContext,
    elem: ObjectRef,
    tag_name: &str,
    results: &mut Vec<ObjectRef>,
) {
    // Check if this element matches
    if let Value::Object(Some(name_s)) = ctx.get_field(elem, 0) {
        if let Some(name) = ctx.read_string(name_s) {
            if tag_name == "*" || name == tag_name {
                results.push(elem);
            }
        }
    }
    // Recurse into children
    let count = ctx.get_field(elem, 3).as_int().unwrap_or(0) as usize;
    if let Value::Object(Some(children_arr)) = ctx.get_field(elem, 2) {
        for i in 0..count {
            if let Value::Object(Some(child)) = ctx.get_array_element(children_arr, i) {
                // Only recurse into Element nodes (check if they have a tag name in field 0)
                if matches!(ctx.get_field(child, 0), Value::Object(Some(_))) {
                    // Check if it has children (field 2) — indicator that it's an Element
                    if matches!(ctx.get_field(child, 2), Value::Object(Some(_))) {
                        dom_collect_by_tag(ctx, child, tag_name, results);
                    }
                }
            }
        }
    }
}

/// Recursively extract text content from a DOM node
pub(crate) fn dom_get_text_content(ctx: &mut dyn NativeContext, node: ObjectRef) -> String {
    // If it's a text node (field 0 = text, field 1 = parent, no field 2)
    // Check if it has children (field 2) — if not, it's a text/comment node
    let has_children = matches!(ctx.get_field(node, 2), Value::Object(Some(_)));
    if !has_children {
        // Text node — return field 0
        return match ctx.get_field(node, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
    }
    // Element node — concatenate children's text
    let count = ctx.get_field(node, 3).as_int().unwrap_or(0) as usize;
    let mut result = String::new();
    if let Value::Object(Some(children_arr)) = ctx.get_field(node, 2) {
        for i in 0..count {
            if let Value::Object(Some(child)) = ctx.get_array_element(children_arr, i) {
                result.push_str(&dom_get_text_content(ctx, child));
            }
        }
    }
    result
}

// =============================================================================
// javax.xml.transform — the identity transform
//
// Synthetic `Transformer` layout: slot 0 is the output-property map
// (`java/util/HashMap`), allocated on the first `setOutputProperty`.
// =============================================================================

const TRANSFORMER_PROPS: usize = 0;

/// Default indent step for `indent=yes`. JAXP's own default varies by
/// implementation and is overridden through a vendor-specific
/// `indent-amount` property we do not model; four spaces matches the
/// `com.sun.org.apache.xml` serializer's shipped default.
const XSLT_INDENT_STEP: usize = 4;

/// XML-escape text content.
fn xslt_escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// XML-escape an attribute value (text escapes plus the quote delimiter).
fn xslt_escape_attr(s: &str) -> String {
    xslt_escape_text(s).replace('"', "&quot;")
}

/// A `javax.xml.transform.TransformerException` carrying `message`.
///
/// Built as the real class so `catch (TransformerException)` matches. When the
/// transform cannot be performed this is what callers get — never a silent
/// empty result, which is indistinguishable from a successful transform of an
/// empty document.
fn xslt_exception(ctx: &mut dyn NativeContext, message: &str) -> MethodCallFailed {
    let detail = ctx.create_string(message);
    if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
        "javax/xml/transform/TransformerException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
    ) {
        return MethodCallFailed::ExceptionThrown(exc);
    }
    RuntimeError::IllegalStateException {
        message: message.to_string(),
    }
    .into()
}

/// A `javax.xml.xpath.XPathExpressionException` explaining that CratonVM's
/// synthetic XML surface ships no XPath engine.
///
/// Built as the real class so `catch (XPathExpressionException)` matches; falls
/// back to `IllegalStateException` when the class cannot be constructed (the
/// same shape as `xslt_exception` above).
fn xpath_unsupported(ctx: &mut dyn NativeContext, what: &str) -> MethodCallFailed {
    let message = format!(
        "{what}: CratonVM's synthetic javax.xml surface has no XPath engine \
         (run without --synthetic-jdk to use the real JDK implementation)"
    );
    let detail = ctx.create_string(&message);
    if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
        "javax/xml/xpath/XPathExpressionException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
    ) {
        return MethodCallFailed::ExceptionThrown(exc);
    }
    RuntimeError::IllegalStateException { message }.into()
}

/// Read a `()Ljava/lang/String;` DOM accessor; "" on null or failure.
fn xslt_dom_str(ctx: &mut dyn NativeContext, node: ObjectRef, method: &str) -> String {
    match ctx.invoke_virtual(node, method, "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    }
}

/// Read a numeric DOM accessor (`()S` for `getNodeType`, `()I` for
/// `getLength`); 0 on failure.
fn xslt_dom_num(ctx: &mut dyn NativeContext, node: ObjectRef, method: &str, desc: &str) -> i32 {
    match ctx.invoke_virtual(node, method, desc, &[]) {
        Ok(Some(v)) => v.as_int().unwrap_or(0),
        _ => 0,
    }
}

/// A node's attributes as plain `(name, value)` data — deliberately not as
/// `ObjectRef`s, so the caller holds nothing the GC can move.
fn xslt_attributes(ctx: &mut dyn NativeContext, node: ObjectRef) -> Vec<(String, String)> {
    let map = match ctx.invoke_virtual(node, "getAttributes", "()Lorg/w3c/dom/NamedNodeMap;", &[]) {
        Ok(Some(Value::Object(Some(map)))) => map,
        _ => return Vec::new(),
    };
    // Pin across the accessor callbacks below — a moving young GC there would
    // relocate the map/attribute nodes (native stale-local family).
    let map_pin = ctx.pin_native_root(map);
    let len = xslt_dom_num(ctx, map, "getLength", "()I").max(0);
    let mut attrs = Vec::new();
    for i in 0..len {
        let map = ctx.read_native_pin(map_pin, map);
        if let Ok(Some(Value::Object(Some(attr)))) =
            ctx.invoke_virtual(map, "item", "(I)Lorg/w3c/dom/Node;", &[Value::Int(i)])
        {
            let attr_pin = ctx.pin_native_root(attr);
            let name = xslt_dom_str(ctx, attr, "getNodeName");
            let attr = ctx.read_native_pin(attr_pin, attr);
            let value = xslt_dom_str(ctx, attr, "getNodeValue");
            ctx.unpin_native_roots(attr_pin);
            if !name.is_empty() {
                attrs.push((name, value));
            }
        }
    }
    ctx.unpin_native_roots(map_pin);
    attrs
}

/// A node's children, each returned WITH its own pin handle.
///
/// The caller serializes them one at a time, and serializing a child re-enters
/// the interpreter, so a bare `Vec<ObjectRef>` would go stale halfway through
/// the list. The caller re-reads each entry through its handle immediately
/// before use and releases the whole batch with the first handle.
fn xslt_child_nodes(ctx: &mut dyn NativeContext, node: ObjectRef) -> Vec<(usize, ObjectRef)> {
    let list = match ctx.invoke_virtual(node, "getChildNodes", "()Lorg/w3c/dom/NodeList;", &[]) {
        Ok(Some(Value::Object(Some(list)))) => list,
        _ => return Vec::new(),
    };
    let list_pin = ctx.pin_native_root(list);
    let len = xslt_dom_num(ctx, list, "getLength", "()I").max(0);
    let mut refs = Vec::new();
    let mut pins = Vec::new();
    for i in 0..len {
        let list = ctx.read_native_pin(list_pin, list);
        if let Ok(Some(Value::Object(Some(child)))) =
            ctx.invoke_virtual(list, "item", "(I)Lorg/w3c/dom/Node;", &[Value::Int(i)])
        {
            pins.push(ctx.pin_native_root(child));
            refs.push(child);
        }
    }
    for (index, pin) in pins.iter().enumerate() {
        let current = refs[index];
        refs[index] = ctx.read_native_pin(*pin, current);
    }
    // Drop the whole gathering batch (the NodeList pin sits underneath the
    // child pins, so it cannot be released on its own) and immediately re-pin
    // just the children. Nothing between these two statements can allocate.
    ctx.unpin_native_roots(list_pin);
    let mut pinned = Vec::with_capacity(refs.len());
    for child in refs {
        pinned.push((ctx.pin_native_root(child), child));
    }
    pinned
}

/// Serialize a DOM node to XML text through the public DOM API only, so it
/// works for this file's synthetic nodes and for a real `org.w3c.dom` tree
/// alike. `indent` is `Some(step)` when the `indent` output property is "yes".
fn xslt_serialize_node(
    ctx: &mut dyn NativeContext,
    node: ObjectRef,
    indent: Option<usize>,
    depth: usize,
    out: &mut String,
) -> Result<(), MethodCallFailed> {
    if depth > 256 {
        return Ok(());
    }
    let node_pin = ctx.pin_native_root(node);
    let node_type = xslt_dom_num(ctx, node, "getNodeType", "()S");
    let node = ctx.read_native_pin(node_pin, node);
    match node_type {
        NODE_TEXT | NODE_CDATA => {
            let mut text = xslt_dom_str(ctx, node, "getNodeValue");
            if text.is_empty() {
                let node = ctx.read_native_pin(node_pin, node);
                text = xslt_dom_str(ctx, node, "getData");
            }
            out.push_str(&xslt_escape_text(&text));
        }
        NODE_DOCUMENT => {
            // A Document is serialized through its root element: unlike
            // Element, it is not guaranteed to answer `getChildNodes`.
            let root =
                ctx.invoke_virtual(node, "getDocumentElement", "()Lorg/w3c/dom/Element;", &[]);
            if let Ok(Some(Value::Object(Some(root)))) = root {
                xslt_serialize_node(ctx, root, indent, depth, out)?;
            } else {
                let node = ctx.read_native_pin(node_pin, node);
                let children = xslt_child_nodes(ctx, node);
                let base = children.first().map(|(pin, _)| *pin);
                for (pin, child) in &children {
                    let child = ctx.read_native_pin(*pin, *child);
                    xslt_serialize_node(ctx, child, indent, depth, out)?;
                }
                if let Some(base) = base {
                    ctx.unpin_native_roots(base);
                }
            }
        }
        NODE_ELEMENT => {
            let mut tag = xslt_dom_str(ctx, node, "getTagName");
            let node = ctx.read_native_pin(node_pin, node);
            if tag.is_empty() {
                tag = xslt_dom_str(ctx, node, "getNodeName");
            }
            let node = ctx.read_native_pin(node_pin, node);
            if !tag.is_empty() {
                let attrs = xslt_attributes(ctx, node);
                let node = ctx.read_native_pin(node_pin, node);
                let children = xslt_child_nodes(ctx, node);
                let base = children.first().map(|(pin, _)| *pin);
                // Mixed content is never re-indented: inserting whitespace
                // around a text node would change the element's value.
                let mut mixed = false;
                for (pin, child) in &children {
                    let child = ctx.read_native_pin(*pin, *child);
                    let kind = xslt_dom_num(ctx, child, "getNodeType", "()S");
                    if kind == NODE_TEXT || kind == NODE_CDATA {
                        mixed = true;
                        break;
                    }
                }
                let child_indent = if mixed { None } else { indent };
                if let Some(step) = indent {
                    if depth > 0 {
                        out.push('\n');
                        out.extend(std::iter::repeat(' ').take(step * depth));
                    }
                }
                out.push('<');
                out.push_str(&tag);
                for (name, value) in &attrs {
                    out.push(' ');
                    out.push_str(name);
                    out.push_str("=\"");
                    out.push_str(&xslt_escape_attr(value));
                    out.push('"');
                }
                if children.is_empty() {
                    out.push_str("/>");
                } else {
                    out.push('>');
                    for (pin, child) in &children {
                        let child = ctx.read_native_pin(*pin, *child);
                        xslt_serialize_node(ctx, child, child_indent, depth + 1, out)?;
                    }
                    if let Some(step) = child_indent {
                        out.push('\n');
                        out.extend(std::iter::repeat(' ').take(step * depth));
                    }
                    out.push_str("</");
                    out.push_str(&tag);
                    out.push('>');
                }
                if let Some(base) = base {
                    ctx.unpin_native_roots(base);
                }
            }
        }
        // Comments, PIs and doctypes carry no information the identity
        // transform's callers depend on, and the synthetic Comment node has no
        // value accessor to read them through.
        _ => {}
    }
    ctx.unpin_native_roots(node_pin);
    Ok(())
}

/// One output property of a synthetic `Transformer`, exactly as it was set.
/// Callers compare case-insensitively — JAXP property values are "yes"/"no"
/// tokens, not user data.
fn xslt_output_property(
    ctx: &mut dyn NativeContext,
    transformer: ObjectRef,
    name: &str,
) -> Option<String> {
    if ctx.object_num_fields(transformer) <= TRANSFORMER_PROPS {
        return None;
    }
    let Value::Object(Some(props)) = ctx.get_field(transformer, TRANSFORMER_PROPS) else {
        return None;
    };
    let props_pin = ctx.pin_native_root(props);
    let key = ctx.create_string(name);
    let props = ctx.read_native_pin(props_pin, props);
    let found = cratonvm_native_collections::native_map_get_pub(
        ctx,
        &[Value::Object(Some(props)), Value::Object(Some(key))],
    );
    ctx.unpin_native_roots(props_pin);
    match found {
        Ok(Some(Value::Object(Some(value)))) => ctx.read_string(value),
        _ => None,
    }
}

/// Pull the XML text out of a `javax.xml.transform.Source`.
///
/// `DOMSource` and `StreamSource` cover essentially every identity-transform
/// caller; anything else (notably `SAXSource`) returns `None` so `transform`
/// can report it rather than emit an empty result.
fn xslt_source_text(
    ctx: &mut dyn NativeContext,
    source: ObjectRef,
) -> Result<Option<String>, MethodCallFailed> {
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(source))
        .unwrap_or_default();
    if class_name.ends_with("DOMSource") {
        if let Ok(Some(Value::Object(Some(node)))) =
            ctx.invoke_virtual(source, "getNode", "()Lorg/w3c/dom/Node;", &[])
        {
            let mut out = String::new();
            xslt_serialize_node(ctx, node, None, 0, &mut out)?;
            return Ok(Some(out));
        }
        return Ok(None);
    }
    if class_name.ends_with("StreamSource") {
        let source_pin = ctx.pin_native_root(source);
        if let Ok(Some(Value::Object(Some(stream)))) =
            ctx.invoke_virtual(source, "getInputStream", "()Ljava/io/InputStream;", &[])
        {
            let text = xml_read_input_stream(ctx, stream);
            ctx.unpin_native_roots(source_pin);
            return Ok(Some(text));
        }
        let source = ctx.read_native_pin(source_pin, source);
        if let Ok(Some(Value::Object(Some(reader)))) =
            ctx.invoke_virtual(source, "getReader", "()Ljava/io/Reader;", &[])
        {
            // `Reader.read()` yields chars; the stream drain's `read()` loop is
            // the same shape, so reuse it rather than duplicating the pinning.
            let text = xml_read_input_stream(ctx, reader);
            ctx.unpin_native_roots(source_pin);
            return Ok(Some(text));
        }
        ctx.unpin_native_roots(source_pin);
        return Ok(None);
    }
    Ok(None)
}

/// Write `text` into a `javax.xml.transform.Result`. Returns `false` when the
/// Result kind is not one we can write, so the caller can raise
/// `TransformerException` instead of dropping the output.
fn xslt_write_result(
    ctx: &mut dyn NativeContext,
    result: ObjectRef,
    text: &str,
) -> Result<bool, MethodCallFailed> {
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(result))
        .unwrap_or_default();
    // Pin across every callback below — each one re-enters the interpreter and
    // can move the Result (native stale-local family).
    let result_pin = ctx.pin_native_root(result);
    if class_name.ends_with("DOMResult") {
        let doc = xml_parse_to_document(ctx, text)?;
        let doc_pin = ctx.pin_native_root(doc);
        let result = ctx.read_native_pin(result_pin, result);
        let doc = ctx.read_native_pin(doc_pin, doc);
        let attached = ctx.invoke_virtual(
            result,
            "setNode",
            "(Lorg/w3c/dom/Node;)V",
            &[Value::Object(Some(doc))],
        );
        ctx.unpin_native_roots(result_pin);
        attached?;
        return Ok(true);
    }
    if !class_name.ends_with("StreamResult") {
        ctx.unpin_native_roots(result_pin);
        return Ok(false);
    }
    if let Ok(Some(Value::Object(Some(writer)))) =
        ctx.invoke_virtual(result, "getWriter", "()Ljava/io/Writer;", &[])
    {
        let writer_pin = ctx.pin_native_root(writer);
        let payload = ctx.create_string(text);
        let writer = ctx.read_native_pin(writer_pin, writer);
        let written = ctx.invoke_virtual(
            writer,
            "write",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(payload))],
        );
        // The flush PROPAGATES, on the same footing as the `write` beside it.
        // `Transformer.transform` declares `throws TransformerException` and
        // the JDK serializer's `flush()` failure reaches the caller as one:
        // `javax.xml.transform.stream.StreamResult` writing to a `Writer` is
        // the buffered case, so the flush is where the bytes actually land.
        // The site already propagated `written` and dropped the flush next to
        // it — two halves of one delivery answered differently.
        // W7-57-close-flush-swallow-sweep.md
        let writer = ctx.read_native_pin(writer_pin, writer);
        let flushed = if written.is_ok() {
            ctx.invoke_virtual(writer, "flush", "()V", &[]).map(|_| ())
        } else {
            Ok(())
        };
        ctx.unpin_native_roots(result_pin);
        written?;
        flushed?;
        return Ok(true);
    }
    let result = ctx.read_native_pin(result_pin, result);
    if let Ok(Some(Value::Object(Some(stream)))) =
        ctx.invoke_virtual(result, "getOutputStream", "()Ljava/io/OutputStream;", &[])
    {
        let stream_pin = ctx.pin_native_root(stream);
        let bytes = text.as_bytes();
        let buffer = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
        let buffer_pin = ctx.pin_native_root(buffer);
        for (index, byte) in bytes.iter().enumerate() {
            ctx.set_array_element(buffer, index, Value::Int(*byte as i8 as i32));
        }
        let stream = ctx.read_native_pin(stream_pin, stream);
        let buffer = ctx.read_native_pin(buffer_pin, buffer);
        let written = ctx.invoke_virtual(stream, "write", "([B)V", &[Value::Object(Some(buffer))]);
        // Same as the `Writer` branch above: the flush is the other half of
        // the same delivery and propagates with it.
        // W7-57-close-flush-swallow-sweep.md
        let stream = ctx.read_native_pin(stream_pin, stream);
        let flushed = if written.is_ok() {
            ctx.invoke_virtual(stream, "flush", "()V", &[]).map(|_| ())
        } else {
            Ok(())
        };
        ctx.unpin_native_roots(result_pin);
        written?;
        flushed?;
        return Ok(true);
    }
    ctx.unpin_native_roots(result_pin);
    Ok(false)
}

// ---------------------------------------------------------------------------
// DOM namespace resolution
//
// The synthetic DOM keeps namespace declarations as ordinary attributes, so
// `Node.getNamespaceURI` / `getPrefix` / `getLocalName` are answerable without
// any parser change: split the qualified name in slot 0, then resolve the
// prefix against the `xmlns*` attributes of the node and its ancestors.
// ---------------------------------------------------------------------------

/// The one namespace URI DOM binds unconditionally, without any declaration.
const DOM_XML_NS_URI: &str = "http://www.w3.org/XML/1998/namespace";

/// Ancestor-walk bound. A parsed document is a tree, but the parent link is a
/// raw slot a caller could in principle cycle; the bound keeps a corrupted
/// document from hanging the VM.
const DOM_NS_MAX_DEPTH: usize = 4096;

/// Split a DOM qualified name into `(prefix, local)`.
///
/// A name with no colon — or a degenerate leading/trailing one, which is not a
/// legal QName — has no prefix, exactly as `Node.getPrefix` specifies.
fn dom_split_qname(qname: &str) -> (Option<&str>, &str) {
    match qname.find(':') {
        Some(i) if i > 0 && i + 1 < qname.len() => (Some(&qname[..i]), &qname[i + 1..]),
        _ => (None, qname),
    }
}

/// Whether `node` has the 5-slot Element layout (tag/attrs/children/count/parent).
///
/// Text and Attr nodes are 2-slot, and their slot 0 is content/name rather
/// than a qualified name — so the namespace accessors must not read it.
fn dom_is_element_shaped(ctx: &dyn NativeContext, node: ObjectRef) -> bool {
    ctx.object_num_fields(node) >= 5
}

/// The element's qualified name (slot 0).
fn dom_qualified_name(ctx: &dyn NativeContext, node: ObjectRef) -> Option<String> {
    match ctx.get_field(node, 0) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// Resolve `prefix` (or the default namespace, when `None`) against the
/// `xmlns` / `xmlns:p` attributes declared on `node` and its ancestors.
///
/// Returns the declaration's value String **as it already lives on the heap**
/// — no allocation, so no GC can move the receiver mid-walk. An `xmlns=""`
/// declaration undeclares the default namespace, which DOM reports as null.
fn dom_lookup_namespace_uri(
    ctx: &dyn NativeContext,
    node: ObjectRef,
    prefix: Option<&str>,
) -> Option<ObjectRef> {
    let wanted = match prefix {
        Some(p) => format!("xmlns:{p}"),
        None => "xmlns".to_string(),
    };
    let mut cur = node;
    for _ in 0..DOM_NS_MAX_DEPTH {
        if !dom_is_element_shaped(ctx, cur) {
            return None;
        }
        if let Value::Object(Some(attrs)) = ctx.get_field(cur, 1) {
            let len = ctx.array_length(attrs);
            for i in 0..len {
                let Value::Object(Some(attr)) = ctx.get_array_element(attrs, i) else {
                    continue;
                };
                if ctx.object_num_fields(attr) < 2 {
                    continue;
                }
                let Value::Object(Some(name)) = ctx.get_field(attr, 0) else {
                    continue;
                };
                if ctx.read_string(name).as_deref() != Some(wanted.as_str()) {
                    continue;
                }
                let Value::Object(Some(value)) = ctx.get_field(attr, 1) else {
                    return None;
                };
                // `xmlns=""` (and `xmlns:p=""`) undeclares the binding.
                if ctx.read_string(value).unwrap_or_default().is_empty() {
                    return None;
                }
                return Some(value);
            }
        }
        match ctx.get_field(cur, 4) {
            Value::Object(Some(parent)) => cur = parent,
            _ => return None,
        }
    }
    None
}

/// `Node.getPrefix()` — the part of the qualified name before the colon.
fn dom_node_prefix(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if !dom_is_element_shaped(ctx, this) {
        return Ok(Some(Value::Object(None)));
    }
    let Some(qname) = dom_qualified_name(ctx, this) else {
        return Ok(Some(Value::Object(None)));
    };
    match dom_split_qname(&qname).0 {
        Some(prefix) => {
            let s = ctx.create_string(prefix);
            Ok(Some(Value::Object(Some(s))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

/// `Node.getLocalName()` — the qualified name with any prefix stripped.
///
/// Unprefixed names (the overwhelmingly common case, and every name in a
/// document that uses no namespaces) return the same String object as before.
fn dom_node_local_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let raw = ctx.get_field(this, 0);
    if !dom_is_element_shaped(ctx, this) {
        return Ok(Some(raw));
    }
    let Some(qname) = dom_qualified_name(ctx, this) else {
        return Ok(Some(raw));
    };
    let (prefix, local) = dom_split_qname(&qname);
    if prefix.is_none() {
        return Ok(Some(raw));
    }
    let s = ctx.create_string(local);
    Ok(Some(Value::Object(Some(s))))
}

/// `Node.getNamespaceURI()` — the URI bound to this node's prefix, or to the
/// default namespace when it has none.
fn dom_node_namespace_uri(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if !dom_is_element_shaped(ctx, this) {
        return Ok(Some(Value::Object(None)));
    }
    let Some(qname) = dom_qualified_name(ctx, this) else {
        return Ok(Some(Value::Object(None)));
    };
    let prefix = dom_split_qname(&qname).0;
    // `xml` is bound by the Namespaces spec itself and needs no declaration.
    if prefix == Some("xml") {
        let s = ctx.create_string(DOM_XML_NS_URI);
        return Ok(Some(Value::Object(Some(s))));
    }
    Ok(Some(Value::Object(dom_lookup_namespace_uri(
        ctx, this, prefix,
    ))))
}

pub(crate) fn register_p68_xml(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // DocumentBuilderFactory = 3-field (namespaceAware=0, validating=1, features=2 HashMap)
    let dbf = "javax/xml/parsers/DocumentBuilderFactory";
    r.register(
        dbf,
        "newInstance",
        "()Ljavax/xml/parsers/DocumentBuilderFactory;",
        |ctx, _args| {
            let obj =
                try_alloc_concurrent_synthetic(ctx, "javax/xml/parsers/DocumentBuilderFactory", 3)?;
            ctx.set_field(obj, 0, Value::Int(0)); // namespaceAware
            ctx.set_field(obj, 1, Value::Int(0)); // validating
            let features = try_alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3)?;
            cratonvm_native_collections::native_map_init(ctx, &[Value::Object(Some(features))])
                .ok();
            ctx.set_field(obj, 2, Value::Object(Some(features)));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(dbf, "setNamespaceAware", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 0 {
            ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Int(0)));
        }
        Ok(None)
    });
    r.register(dbf, "isNamespaceAware", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 0 {
            Ok(Some(ctx.get_field(this, 0)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    r.register(dbf, "setValidating", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 1 {
            ctx.set_field(this, 1, args.get(1).copied().unwrap_or(Value::Int(0)));
        }
        Ok(None)
    });
    r.register(dbf, "isValidating", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 1 {
            Ok(Some(ctx.get_field(this, 1)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    r.register(dbf, "setFeature", "(Ljava/lang/String;Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 2 {
            if let Value::Object(Some(features)) = ctx.get_field(this, 2) {
                let key = args.get(1).copied().unwrap_or(Value::Object(None));
                let val_int = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
                // Store as Boolean wrapper
                let bool_obj = try_alloc_concurrent_synthetic(ctx, "java/lang/Boolean", 1)?;
                ctx.set_field(bool_obj, 0, Value::Int(val_int));
                cratonvm_native_collections::native_map_put_pub(
                    ctx,
                    &[
                        Value::Object(Some(features)),
                        key,
                        Value::Object(Some(bool_obj)),
                    ],
                )?;
            }
        }
        Ok(None)
    });
    r.register(dbf, "getFeature", "(Ljava/lang/String;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 2 {
            if let Value::Object(Some(features)) = ctx.get_field(this, 2) {
                let key = args.get(1).copied().unwrap_or(Value::Object(None));
                let result = cratonvm_native_collections::native_map_get_pub(
                    ctx,
                    &[Value::Object(Some(features)), key],
                )?;
                if let Some(Value::Object(Some(bool_obj))) = result {
                    // Boolean wrapper field 0 = 0/1
                    return Ok(Some(ctx.get_field(bool_obj, 0)));
                }
            }
        }
        Ok(Some(Value::Int(0)))
    });
    // The builder now carries the two configuration flags the factory holds
    // (namespaceAware=0, validating=1) so `DocumentBuilder`'s own accessors
    // below can answer truthfully instead of a flat `false`.
    r.register(
        dbf,
        "newDocumentBuilder",
        "()Ljavax/xml/parsers/DocumentBuilder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Read the flags out as plain ints BEFORE the allocation below —
            // `alloc_concurrent_synthetic` can move `this` (native stale-local
            // family), and an i32 carries across a GC where an ObjectRef would
            // not.
            let ns = if ctx.object_num_fields(this) > 0 {
                ctx.get_field(this, 0).as_int().unwrap_or(0)
            } else {
                0
            };
            let validating = if ctx.object_num_fields(this) > 1 {
                ctx.get_field(this, 1).as_int().unwrap_or(0)
            } else {
                0
            };
            let obj = try_alloc_concurrent_synthetic(ctx, "javax/xml/parsers/DocumentBuilder", 2)?;
            if ctx.object_num_fields(obj) > 1 {
                ctx.set_field(obj, 0, Value::Int(ns));
                ctx.set_field(obj, 1, Value::Int(validating));
            }
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // DocumentBuilder — real XML parsing
    let db = "javax/xml/parsers/DocumentBuilder";
    r.register(
        db,
        "parse",
        "(Ljava/io/InputStream;)Lorg/w3c/dom/Document;",
        |ctx, args| {
            let xml_text = match args.get(1) {
                Some(Value::Object(Some(is))) => xml_read_input_stream(ctx, *is),
                _ => String::new(),
            };
            let doc = xml_parse_to_document(ctx, &xml_text)?;
            Ok(Some(Value::Object(Some(doc))))
        },
    );
    r.register(
        db,
        "parse",
        "(Ljava/io/File;)Lorg/w3c/dom/Document;",
        |ctx, args| {
            // Read file path from File object (field 0 = path string)
            let path = match args.get(1) {
                Some(Value::Object(Some(file_obj))) => match ctx.get_field(*file_obj, 0) {
                    Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                    _ => String::new(),
                },
                _ => String::new(),
            };
            let xml_text = std::fs::read_to_string(&path).unwrap_or_default();
            let doc = xml_parse_to_document(ctx, &xml_text)?;
            Ok(Some(Value::Object(Some(doc))))
        },
    );
    r.register(
        db,
        "parse",
        "(Ljava/lang/String;)Lorg/w3c/dom/Document;",
        |ctx, args| {
            // URI string — treat as file path
            let uri = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let path = uri.strip_prefix("file:").unwrap_or(&uri);
            let xml_text = std::fs::read_to_string(path).unwrap_or_default();
            let doc = xml_parse_to_document(ctx, &xml_text)?;
            Ok(Some(Value::Object(Some(doc))))
        },
    );
    r.register(
        db,
        "newDocument",
        "()Lorg/w3c/dom/Document;",
        |ctx, _args| {
            let doc = try_alloc_concurrent_synthetic(ctx, "org/w3c/dom/Document", 2)?;
            ctx.set_field(doc, 0, Value::Object(None));
            ctx.set_field(doc, 1, Value::Object(None));
            Ok(Some(Value::Object(Some(doc))))
        },
    );
    // These two used to be flat `false`, so a caller that had explicitly done
    // `factory.setNamespaceAware(true)` was told its builder was
    // namespace-unaware — the usual reaction is to fall back to a
    // prefix-splitting code path on a document that was parsed for it.
    // Report what the producing factory was configured with.
    r.register(db, "isNamespaceAware", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let flag = if ctx.object_num_fields(this) > 0 {
            ctx.get_field(this, 0).as_int().unwrap_or(0)
        } else {
            0
        };
        Ok(Some(Value::Int(i32::from(flag != 0))))
    });
    r.register(db, "isValidating", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let flag = if ctx.object_num_fields(this) > 1 {
            ctx.get_field(this, 1).as_int().unwrap_or(0)
        } else {
            0
        };
        Ok(Some(Value::Int(i32::from(flag != 0))))
    });

    // SAXParserFactory = 3-field (namespaceAware=0, validating=1, features=2 HashMap)
    let spf = "javax/xml/parsers/SAXParserFactory";
    r.register(
        spf,
        "newInstance",
        "()Ljavax/xml/parsers/SAXParserFactory;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "javax/xml/parsers/SAXParserFactory", 3)?;
            ctx.set_field(obj, 0, Value::Int(0));
            ctx.set_field(obj, 1, Value::Int(0));
            let features = try_alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3)?;
            cratonvm_native_collections::native_map_init(ctx, &[Value::Object(Some(features))])
                .ok();
            ctx.set_field(obj, 2, Value::Object(Some(features)));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(spf, "setNamespaceAware", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 0 {
            ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Int(0)));
        }
        Ok(None)
    });
    r.register(spf, "isNamespaceAware", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 0 {
            Ok(Some(ctx.get_field(this, 0)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    r.register(spf, "setValidating", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 1 {
            ctx.set_field(this, 1, args.get(1).copied().unwrap_or(Value::Int(0)));
        }
        Ok(None)
    });
    r.register(spf, "isValidating", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 1 {
            Ok(Some(ctx.get_field(this, 1)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    r.register(spf, "setFeature", "(Ljava/lang/String;Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 2 {
            if let Value::Object(Some(features)) = ctx.get_field(this, 2) {
                let key = args.get(1).copied().unwrap_or(Value::Object(None));
                let val_int = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
                let bool_obj = try_alloc_concurrent_synthetic(ctx, "java/lang/Boolean", 1)?;
                ctx.set_field(bool_obj, 0, Value::Int(val_int));
                cratonvm_native_collections::native_map_put_pub(
                    ctx,
                    &[
                        Value::Object(Some(features)),
                        key,
                        Value::Object(Some(bool_obj)),
                    ],
                )?;
            }
        }
        Ok(None)
    });
    r.register(spf, "getFeature", "(Ljava/lang/String;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 2 {
            if let Value::Object(Some(features)) = ctx.get_field(this, 2) {
                let key = args.get(1).copied().unwrap_or(Value::Object(None));
                let result = cratonvm_native_collections::native_map_get_pub(
                    ctx,
                    &[Value::Object(Some(features)), key],
                )?;
                if let Some(Value::Object(Some(bool_obj))) = result {
                    return Ok(Some(ctx.get_field(bool_obj, 0)));
                }
            }
        }
        Ok(Some(Value::Int(0)))
    });
    // Carry the factory's namespaceAware flag into the parser (slot 0) so
    // `SAXParser.isNamespaceAware()` below reports what was configured.
    r.register(
        spf,
        "newSAXParser",
        "()Ljavax/xml/parsers/SAXParser;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Plain int before the allocation — see `newDocumentBuilder`.
            let ns = if ctx.object_num_fields(this) > 0 {
                ctx.get_field(this, 0).as_int().unwrap_or(0)
            } else {
                0
            };
            let obj = try_alloc_concurrent_synthetic(ctx, "javax/xml/parsers/SAXParser", 1)?;
            if ctx.object_num_fields(obj) > 0 {
                ctx.set_field(obj, 0, Value::Int(ns));
            }
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // SAXParser — real SAX parsing via handler callbacks
    let sp = "javax/xml/parsers/SAXParser";
    r.register(
        sp,
        "parse",
        "(Ljava/io/InputStream;Lorg/xml/sax/helpers/DefaultHandler;)V",
        |ctx, args| {
            let xml_text = match args.get(1) {
                Some(Value::Object(Some(is))) => xml_read_input_stream(ctx, *is),
                _ => return Ok(None),
            };
            let handler = match args.get(2) {
                Some(Value::Object(Some(h))) => *h,
                _ => return Ok(None),
            };
            if let Some(root) = xml_parse(&xml_text) {
                // The SAX callbacks are arbitrary Java; each can move `handler`.
                let h_pin = ctx.pin_native_root(handler);
                let _ = ctx.invoke_virtual(handler, "startDocument", "()V", &[]);
                let handler = ctx.read_native_pin(h_pin, handler);
                sax_walk(ctx, handler, &root)?;
                let handler = ctx.read_native_pin(h_pin, handler);
                let _ = ctx.invoke_virtual(handler, "endDocument", "()V", &[]);
            }
            Ok(None)
        },
    );
    r.register(
        sp,
        "parse",
        "(Ljava/io/File;Lorg/xml/sax/helpers/DefaultHandler;)V",
        |ctx, args| {
            let path = match args.get(1) {
                Some(Value::Object(Some(f))) => match ctx.get_field(*f, 0) {
                    Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                    _ => String::new(),
                },
                _ => return Ok(None),
            };
            let handler = match args.get(2) {
                Some(Value::Object(Some(h))) => *h,
                _ => return Ok(None),
            };
            let xml_text = std::fs::read_to_string(&path).unwrap_or_default();
            if let Some(root) = xml_parse(&xml_text) {
                // The SAX callbacks are arbitrary Java; each can move `handler`.
                let h_pin = ctx.pin_native_root(handler);
                let _ = ctx.invoke_virtual(handler, "startDocument", "()V", &[]);
                let handler = ctx.read_native_pin(h_pin, handler);
                sax_walk(ctx, handler, &root)?;
                let handler = ctx.read_native_pin(h_pin, handler);
                let _ = ctx.invoke_virtual(handler, "endDocument", "()V", &[]);
            }
            Ok(None)
        },
    );
    r.register(sp, "isNamespaceAware", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let flag = if ctx.object_num_fields(this) > 0 {
            ctx.get_field(this, 0).as_int().unwrap_or(0)
        } else {
            0
        };
        Ok(Some(Value::Int(i32::from(flag != 0))))
    });

    // === DOM Node/Element/Document/Text/Attr/NodeList methods (G7) ===

    // Document
    let doc_cls = "org/w3c/dom/Document";
    r.register(
        doc_cls,
        "getDocumentElement",
        "()Lorg/w3c/dom/Element;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(
        doc_cls,
        "createElement",
        "(Ljava/lang/String;)Lorg/w3c/dom/Element;",
        |ctx, args| {
            let elem = try_alloc_concurrent_synthetic(ctx, "org/w3c/dom/Element", 5)?;
            ctx.set_field(elem, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
            let attrs = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
            let children = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
            ctx.set_field(elem, 1, Value::Object(Some(attrs)));
            ctx.set_field(elem, 2, Value::Object(Some(children)));
            ctx.set_field(elem, 3, Value::Int(0));
            Ok(Some(Value::Object(Some(elem))))
        },
    );
    r.register(
        doc_cls,
        "createTextNode",
        "(Ljava/lang/String;)Lorg/w3c/dom/Text;",
        |ctx, args| {
            let t = try_alloc_concurrent_synthetic(ctx, "org/w3c/dom/Text", 2)?;
            ctx.set_field(t, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
            ctx.set_field(t, 1, Value::Object(None));
            Ok(Some(Value::Object(Some(t))))
        },
    );
    // The `getNodeType()` family below (Document/Element/Text/Comment/Attr)
    // is constant BY DEFINITION — DOM Level 2 fixes one nodeType per node
    // interface (Document=9, Element=1, Text=3, Comment=8, Attr=2), and each
    // native is registered on exactly the class whose constant it returns.
    // These are not stubs and must not be "implemented"; the constants ARE
    // the specification.
    r.register(doc_cls, "getNodeType", "()S", |_ctx, _args| {
        Ok(Some(Value::Int(NODE_DOCUMENT)))
    });
    r.register(
        doc_cls,
        "getNodeName",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let s = ctx.create_string("#document");
            Ok(Some(Value::Object(Some(s))))
        },
    );
    // getElementById — IMPLEMENTED (was an unconditional null).
    //
    // DOM Level 2 matches only attributes whose *declared type* is ID, and there
    // are exactly two ways an attribute acquires that type. Both now exist:
    //   * a DTD `<!ATTLIST elem attr ID …>` declaration. `XmlParser::skip_prolog`
    //     used to brace-count over the DOCTYPE's internal subset and throw it
    //     away, so no declaration survived the parse in any form; it now
    //     harvests them (`parse_attlist_id_declarations`) and
    //     `xml_parse_to_document` stores them on the document (`DOC_ID_ATTRS`);
    //   * `Element.setIdAttribute*`, registered below, which flags the `Attr`
    //     itself (`ATTR_IS_ID`).
    // A document that declares neither still gets null for every argument —
    // which is correct, and is why matching any attribute literally spelled
    // "id" would be a heuristic that DISAGREES with HotSpot.
    r.register(
        doc_cls,
        "getElementById",
        "(Ljava/lang/String;)Lorg/w3c/dom/Element;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => return Ok(Some(Value::Object(None))),
            };
            let Value::Object(Some(root)) = ctx.get_field(this, 0) else {
                return Ok(Some(Value::Object(None)));
            };
            let declarations = match ctx.get_field(this, DOC_ID_ATTRS) {
                Value::Object(Some(s)) => {
                    decode_id_attributes(&ctx.read_string(s).unwrap_or_default())
                }
                _ => Vec::new(),
            };
            Ok(Some(match dom_find_by_id(ctx, root, &declarations, &id) {
                Some(found) => Value::Object(Some(found)),
                None => Value::Object(None),
            }))
        },
    );
    r.register(
        doc_cls,
        "getElementsByTagName",
        "(Ljava/lang/String;)Lorg/w3c/dom/NodeList;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let tag_name = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let root = match ctx.get_field(this, 0) {
                Value::Object(Some(r)) => r,
                _ => {
                    let nl = try_alloc_concurrent_synthetic(ctx, "org/w3c/dom/NodeList", 2)?;
                    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
                    ctx.set_field(nl, 0, Value::Object(Some(arr)));
                    ctx.set_field(nl, 1, Value::Int(0));
                    return Ok(Some(Value::Object(Some(nl))));
                }
            };
            dom_get_elements_by_tag(ctx, root, &tag_name)
        },
    );

    // Element
    let elem_cls = "org/w3c/dom/Element";
    r.register(
        elem_cls,
        "getTagName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(
        elem_cls,
        "getNodeName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(elem_cls, "getNodeType", "()S", |_ctx, _args| {
        Ok(Some(Value::Int(NODE_ELEMENT)))
    });
    r.register(
        elem_cls,
        "getAttribute",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let attr_name = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => return Ok(Some(Value::Object(None))),
            };
            if let Value::Object(Some(attrs_arr)) = ctx.get_field(this, 1) {
                let len = ctx.array_length(attrs_arr);
                for i in 0..len {
                    if let Value::Object(Some(attr)) = ctx.get_array_element(attrs_arr, i) {
                        if let Value::Object(Some(n)) = ctx.get_field(attr, 0) {
                            if ctx.read_string(n).as_deref() == Some(&attr_name) {
                                if let Value::Object(Some(v)) = ctx.get_field(attr, 1) {
                                    return Ok(Some(Value::Object(Some(v))));
                                }
                            }
                        }
                    }
                }
            }
            let empty = ctx.create_string("");
            Ok(Some(Value::Object(Some(empty))))
        },
    );
    r.register(
        elem_cls,
        "hasAttribute",
        "(Ljava/lang/String;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let attr_name = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => return Ok(Some(Value::Int(0))),
            };
            if let Value::Object(Some(attrs_arr)) = ctx.get_field(this, 1) {
                let len = ctx.array_length(attrs_arr);
                for i in 0..len {
                    if let Value::Object(Some(attr)) = ctx.get_array_element(attrs_arr, i) {
                        if let Value::Object(Some(n)) = ctx.get_field(attr, 0) {
                            if ctx.read_string(n).as_deref() == Some(&attr_name) {
                                return Ok(Some(Value::Int(1)));
                            }
                        }
                    }
                }
            }
            Ok(Some(Value::Int(0)))
        },
    );
    r.register(
        elem_cls,
        "setAttribute",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        |ctx, args| {
            let _this = obj_arg(args, 0)?;
            // Simplified: attributes are immutable after parse for now
            let _ = (ctx, args);
            Ok(None)
        },
    );
    // `setIdAttribute(String name, boolean isId)` — the second of DOM Level 2's
    // two routes to an ID-typed attribute, and the only one available to a
    // document with no DTD. Registered because `getElementById` consults it
    // (`ATTR_IS_ID`) alongside the DTD declarations, and because leaving it off
    // the concrete carrier meant an `AbstractMethodError` for any caller that
    // used it. `setIdAttributeNS` ignores the namespace URI for the same reason
    // the rest of this DOM does: attributes are stored by qualified name only.
    fn dom_set_id_attribute(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        // (this, [uri,] name, isId) — the name is the LAST String argument and
        // the flag the last Int, so both overloads read the same way.
        let name = args
            .iter()
            .rev()
            .find_map(|v| match v {
                Value::Object(Some(s)) => ctx.read_string(*s),
                _ => None,
            })
            .unwrap_or_default();
        let is_id = args
            .iter()
            .rev()
            .find_map(|v| match v {
                Value::Int(i) => Some(*i != 0),
                _ => None,
            })
            .unwrap_or(true);
        let Value::Object(Some(attrs)) = ctx.get_field(this, 1) else {
            return Ok(None);
        };
        for i in 0..ctx.array_length(attrs) {
            let Value::Object(Some(attr)) = ctx.get_array_element(attrs, i) else {
                continue;
            };
            let attr_name = match ctx.get_field(attr, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => continue,
            };
            if attr_name == name {
                ctx.set_field(attr, ATTR_IS_ID, Value::Int(i32::from(is_id)));
                return Ok(None);
            }
        }
        // DOM specifies NOT_FOUND_ERR for an attribute this element does not
        // carry.
        Err(RuntimeError::IllegalArgumentException {
            message: format!("setIdAttribute: no attribute named {name}"),
        }
        .into())
    }
    r.register(
        elem_cls,
        "setIdAttribute",
        "(Ljava/lang/String;Z)V",
        dom_set_id_attribute,
    );
    r.register(
        elem_cls,
        "setIdAttributeNS",
        "(Ljava/lang/String;Ljava/lang/String;Z)V",
        dom_set_id_attribute,
    );
    r.register(
        elem_cls,
        "getChildNodes",
        "()Lorg/w3c/dom/NodeList;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let nl = try_alloc_concurrent_synthetic(ctx, "org/w3c/dom/NodeList", 2)?;
            ctx.set_field(nl, 0, ctx.get_field(this, 2)); // children array
            ctx.set_field(nl, 1, ctx.get_field(this, 3)); // child count
            Ok(Some(Value::Object(Some(nl))))
        },
    );
    r.register(
        elem_cls,
        "getElementsByTagName",
        "(Ljava/lang/String;)Lorg/w3c/dom/NodeList;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let tag_name = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            dom_get_elements_by_tag(ctx, this, &tag_name)
        },
    );
    r.register(
        elem_cls,
        "getFirstChild",
        "()Lorg/w3c/dom/Node;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let count = ctx.get_field(this, 3).as_int().unwrap_or(0);
            if count > 0 {
                if let Value::Object(Some(arr)) = ctx.get_field(this, 2) {
                    return Ok(Some(ctx.get_array_element(arr, 0)));
                }
            }
            Ok(Some(Value::Object(None)))
        },
    );
    r.register(
        elem_cls,
        "getLastChild",
        "()Lorg/w3c/dom/Node;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let count = ctx.get_field(this, 3).as_int().unwrap_or(0);
            if count > 0 {
                if let Value::Object(Some(arr)) = ctx.get_field(this, 2) {
                    return Ok(Some(ctx.get_array_element(arr, count as usize - 1)));
                }
            }
            Ok(Some(Value::Object(None)))
        },
    );
    r.register(elem_cls, "hasChildNodes", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let count = ctx.get_field(this, 3).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if count > 0 { 1 } else { 0 })))
    });
    r.register(
        elem_cls,
        "getAttributes",
        "()Lorg/w3c/dom/NamedNodeMap;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let nm = try_alloc_concurrent_synthetic(ctx, "org/w3c/dom/NamedNodeMap", 2)?;
            let attrs = ctx.get_field(this, 1);
            let len = match attrs {
                Value::Object(Some(arr)) => ctx.array_length(arr) as i32,
                _ => 0,
            };
            ctx.set_field(nm, 0, attrs);
            ctx.set_field(nm, 1, Value::Int(len));
            Ok(Some(Value::Object(Some(nm))))
        },
    );
    r.register(
        elem_cls,
        "getTextContent",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let text = dom_get_text_content(ctx, this);
            let s = ctx.create_string(&text);
            Ok(Some(Value::Object(Some(s))))
        },
    );
    r.register(
        elem_cls,
        "getNodeValue",
        "()Ljava/lang/String;",
        |_ctx, _args| {
            Ok(Some(Value::Object(None))) // Element nodes have null nodeValue
        },
    );
    r.register(
        elem_cls,
        "getParentNode",
        "()Lorg/w3c/dom/Node;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 4)))
        },
    );

    // Node interface (registered on both Element and generic Node)
    //
    // The wave-2/3 justification for the constant nulls here ("the parser
    // never builds a prefix→URI scope stack, so there is nothing to resolve
    // against") was wrong about what is *required*: a parse-time scope stack
    // is a convenience, not a precondition. `xml_parse` records every
    // `xmlns` / `xmlns:p` declaration as an ordinary attribute (Element slot
    // 1) and every element carries its parent (slot 4), so the lexical scope
    // DOM Level 2 asks for is fully reconstructible at query time by walking
    // ancestors — which is what `dom_lookup_namespace_uri` does. The three
    // accessors still move together; they are now consistently
    // namespace-AWARE instead of consistently unaware.
    for cls in ["org/w3c/dom/Node", "org/w3c/dom/Element"] {
        r.register(
            cls,
            "getNamespaceURI",
            "()Ljava/lang/String;",
            dom_node_namespace_uri,
        );
        r.register(
            cls,
            "getLocalName",
            "()Ljava/lang/String;",
            dom_node_local_name,
        );
        r.register(cls, "getPrefix", "()Ljava/lang/String;", dom_node_prefix);
    }

    // Text/CharacterData
    let text_cls = "org/w3c/dom/Text";
    r.register(text_cls, "getNodeType", "()S", |_ctx, _args| {
        Ok(Some(Value::Int(NODE_TEXT)))
    });
    r.register(
        text_cls,
        "getNodeName",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let s = ctx.create_string("#text");
            Ok(Some(Value::Object(Some(s))))
        },
    );
    r.register(
        text_cls,
        "getNodeValue",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(
        text_cls,
        "getTextContent",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(text_cls, "getData", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(text_cls, "getLength", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let len = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).map(|s| s.len()).unwrap_or(0),
            _ => 0,
        };
        Ok(Some(Value::Int(len as i32)))
    });
    let cd = "org/w3c/dom/CharacterData";
    r.register(cd, "getData", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });

    // Comment
    r.register(
        "org/w3c/dom/Comment",
        "getNodeType",
        "()S",
        |_ctx, _args| Ok(Some(Value::Int(NODE_COMMENT))),
    );

    // Attr
    let attr_cls = "org/w3c/dom/Attr";
    r.register(attr_cls, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(attr_cls, "getValue", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(attr_cls, "getNodeType", "()S", |_ctx, _args| {
        Ok(Some(Value::Int(NODE_ATTRIBUTE)))
    });
    r.register(
        attr_cls,
        "getNodeName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(
        attr_cls,
        "getNodeValue",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        },
    );

    // NodeList
    let nl_cls = "org/w3c/dom/NodeList";
    r.register(nl_cls, "item", "(I)Lorg/w3c/dom/Node;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as usize;
        let len = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        if idx >= len {
            return Ok(Some(Value::Object(None)));
        }
        if let Value::Object(Some(arr)) = ctx.get_field(this, 0) {
            Ok(Some(ctx.get_array_element(arr, idx)))
        } else {
            Ok(Some(Value::Object(None)))
        }
    });
    r.register(nl_cls, "getLength", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });

    // NamedNodeMap
    let nm_cls = "org/w3c/dom/NamedNodeMap";
    r.register(nm_cls, "item", "(I)Lorg/w3c/dom/Node;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as usize;
        if let Value::Object(Some(arr)) = ctx.get_field(this, 0) {
            if idx < ctx.array_length(arr) {
                return Ok(Some(ctx.get_array_element(arr, idx)));
            }
        }
        Ok(Some(Value::Object(None)))
    });
    r.register(nm_cls, "getLength", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(
        nm_cls,
        "getNamedItem",
        "(Ljava/lang/String;)Lorg/w3c/dom/Node;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => return Ok(Some(Value::Object(None))),
            };
            if let Value::Object(Some(arr)) = ctx.get_field(this, 0) {
                let len = ctx.array_length(arr);
                for i in 0..len {
                    if let Value::Object(Some(attr)) = ctx.get_array_element(arr, i) {
                        if let Value::Object(Some(n)) = ctx.get_field(attr, 0) {
                            if ctx.read_string(n).as_deref() == Some(&name) {
                                return Ok(Some(Value::Object(Some(attr))));
                            }
                        }
                    }
                }
            }
            Ok(Some(Value::Object(None)))
        },
    );

    // TransformerFactory
    let tf = "javax/xml/transform/TransformerFactory";
    r.register(
        tf,
        "newInstance",
        "()Ljavax/xml/transform/TransformerFactory;",
        |ctx, _args| {
            let obj =
                try_alloc_concurrent_synthetic(ctx, "javax/xml/transform/TransformerFactory", 0)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        tf,
        "newTransformer",
        "()Ljavax/xml/transform/Transformer;",
        |ctx, _args| {
            // One slot for the output-property map (filled lazily by
            // `setOutputProperty`); no stylesheet, so this is the identity
            // transformer, which is what `newTransformer()` means.
            let obj = try_alloc_concurrent_synthetic(ctx, "javax/xml/transform/Transformer", 1)?;
            ctx.set_field(obj, TRANSFORMER_PROPS, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // Transformer
    let tr = "javax/xml/transform/Transformer";
    r.register(
        tr,
        "setOutputProperty",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = args.get(1).copied().unwrap_or(Value::Object(None));
            let value = args.get(2).copied().unwrap_or(Value::Object(None));
            if ctx.object_num_fields(this) <= TRANSFORMER_PROPS {
                return Ok(None);
            }
            let props = match ctx.get_field(this, TRANSFORMER_PROPS) {
                Value::Object(Some(props)) => props,
                _ => {
                    // Pin across the map alloc/init below — a moving young GC
                    // there would relocate them (native stale-local family).
                    let this_pin = ctx.pin_native_root(this);
                    let name_pin = pinned_object_value(ctx, name);
                    let value_pin = pinned_object_value(ctx, value);
                    let props = try_alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3)?;
                    let props_pin = ctx.pin_native_root(props);
                    cratonvm_native_collections::native_map_init(
                        ctx,
                        &[Value::Object(Some(props))],
                    )
                    .ok();
                    let this = ctx.read_native_pin(this_pin, this);
                    let props = ctx.read_native_pin(props_pin, props);
                    ctx.set_field(this, TRANSFORMER_PROPS, Value::Object(Some(props)));
                    let name = read_pinned_object_value(ctx, name_pin, name);
                    let value = read_pinned_object_value(ctx, value_pin, value);
                    ctx.unpin_native_roots(this_pin);
                    cratonvm_native_collections::native_map_put_pub(
                        ctx,
                        &[Value::Object(Some(props)), name, value],
                    )?;
                    return Ok(None);
                }
            };
            cratonvm_native_collections::native_map_put_pub(
                ctx,
                &[Value::Object(Some(props)), name, value],
            )?;
            Ok(None)
        },
    );
    // `getOutputProperty` exists so a caller can read back what it set — the
    // serializer and the getter must agree on one store.
    r.register(
        tr,
        "getOutputProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if ctx.object_num_fields(this) <= TRANSFORMER_PROPS {
                return Ok(Some(Value::Object(None)));
            }
            let Value::Object(Some(props)) = ctx.get_field(this, TRANSFORMER_PROPS) else {
                return Ok(Some(Value::Object(None)));
            };
            let name = args.get(1).copied().unwrap_or(Value::Object(None));
            let found = cratonvm_native_collections::native_map_get_pub(
                ctx,
                &[Value::Object(Some(props)), name],
            )?;
            Ok(Some(found.unwrap_or(Value::Object(None))))
        },
    );
    r.register(
        tr,
        "transform",
        "(Ljavax/xml/transform/Source;Ljavax/xml/transform/Result;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let source = obj_arg(args, 1)?;
            let result = obj_arg(args, 2)?;
            // No stylesheet is ever attached here (`newTransformer()` is the
            // only factory), so this is the identity transform: serialize the
            // Source and hand the text to the Result. A real XSLT engine is
            // out of scope — see the unsupported-Source arm below, which
            // reports rather than silently emitting nothing.
            let this_pin = ctx.pin_native_root(this);
            let source_pin = ctx.pin_native_root(source);
            let result_pin = ctx.pin_native_root(result);
            let text = xslt_source_text(ctx, source);
            let this = ctx.read_native_pin(this_pin, this);
            let Ok(Some(body)) = text else {
                let source = ctx.read_native_pin(source_pin, source);
                let class_name = ctx
                    .class_name_of_id(ctx.class_id_of_object(source))
                    .unwrap_or_default();
                ctx.unpin_native_roots(this_pin);
                return Err(xslt_exception(
                    ctx,
                    &format!("unsupported or empty transform Source: {class_name}"),
                ));
            };
            // Each property read interns a key String, so re-read the pinned
            // receiver between them (native stale-local family).
            let indent = xslt_output_property(ctx, this, "indent")
                .filter(|v| v.eq_ignore_ascii_case("yes"))
                .map(|_| XSLT_INDENT_STEP);
            let this = ctx.read_native_pin(this_pin, this);
            let omit_declaration = xslt_output_property(ctx, this, "omit-xml-declaration")
                .is_some_and(|v| v.eq_ignore_ascii_case("yes"));
            let this = ctx.read_native_pin(this_pin, this);
            let encoding =
                xslt_output_property(ctx, this, "encoding").unwrap_or_else(|| "UTF-8".to_string());
            let mut out = String::new();
            if !omit_declaration {
                out.push_str(&format!("<?xml version=\"1.0\" encoding=\"{encoding}\"?>"));
                if indent.is_some() {
                    out.push('\n');
                }
            }
            out.push_str(&body);
            let result = ctx.read_native_pin(result_pin, result);
            let written = xslt_write_result(ctx, result, &out);
            let result = ctx.read_native_pin(result_pin, result);
            let class_name = ctx
                .class_name_of_id(ctx.class_id_of_object(result))
                .unwrap_or_default();
            ctx.unpin_native_roots(this_pin);
            if !written? {
                return Err(xslt_exception(
                    ctx,
                    &format!("unsupported transform Result: {class_name}"),
                ));
            }
            Ok(None)
        },
    );

    // XPath
    let xpf = "javax/xml/xpath/XPathFactory";
    r.register(
        xpf,
        "newInstance",
        "()Ljavax/xml/xpath/XPathFactory;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "javax/xml/xpath/XPathFactory", 0)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        xpf,
        "newXPath",
        "()Ljavax/xml/xpath/XPath;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "javax/xml/xpath/XPath", 0)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    let xp = "javax/xml/xpath/XPath";
    // There is no XPath engine behind these two. They used to answer `null`,
    // which is indistinguishable from a legitimate "expression matched
    // nothing" (for `evaluate`) and detonates as an NPE one call later at
    // `compile(expr).evaluate(doc)` — with a stack that points at the caller
    // rather than at the missing engine. Both methods declare `throws
    // XPathExpressionException`, so raising it is spec-legal and tells the
    // truth. NOTE: this converts calls that used to "succeed" with null into
    // a checked exception; see the wave-2 report.
    r.register(
        xp,
        "evaluate",
        "(Ljava/lang/String;Ljava/lang/Object;)Ljava/lang/String;",
        |ctx, _args| Err(xpath_unsupported(ctx, "XPath.evaluate")),
    );
    r.register(
        xp,
        "compile",
        "(Ljava/lang/String;)Ljavax/xml/xpath/XPathExpression;",
        |ctx, _args| Err(xpath_unsupported(ctx, "XPath.compile")),
    );
    r.set_category(__prev_cat);
}

// =============================================================================
// Phase J: Jackson ObjectMapper + Gson — Reflection-based JSON (M16)
// =============================================================================

/// Maximum recursion depth for JSON serialization/deserialization to prevent
/// stack overflow from deeply nested objects.
pub(crate) const JSON_MAX_DEPTH: usize = 64;

/// Maximum JSON input size in bytes (16 MB).
pub(crate) const JSON_MAX_INPUT_SIZE: usize = 16 * 1024 * 1024;

/// Maximum CAS retry iterations before yielding to prevent livelock.
pub(crate) const CAS_MAX_RETRIES: usize = 1024;

/// Check if a class has a @JsonIgnore annotation on a field.
pub(crate) fn field_has_json_ignore(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    field_name: &str,
) -> bool {
    let annotations = ctx.field_annotations(class_id, field_name);
    annotations.iter().any(|a| {
        a.type_descriptor.ends_with("JsonIgnore;") || a.type_descriptor.ends_with("Transient;")
    })
}

/// Get the @JsonProperty / @SerializedName override for a field, if any.
pub(crate) fn field_json_name(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    field_name: &str,
) -> Option<String> {
    let annotations = ctx.field_annotations(class_id, field_name);
    for ann in &annotations {
        if ann.type_descriptor.contains("JsonProperty")
            || ann.type_descriptor.contains("SerializedName")
        {
            // Try to extract annotation "value" element
            for (name, elem) in &ann.elements {
                if name == "value" {
                    if let cratonvm_native_api::AnnotationElementValue::StringVal(val) = elem {
                        if !val.is_empty() {
                            return Some(val.clone());
                        }
                    }
                }
            }
        }
    }
    None
}

/// Serialize a Java object to a JSON string using reflection.
/// Walks declared fields, reads each value, and builds a JSON object string.
pub(crate) fn reflection_serialize_to_json(ctx: &mut dyn NativeContext, obj: ObjectRef) -> String {
    reflection_serialize_to_json_depth(ctx, obj, 0)
}

pub(crate) fn reflection_serialize_to_json_depth(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    depth: usize,
) -> String {
    if depth > JSON_MAX_DEPTH {
        return "null".to_string();
    }

    let class_id = ctx.class_id_of_object(obj);
    let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();

    // For java.lang.String, just quote it
    if class_name == "java/lang/String" {
        let s = ctx.read_string(obj).unwrap_or_default();
        return format!("\"{}\"", json_escape(&s));
    }

    let fields = ctx.declared_fields(class_id);
    let mut entries = Vec::new();
    for meta in &fields {
        // Skip static fields
        if meta.is_static {
            continue;
        }
        // Skip @JsonIgnore / @Transient annotated fields
        if field_has_json_ignore(ctx, class_id, &meta.name) {
            continue;
        }
        let val = ctx.get_field(obj, meta.slot_index);
        let json_val = value_to_json_depth(ctx, &val, &meta.descriptor, depth + 1);
        // Use @JsonProperty/@SerializedName name if present
        let output_name =
            field_json_name(ctx, class_id, &meta.name).unwrap_or_else(|| meta.name.clone());
        entries.push(format!("\"{}\":{}", json_escape(&output_name), json_val));
    }
    format!("{{{}}}", entries.join(","))
}

/// Convert a JVM Value to its JSON representation (depth-limited).
pub(crate) fn value_to_json(ctx: &mut dyn NativeContext, val: &Value, descriptor: &str) -> String {
    value_to_json_depth(ctx, val, descriptor, 0)
}

pub(crate) fn value_to_json_depth(
    ctx: &mut dyn NativeContext,
    val: &Value,
    descriptor: &str,
    depth: usize,
) -> String {
    if depth > JSON_MAX_DEPTH {
        return "null".to_string();
    }
    match val {
        Value::Int(n) => match descriptor {
            "Z" => {
                if *n != 0 {
                    "true".to_string()
                } else {
                    "false".to_string()
                }
            }
            "C" => {
                let ch = char::from_u32(*n as u32).unwrap_or('?');
                format!("\"{}\"", json_escape(&ch.to_string()))
            }
            _ => n.to_string(),
        },
        Value::Long(n) => n.to_string(),
        Value::Float(f) => {
            if f.is_nan() {
                "null".to_string()
            } else if f.is_infinite() {
                "null".to_string()
            } else {
                format!("{}", f)
            }
        }
        Value::Double(d) => {
            if d.is_nan() {
                "null".to_string()
            } else if d.is_infinite() {
                "null".to_string()
            } else {
                format!("{}", d)
            }
        }
        Value::Object(None) => "null".to_string(),
        Value::Object(Some(obj_ref)) => {
            let cid = ctx.class_id_of_object(*obj_ref);
            let cname = ctx.class_name_of_id(cid).unwrap_or_default();
            if cname == "java/lang/String" {
                let s = ctx.read_string(*obj_ref).unwrap_or_default();
                format!("\"{}\"", json_escape(&s))
            } else if cname.starts_with("[") || descriptor.starts_with("[") {
                // Array — serialize elements
                let len = ctx.array_length(*obj_ref);
                let mut elems = Vec::new();
                let elem_desc = if descriptor.len() > 1 {
                    &descriptor[1..]
                } else {
                    "I"
                };
                for i in 0..len {
                    let elem = ctx.get_array_element(*obj_ref, i);
                    elems.push(value_to_json_depth(ctx, &elem, elem_desc, depth + 1));
                }
                format!("[{}]", elems.join(","))
            } else if cname == "java/lang/Integer"
                || cname == "java/lang/Byte"
                || cname == "java/lang/Short"
            {
                let inner = ctx.get_field(*obj_ref, 0);
                value_to_json_depth(ctx, &inner, "I", depth + 1)
            } else if cname == "java/lang/Long" {
                let inner = ctx.get_field(*obj_ref, 0);
                value_to_json_depth(ctx, &inner, "J", depth + 1)
            } else if cname == "java/lang/Float" {
                let inner = ctx.get_field(*obj_ref, 0);
                value_to_json_depth(ctx, &inner, "F", depth + 1)
            } else if cname == "java/lang/Double" {
                let inner = ctx.get_field(*obj_ref, 0);
                value_to_json_depth(ctx, &inner, "D", depth + 1)
            } else if cname == "java/lang/Boolean" {
                let inner = ctx.get_field(*obj_ref, 0);
                value_to_json_depth(ctx, &inner, "Z", depth + 1)
            } else {
                // Nested object — recurse (depth-limited)
                reflection_serialize_to_json_depth(ctx, *obj_ref, depth + 1)
            }
        }
        _ => "null".to_string(),
    }
}

/// Escape a string for JSON (handles all control chars and Unicode).
pub(crate) fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0C' => out.push_str("\\f"),
            c if c < '\x20' => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// Read exactly four hex digits as a u16 from the char iterator, advancing it.
/// Returns None if fewer than four hex digits are available (malformed `\u`).
pub(crate) fn json_read_u16_hex(chars: &mut std::str::Chars<'_>) -> Option<u16> {
    // A `\u` escape is a fixed 4-char window. On a malformed escape we must
    // still CONSUME the full window (up to end-of-input) so the caller emits a
    // single replacement char rather than leaving the trailing chars behind
    // (e.g. `\uZZZZ` → "\u{FFFD}", not "\u{FFFD}ZZZ"). Only a genuine
    // truncation (end-of-input before 4 chars) returns early — there is nothing
    // left to consume.
    let mut code: u16 = 0;
    let mut valid = true;
    for _ in 0..4 {
        let c = chars.next()?; // None == real EOF: nothing more to consume
        match c.to_digit(16) {
            Some(d) => code = code.wrapping_shl(4) | (d as u16),
            None => valid = false, // consume the char, but mark the escape malformed
        }
    }
    if valid {
        Some(code)
    } else {
        None
    }
}

/// Unescape a JSON string value (handles \\n, \\t, \\uXXXX, etc.).
///
/// FIX (finding 1): correctly decode UTF-16 surrogate PAIRS. A `😀`
/// sequence is two escapes that together form one astral code point (U+1F600).
/// The previous code fed each half to `char::from_u32`, which returns `None` for
/// any surrogate (0xD800..=0xDFFF), so emoji and other supplementary-plane chars
/// were SILENTLY DROPPED. We now combine a high surrogate with a following low
/// surrogate; unpaired/invalid surrogates become U+FFFD instead of vanishing.
/// Operates purely on `char`s, so it is inherently UTF-8-boundary safe and never
/// panics on malformed multi-byte input.
pub(crate) fn json_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('/') => out.push('/'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('b') => out.push('\x08'),
                Some('f') => out.push('\x0C'),
                Some('u') => {
                    match json_read_u16_hex(&mut chars) {
                        Some(unit) => {
                            if (0xD800..=0xDBFF).contains(&unit) {
                                // High surrogate: try to consume a following `\uDCxx` low surrogate.
                                let mut lookahead = chars.clone();
                                let low = if lookahead.next() == Some('\\')
                                    && lookahead.next() == Some('u')
                                {
                                    json_read_u16_hex(&mut lookahead)
                                } else {
                                    None
                                };
                                match low {
                                    Some(lo) if (0xDC00..=0xDFFF).contains(&lo) => {
                                        // Valid surrogate pair → astral code point.
                                        let cp = 0x10000
                                            + (((unit as u32) - 0xD800) << 10)
                                            + ((lo as u32) - 0xDC00);
                                        // SAFETY: cp is in 0x10000..=0x10FFFF, always a valid char.
                                        out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                                        chars = lookahead; // consume the low-surrogate escape
                                    }
                                    _ => {
                                        // Unpaired high surrogate → replacement char.
                                        out.push('\u{FFFD}');
                                    }
                                }
                            } else if (0xDC00..=0xDFFF).contains(&unit) {
                                // Unpaired low surrogate → replacement char.
                                out.push('\u{FFFD}');
                            } else {
                                // BMP scalar value (never a surrogate here).
                                out.push(char::from_u32(unit as u32).unwrap_or('\u{FFFD}'));
                            }
                        }
                        // Malformed `\u` with fewer than 4 hex digits: emit replacement
                        // rather than panicking or silently dropping.
                        None => out.push('\u{FFFD}'),
                    }
                }
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Number of bytes occupied by the UTF-8 character whose leading byte sits at
/// `bytes[i]`. Returns at least 1, and clamps so the result never runs past the
/// slice end.
///
/// FIX (finding 1): the byte-level scanners below skip an *escaped* character by
/// advancing past the backslash and then over the escaped char. If that char is
/// multi-byte UTF-8 (e.g. `\é`, `\😀`), a blind `+= 2` lands the cursor in the
/// MIDDLE of a UTF-8 sequence; the later `&s[start..i]` slice then panics with
/// "byte index N is not a char boundary". Skipping the full UTF-8 width keeps the
/// cursor on a char boundary so malformed input degrades gracefully (never panics).
#[inline]
pub(crate) fn json_utf8_char_len(bytes: &[u8], i: usize) -> usize {
    if i >= bytes.len() {
        return 0;
    }
    let b = bytes[i];
    let n = if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        // Continuation/invalid leading byte: advance one byte to make progress.
        1
    };
    n.min(bytes.len() - i)
}

/// Simple JSON tokenizer for deserialization.
/// Parses a JSON object string into (key, value_str) pairs.
/// Enforces size limits and handles escaped quotes correctly.
pub(crate) fn parse_json_object(json: &str) -> Vec<(String, String)> {
    if json.len() > JSON_MAX_INPUT_SIZE {
        return Vec::new();
    }
    let json = json.trim();
    if json.len() < 2 || !json.starts_with('{') || !json.ends_with('}') {
        return Vec::new();
    }
    let inner = &json[1..json.len() - 1];
    let mut result = Vec::new();
    let mut i = 0;
    let bytes = inner.as_bytes();
    while i < bytes.len() {
        // Skip whitespace and commas
        while i < bytes.len()
            && (bytes[i] == b' '
                || bytes[i] == b','
                || bytes[i] == b'\n'
                || bytes[i] == b'\r'
                || bytes[i] == b'\t')
        {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }

        // Expect key string
        if bytes[i] != b'"' {
            break;
        }
        i += 1;
        let key_start = i;
        while i < bytes.len() {
            if bytes[i] == b'\\' && i + 1 < bytes.len() {
                // Skip the backslash, then the FULL escaped char (UTF-8 width-aware,
                // finding 1) so `i` stays on a char boundary even for `\é`, `\😀`, etc.
                i += 1 + json_utf8_char_len(bytes, i + 1);
            } else if bytes[i] == b'"' {
                break;
            } else {
                i += json_utf8_char_len(bytes, i);
            }
        }
        if i >= bytes.len() {
            break;
        } // unterminated string
        let raw_key = &inner[key_start..i.min(inner.len())];
        let key = json_unescape(raw_key);
        i += 1; // skip closing quote

        // Skip colon and whitespace
        while i < bytes.len() && (bytes[i] == b':' || bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }

        // Read value
        let val = read_json_value(inner, &mut i);
        result.push((key, val));
    }
    result
}

/// Simple JSON pretty-printer: add newlines and indentation to compact JSON.
pub(crate) fn json_pretty_print(json: &str) -> String {
    let mut out = String::with_capacity(json.len() * 2);
    let mut indent = 0usize;
    let mut in_string = false;
    let mut prev = 0u8;
    let bytes = json.as_bytes();
    for &b in bytes {
        if in_string {
            out.push(b as char);
            if b == b'"' && prev != b'\\' {
                in_string = false;
            }
            prev = b;
            continue;
        }
        match b {
            b'"' => {
                in_string = true;
                out.push(b as char);
            }
            b'{' | b'[' => {
                out.push(b as char);
                indent += 2;
                out.push('\n');
                for _ in 0..indent {
                    out.push(' ');
                }
            }
            b'}' | b']' => {
                indent = indent.saturating_sub(2);
                out.push('\n');
                for _ in 0..indent {
                    out.push(' ');
                }
                out.push(b as char);
            }
            b',' => {
                out.push(',');
                out.push('\n');
                for _ in 0..indent {
                    out.push(' ');
                }
            }
            b':' => {
                out.push_str(" : ");
            }
            b' ' | b'\n' | b'\r' | b'\t' => {} // skip existing whitespace
            _ => out.push(b as char),
        }
        prev = b;
    }
    out
}

/// Read a single JSON value starting at position i.
/// Handles strings with escaped quotes, nested objects/arrays with string awareness.
pub(crate) fn read_json_value(s: &str, i: &mut usize) -> String {
    let bytes = s.as_bytes();
    if *i >= bytes.len() {
        return String::new();
    }

    match bytes[*i] {
        b'"' => {
            *i += 1;
            let start = *i;
            while *i < bytes.len() {
                if bytes[*i] == b'\\' && *i + 1 < bytes.len() {
                    // Skip backslash + full escaped char (UTF-8-width aware, finding 1)
                    // so `*i` stays on a char boundary for the slice below.
                    *i += 1 + json_utf8_char_len(bytes, *i + 1);
                } else if bytes[*i] == b'"' {
                    break;
                } else {
                    *i += json_utf8_char_len(bytes, *i);
                }
            }
            let val = json_unescape(&s[start..(*i).min(bytes.len())]);
            if *i < bytes.len() {
                *i += 1;
            } // skip closing quote
            val
        }
        b'{' => {
            let start = *i;
            let mut depth = 0i32;
            let mut in_string = false;
            while *i < bytes.len() {
                if in_string {
                    // UTF-8-width-aware escape skip keeps `*i` on a char boundary (finding 1).
                    if bytes[*i] == b'\\' && *i + 1 < bytes.len() {
                        *i += 1 + json_utf8_char_len(bytes, *i + 1);
                        continue;
                    }
                    if bytes[*i] == b'"' {
                        in_string = false;
                    }
                } else {
                    match bytes[*i] {
                        b'"' => in_string = true,
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                *i += 1;
                                return s[start..(*i).min(bytes.len())].to_string();
                            }
                        }
                        _ => {}
                    }
                }
                *i += json_utf8_char_len(bytes, *i);
            }
            s[start..(*i).min(bytes.len())].to_string()
        }
        b'[' => {
            let start = *i;
            let mut depth = 0i32;
            let mut in_string = false;
            while *i < bytes.len() {
                if in_string {
                    // UTF-8-width-aware escape skip keeps `*i` on a char boundary (finding 1).
                    if bytes[*i] == b'\\' && *i + 1 < bytes.len() {
                        *i += 1 + json_utf8_char_len(bytes, *i + 1);
                        continue;
                    }
                    if bytes[*i] == b'"' {
                        in_string = false;
                    }
                } else {
                    match bytes[*i] {
                        b'"' => in_string = true,
                        b'[' => depth += 1,
                        b']' => {
                            depth -= 1;
                            if depth == 0 {
                                *i += 1;
                                return s[start..(*i).min(bytes.len())].to_string();
                            }
                        }
                        _ => {}
                    }
                }
                *i += json_utf8_char_len(bytes, *i);
            }
            s[start..(*i).min(bytes.len())].to_string()
        }
        _ => {
            // Number, boolean, null
            let start = *i;
            while *i < bytes.len()
                && bytes[*i] != b','
                && bytes[*i] != b'}'
                && bytes[*i] != b']'
                && bytes[*i] != b' '
                && bytes[*i] != b'\n'
                && bytes[*i] != b'\r'
                && bytes[*i] != b'\t'
            {
                *i += 1;
            }
            s[start..*i].to_string()
        }
    }
}

/// Deserialize a JSON string into a Java object using reflection (depth-limited).
pub(crate) fn reflection_deserialize_from_json(
    ctx: &mut dyn NativeContext,
    json: &str,
    class_id: ClassId,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    Ok(reflection_deserialize_from_json_depth(
        ctx, json, class_id, 0,
    )?)
}

pub(crate) fn reflection_deserialize_from_json_depth(
    ctx: &mut dyn NativeContext,
    json: &str,
    class_id: ClassId,
    depth: usize,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    if depth > JSON_MAX_DEPTH || json.len() > JSON_MAX_INPUT_SIZE {
        return Ok(None);
    }
    let fields = ctx.declared_fields(class_id);
    let instance_fields: Vec<_> = fields.into_iter().filter(|f| !f.is_static).collect();
    let obj = ctx.alloc_object(class_id, instance_fields.len());
    let pairs = parse_json_object(json);

    for (key, val_str) in &pairs {
        // Match by field name or by @JsonProperty/@SerializedName alias
        let meta = instance_fields.iter().find(|f| {
            if f.name == *key {
                return true;
            }
            if let Some(alias) = field_json_name(ctx, class_id, &f.name) {
                return alias == *key;
            }
            false
        });
        if let Some(meta) = meta {
            if field_has_json_ignore(ctx, class_id, &meta.name) {
                continue;
            }
            let value = json_str_to_value_depth(ctx, val_str, &meta.descriptor, depth + 1);
            ctx.set_field(obj, meta.slot_index, value?);
        }
    }
    Ok(Some(obj))
}

/// Convert a JSON value string to a JVM Value based on the field descriptor.
pub(crate) fn json_str_to_value(
    ctx: &mut dyn NativeContext,
    val_str: &str,
    descriptor: &str,
) -> Result<Value, MethodCallFailed> {
    Ok(json_str_to_value_depth(ctx, val_str, descriptor, 0)?)
}

pub(crate) fn json_str_to_value_depth(
    ctx: &mut dyn NativeContext,
    val_str: &str,
    descriptor: &str,
    depth: usize,
) -> Result<Value, MethodCallFailed> {
    if depth > JSON_MAX_DEPTH {
        return Ok(Value::Object(None));
    }
    let val_str = val_str.trim();
    if val_str == "null" {
        return match descriptor {
            "I" | "B" | "S" | "C" | "Z" => Ok(Value::Int(0)),
            "J" => Ok(Value::Long(0)),
            "F" => Ok(Value::Float(0.0)),
            "D" => Ok(Value::Double(0.0)),
            _ => Ok(Value::Object(None)),
        };
    }
    match descriptor {
        "I" | "B" | "S" => Ok(Value::Int(val_str.parse::<i32>().unwrap_or(0))),
        "J" => Ok(Value::Long(val_str.parse::<i64>().unwrap_or(0))),
        "F" => Ok(Value::Float(val_str.parse::<f32>().unwrap_or(0.0))),
        "D" => Ok(Value::Double(val_str.parse::<f64>().unwrap_or(0.0))),
        "Z" => Ok(Value::Int(if val_str == "true" { 1 } else { 0 })),
        "C" => {
            let ch = val_str.chars().next().unwrap_or('\0') as i32;
            Ok(Value::Int(ch))
        }
        desc if desc.starts_with("L") && desc.ends_with(";") => {
            let class_name = &desc[1..desc.len() - 1];
            if class_name == "java/lang/String" {
                let s = ctx.create_string(val_str);
                Ok(Value::Object(Some(s)))
            } else if class_name == "java/lang/Integer" {
                let n = val_str.parse::<i32>().unwrap_or(0);
                let boxed = try_alloc_concurrent_synthetic(ctx, "java/lang/Integer", 1)?;
                ctx.set_field(boxed, 0, Value::Int(n));
                Ok(Value::Object(Some(boxed)))
            } else if class_name == "java/lang/Long" {
                let n = val_str.parse::<i64>().unwrap_or(0);
                let boxed = try_alloc_concurrent_synthetic(ctx, "java/lang/Long", 1)?;
                ctx.set_field(boxed, 0, Value::Long(n));
                Ok(Value::Object(Some(boxed)))
            } else if class_name == "java/lang/Float" {
                let n = val_str.parse::<f32>().unwrap_or(0.0);
                let boxed = try_alloc_concurrent_synthetic(ctx, "java/lang/Float", 1)?;
                ctx.set_field(boxed, 0, Value::Float(n));
                Ok(Value::Object(Some(boxed)))
            } else if class_name == "java/lang/Double" {
                let n = val_str.parse::<f64>().unwrap_or(0.0);
                let boxed = try_alloc_concurrent_synthetic(ctx, "java/lang/Double", 1)?;
                ctx.set_field(boxed, 0, Value::Double(n));
                Ok(Value::Object(Some(boxed)))
            } else if class_name == "java/lang/Boolean" {
                let b = if val_str == "true" { 1 } else { 0 };
                let boxed = try_alloc_concurrent_synthetic(ctx, "java/lang/Boolean", 1)?;
                ctx.set_field(boxed, 0, Value::Int(b));
                Ok(Value::Object(Some(boxed)))
            } else if val_str.starts_with("{") {
                // Nested object — depth-limited deserialization
                if let Some(cid) = ctx.class_id_by_name(class_name) {
                    match reflection_deserialize_from_json_depth(ctx, val_str, cid, depth + 1)? {
                        Some(nested) => Ok(Value::Object(Some(nested))),
                        None => Ok(Value::Object(None)),
                    }
                } else {
                    Ok(Value::Object(None))
                }
            } else {
                Ok(Value::Object(None))
            }
        }
        desc if desc.starts_with("[") => {
            // Array field — parse JSON array
            let elem_desc = &desc[1..];
            let trimmed = val_str.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                let inner = &trimmed[1..trimmed.len() - 1];
                // Parse array elements
                let mut elements = Vec::new();
                let mut i = 0;
                while i < inner.len() {
                    // Skip whitespace and commas
                    while i < inner.len() {
                        let b = inner.as_bytes()[i];
                        if b == b' ' || b == b',' || b == b'\n' || b == b'\r' || b == b'\t' {
                            i += 1;
                        } else {
                            break;
                        }
                    }
                    if i >= inner.len() {
                        break;
                    }
                    let val = read_json_value(inner, &mut i);
                    if val.is_empty() && i >= inner.len() {
                        break;
                    }
                    elements.push(val);
                }
                let arr_type = match elem_desc {
                    "I" | "B" | "S" | "C" | "Z" => cratonvm_types::ArrayElementType::Int,
                    "J" => cratonvm_types::ArrayElementType::Long,
                    "F" => cratonvm_types::ArrayElementType::Float,
                    "D" => cratonvm_types::ArrayElementType::Double,
                    _ => cratonvm_types::ArrayElementType::Reference,
                };
                let arr = ctx.new_array(arr_type, elements.len());
                for (idx, elem_str) in elements.iter().enumerate() {
                    let elem_val = json_str_to_value_depth(ctx, elem_str, elem_desc, depth + 1);
                    ctx.set_array_element(arr, idx, elem_val?);
                }
                Ok(Value::Object(Some(arr)))
            } else {
                Ok(Value::Object(None))
            }
        }
        _ => Ok(Value::Object(None)),
    }
}

pub(crate) fn register_jackson_gson_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // -----------------------------------------------------------------------
    // Jackson: com/fasterxml/jackson/databind/ObjectMapper
    // -----------------------------------------------------------------------
    let om = "com/fasterxml/jackson/databind/ObjectMapper";

    // ObjectMapper config bitfield (stored in field 0 as Int):
    //   bit 0: FAIL_ON_UNKNOWN_PROPERTIES (default: 1 = enabled)
    //   bit 1: INDENT_OUTPUT (default: 0 = disabled)
    //   bit 2: WRITE_DATES_AS_TIMESTAMPS (default: 1 = enabled)
    //   bit 3: FAIL_ON_NULL_FOR_PRIMITIVES (default: 0 = disabled)
    //   bit 4: SERIALIZE_NULLS (default: 0 = disabled; for Gson compat)
    const OM_CFG_FAIL_UNKNOWN: i32 = 1 << 0;
    const OM_CFG_INDENT: i32 = 1 << 1;
    const OM_CFG_DATES_TIMESTAMPS: i32 = 1 << 2;
    const OM_CFG_FAIL_NULL_PRIM: i32 = 1 << 3;
    const OM_CFG_FIELD: usize = 0;

    // Default config: FAIL_ON_UNKNOWN_PROPERTIES | WRITE_DATES_AS_TIMESTAMPS
    const OM_DEFAULT_CFG: i32 = OM_CFG_FAIL_UNKNOWN | OM_CFG_DATES_TIMESTAMPS;

    r.register(om, "<init>", "()V", |ctx, args| {
        if let Some(Value::Object(Some(this))) = args.first() {
            ctx.set_field(*this, OM_CFG_FIELD, Value::Int(OM_DEFAULT_CFG));
        }
        Ok(None)
    });

    // writeValueAsString(Object) -> String
    r.register(
        om,
        "writeValueAsString",
        "(Ljava/lang/Object;)Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    let null_str = ctx.create_string("null");
                    return Ok(Some(Value::Object(Some(null_str))));
                }
            };
            let obj = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    let null_str = ctx.create_string("null");
                    return Ok(Some(Value::Object(Some(null_str))));
                }
            };
            let cfg = match ctx.get_field(this, OM_CFG_FIELD) {
                Value::Int(v) => v,
                _ => OM_DEFAULT_CFG,
            };
            let indent = (cfg & OM_CFG_INDENT) != 0;
            let json = reflection_serialize_to_json(ctx, obj);
            let output = if indent {
                json_pretty_print(&json)
            } else {
                json
            };
            let result = ctx.create_string(&output);
            Ok(Some(Value::Object(Some(result))))
        },
    );

    // writeValueAsBytes(Object) -> byte[]
    r.register(
        om,
        "writeValueAsBytes",
        "(Ljava/lang/Object;)[B",
        |ctx, args| {
            let obj = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 0);
                    return Ok(Some(Value::Object(Some(arr))));
                }
            };
            let json = reflection_serialize_to_json(ctx, obj);
            let bytes = json.as_bytes();
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
            for (i, &b) in bytes.iter().enumerate() {
                ctx.set_array_element(arr, i, Value::Int(b as i32));
            }
            Ok(Some(Value::Object(Some(arr))))
        },
    );

    // readValue(String, Class) -> Object
    r.register(
        om,
        "readValue",
        "(Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/Object;",
        |ctx, args| {
            let json_ref = match args.get(1) {
                Some(Value::Object(Some(s))) => *s,
                _ => return Ok(Some(Value::Object(None))),
            };
            let class_mirror = match args.get(2) {
                Some(Value::Object(Some(c))) => *c,
                _ => return Ok(Some(Value::Object(None))),
            };
            let json = match ctx.read_string(json_ref) {
                Some(s) => s,
                None => return Ok(Some(Value::Object(None))),
            };
            let class_id = match crate::lang_class::mirror_class_id(ctx, class_mirror) {
                Some(id) => id,
                None => return Ok(Some(Value::Object(None))),
            };
            match reflection_deserialize_from_json(ctx, &json, class_id)? {
                Some(obj) => Ok(Some(Value::Object(Some(obj)))),
                None => Ok(Some(Value::Object(None))),
            }
        },
    );

    // Helper: resolve an enum name from an ObjectRef (field 0 is the name string)
    fn om_enum_name(ctx: &mut dyn NativeContext, enum_ref: ObjectRef) -> Option<String> {
        if let Value::Object(Some(name_ref)) = ctx.get_field(enum_ref, 0) {
            ctx.read_string(name_ref)
        } else {
            None
        }
    }

    /// Map a Jackson feature enum name to its config bit.
    fn om_feature_bit(name: &str) -> Result<Option<i32>, MethodCallFailed> {
        match name {
            "FAIL_ON_UNKNOWN_PROPERTIES" => Ok(Some(OM_CFG_FAIL_UNKNOWN)),
            "INDENT_OUTPUT" | "WRITE_INDENTED" => Ok(Some(OM_CFG_INDENT)),
            "WRITE_DATES_AS_TIMESTAMPS" => Ok(Some(OM_CFG_DATES_TIMESTAMPS)),
            "FAIL_ON_NULL_FOR_PRIMITIVES" => Ok(Some(OM_CFG_FAIL_NULL_PRIM)),
            _ => Ok(None),
        }
    }

    // configure(Feature, boolean) — set or clear a config bit
    r.register(
        om,
        "configure",
        "(Ljava/lang/Enum;Z)Lcom/fasterxml/jackson/databind/ObjectMapper;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let enabled = matches!(args.get(2), Some(Value::Int(v)) if *v != 0);
            if let Some(Value::Object(Some(enum_ref))) = args.get(1) {
                if let Some(name) = om_enum_name(ctx, *enum_ref) {
                    if let Ok(Some(bit)) = om_feature_bit(&name) {
                        let cfg = match ctx.get_field(this, OM_CFG_FIELD) {
                            Value::Int(v) => v,
                            _ => OM_DEFAULT_CFG,
                        };
                        let new_cfg = if enabled { cfg | bit } else { cfg & !bit };
                        ctx.set_field(this, OM_CFG_FIELD, Value::Int(new_cfg));
                    }
                }
            }
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // enable(Feature) — set a config bit
    r.register(
        om,
        "enable",
        "(Ljava/lang/Enum;)Lcom/fasterxml/jackson/databind/ObjectMapper;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            if let Some(Value::Object(Some(enum_ref))) = args.get(1) {
                if let Some(name) = om_enum_name(ctx, *enum_ref) {
                    if let Ok(Some(bit)) = om_feature_bit(&name) {
                        let cfg = match ctx.get_field(this, OM_CFG_FIELD) {
                            Value::Int(v) => v,
                            _ => OM_DEFAULT_CFG,
                        };
                        ctx.set_field(this, OM_CFG_FIELD, Value::Int(cfg | bit));
                    }
                }
            }
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // disable(Feature) — clear a config bit
    r.register(
        om,
        "disable",
        "(Ljava/lang/Enum;)Lcom/fasterxml/jackson/databind/ObjectMapper;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            if let Some(Value::Object(Some(enum_ref))) = args.get(1) {
                if let Some(name) = om_enum_name(ctx, *enum_ref) {
                    if let Ok(Some(bit)) = om_feature_bit(&name) {
                        let cfg = match ctx.get_field(this, OM_CFG_FIELD) {
                            Value::Int(v) => v,
                            _ => OM_DEFAULT_CFG,
                        };
                        ctx.set_field(this, OM_CFG_FIELD, Value::Int(cfg & !bit));
                    }
                }
            }
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // setSerializationInclusion — return this (no direct config bit)
    r.register(
        om,
        "setSerializationInclusion",
        "(Ljava/lang/Enum;)Lcom/fasterxml/jackson/databind/ObjectMapper;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );

    // registerModule — return this (modules not supported but don't crash)
    r.register(
        om,
        "registerModule",
        "(Lcom/fasterxml/jackson/databind/Module;)Lcom/fasterxml/jackson/databind/ObjectMapper;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );

    // -----------------------------------------------------------------------
    // Gson: com/google/gson/Gson
    // -----------------------------------------------------------------------
    let gson = "com/google/gson/Gson";

    // Gson config bitfield (stored in field 0 as Int):
    //   bit 0: prettyPrinting (default: 0)
    //   bit 1: serializeNulls (default: 0)
    //   bit 2: disableHtmlEscaping (default: 0)
    const GSON_CFG_PRETTY: i32 = 1 << 0;
    const GSON_CFG_SERIALIZE_NULLS: i32 = 1 << 1;
    const GSON_CFG_NO_HTML_ESCAPE: i32 = 1 << 2;
    const GSON_CFG_FIELD: usize = 0;

    r.register(gson, "<init>", "()V", |ctx, args| {
        if let Some(Value::Object(Some(this))) = args.first() {
            ctx.set_field(*this, GSON_CFG_FIELD, Value::Int(0)); // default: all off
        }
        Ok(None)
    });

    // toJson(Object) -> String (respects prettyPrinting)
    r.register(
        gson,
        "toJson",
        "(Ljava/lang/Object;)Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    let null_str = ctx.create_string("null");
                    return Ok(Some(Value::Object(Some(null_str))));
                }
            };
            let obj = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    let null_str = ctx.create_string("null");
                    return Ok(Some(Value::Object(Some(null_str))));
                }
            };
            let cfg = match ctx.get_field(this, GSON_CFG_FIELD) {
                Value::Int(v) => v,
                _ => 0,
            };
            let json = reflection_serialize_to_json(ctx, obj);
            let output = if (cfg & GSON_CFG_PRETTY) != 0 {
                json_pretty_print(&json)
            } else {
                json
            };
            let result = ctx.create_string(&output);
            Ok(Some(Value::Object(Some(result))))
        },
    );

    // fromJson(String, Class) -> Object
    r.register(
        gson,
        "fromJson",
        "(Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/Object;",
        |ctx, args| {
            let json_ref = match args.get(1) {
                Some(Value::Object(Some(s))) => *s,
                _ => return Ok(Some(Value::Object(None))),
            };
            let class_mirror = match args.get(2) {
                Some(Value::Object(Some(c))) => *c,
                _ => return Ok(Some(Value::Object(None))),
            };
            let json = match ctx.read_string(json_ref) {
                Some(s) => s,
                None => return Ok(Some(Value::Object(None))),
            };
            let class_id = match crate::lang_class::mirror_class_id(ctx, class_mirror) {
                Some(id) => id,
                None => return Ok(Some(Value::Object(None))),
            };
            match reflection_deserialize_from_json(ctx, &json, class_id)? {
                Some(obj) => Ok(Some(Value::Object(Some(obj)))),
                None => Ok(Some(Value::Object(None))),
            }
        },
    );

    // GsonBuilder — field 0 stores config bitfield (same bits as Gson)
    let gsb = "com/google/gson/GsonBuilder";
    r.register(gsb, "<init>", "()V", |ctx, args| {
        if let Some(Value::Object(Some(this))) = args.first() {
            ctx.set_field(*this, 0, Value::Int(0));
        }
        Ok(None)
    });
    r.register(gsb, "create", "()Lcom/google/gson/Gson;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => {
                let g = try_alloc_concurrent_synthetic(ctx, "com/google/gson/Gson", 1)?;
                return Ok(Some(Value::Object(Some(g))));
            }
        };
        let cfg = match ctx.get_field(this, 0) {
            Value::Int(v) => v,
            _ => 0,
        };
        let g = try_alloc_concurrent_synthetic(ctx, "com/google/gson/Gson", 1)?;
        ctx.set_field(g, GSON_CFG_FIELD, Value::Int(cfg));
        Ok(Some(Value::Object(Some(g))))
    });
    r.register(
        gsb,
        "setPrettyPrinting",
        "()Lcom/google/gson/GsonBuilder;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let cfg = match ctx.get_field(this, 0) {
                Value::Int(v) => v,
                _ => 0,
            };
            ctx.set_field(this, 0, Value::Int(cfg | GSON_CFG_PRETTY));
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        gsb,
        "serializeNulls",
        "()Lcom/google/gson/GsonBuilder;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let cfg = match ctx.get_field(this, 0) {
                Value::Int(v) => v,
                _ => 0,
            };
            ctx.set_field(this, 0, Value::Int(cfg | GSON_CFG_SERIALIZE_NULLS));
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        gsb,
        "disableHtmlEscaping",
        "()Lcom/google/gson/GsonBuilder;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let cfg = match ctx.get_field(this, 0) {
                Value::Int(v) => v,
                _ => 0,
            };
            ctx.set_field(this, 0, Value::Int(cfg | GSON_CFG_NO_HTML_ESCAPE));
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        gsb,
        "setDateFormat",
        "(Ljava/lang/String;)Lcom/google/gson/GsonBuilder;",
        |_ctx, args| {
            // Date format not yet supported, but store for future use
            Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
        },
    );

    // -----------------------------------------------------------------------
    // ObjectMapper.readTree(String) -> JsonNode
    // -----------------------------------------------------------------------
    r.register(
        om,
        "readTree",
        "(Ljava/lang/String;)Lcom/fasterxml/jackson/databind/JsonNode;",
        |ctx, args| {
            let json_ref = match args.get(1) {
                Some(Value::Object(Some(s))) => *s,
                _ => return Ok(Some(Value::Object(None))),
            };
            let json = match ctx.read_string(json_ref) {
                Some(s) => s,
                None => return Ok(Some(Value::Object(None))),
            };
            let node = build_json_tree_node(ctx, &json)?;
            Ok(Some(Value::Object(Some(node))))
        },
    );

    // -----------------------------------------------------------------------
    // Jackson JsonNode / ObjectNode (tree model) — backed by synthetic fields
    //
    // Layout (8 fields):
    //   0: type  (Int: 0=null, 1=object, 2=array, 3=string, 4=number, 5=boolean, -1=missing)
    //   1: text_value (Object: String for string/number nodes, null otherwise)
    //   2: num_value  (Long: parsed integer for number nodes)
    //   3: bool_value (Int: 0/1 for boolean nodes)
    //   4: children   (Object: array of JsonNode for object/array children)
    //   5: keys       (Object: array of Strings — child key names for object nodes)
    //   6: child_count (Int: number of children)
    //   7: dbl_value  (Double: parsed float/double for number nodes — preserves precision)
    // -----------------------------------------------------------------------
    const JN: &str = "com/fasterxml/jackson/databind/JsonNode";
    const JN_TYPE: usize = 0;
    const JN_TEXT: usize = 1;
    const JN_NUM: usize = 2;
    const JN_BOOL: usize = 3;
    const JN_CHILDREN: usize = 4;
    const JN_KEYS: usize = 5;
    const JN_COUNT: usize = 6;
    const JN_DBL: usize = 7;

    r.register(JN, "asText", "()Ljava/lang/String;", |ctx, args| {
        let this = crate::obj_arg(args, 0)?;
        let ntype = match ctx.get_field(this, JN_TYPE) {
            Value::Int(v) => v,
            _ => 0,
        };
        if ntype == 3 {
            // string node
            Ok(Some(ctx.get_field(this, JN_TEXT)))
        } else if ntype == 4 {
            // number node
            let text_val = ctx.get_field(this, JN_TEXT);
            if let Value::Object(Some(_)) = text_val {
                Ok(Some(text_val))
            } else {
                let n = match ctx.get_field(this, JN_NUM) {
                    Value::Long(v) => v,
                    _ => 0,
                };
                let s = ctx.create_string(&n.to_string());
                Ok(Some(Value::Object(Some(s))))
            }
        } else if ntype == 5 {
            // boolean
            let b = match ctx.get_field(this, JN_BOOL) {
                Value::Int(v) => v,
                _ => 0,
            };
            let s = ctx.create_string(if b != 0 { "true" } else { "false" });
            Ok(Some(Value::Object(Some(s))))
        } else {
            let s = ctx.create_string("");
            Ok(Some(Value::Object(Some(s))))
        }
    });

    r.register(JN, "asInt", "()I", |ctx, args| {
        let this = crate::obj_arg(args, 0)?;
        let n = match ctx.get_field(this, JN_NUM) {
            Value::Long(v) => v as i32,
            _ => 0,
        };
        Ok(Some(Value::Int(n)))
    });

    r.register(JN, "asLong", "()J", |ctx, args| {
        let this = crate::obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, JN_NUM)))
    });

    r.register(JN, "asDouble", "()D", |ctx, args| {
        let this = crate::obj_arg(args, 0)?;
        // Use the dedicated double field for full precision; fall back to long
        let d = match ctx.get_field(this, JN_DBL) {
            Value::Double(v) if v != 0.0 => v,
            _ => match ctx.get_field(this, JN_NUM) {
                Value::Long(v) => v as f64,
                _ => 0.0,
            },
        };
        Ok(Some(Value::Double(d)))
    });

    r.register(JN, "asBoolean", "()Z", |ctx, args| {
        let this = crate::obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, JN_BOOL)))
    });

    r.register(JN, "isNull", "()Z", |ctx, args| {
        let this = crate::obj_arg(args, 0)?;
        let ntype = match ctx.get_field(this, JN_TYPE) {
            Value::Int(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(if ntype == 0 { 1 } else { 0 })))
    });

    r.register(JN, "isMissingNode", "()Z", |ctx, args| {
        let this = crate::obj_arg(args, 0)?;
        let ntype = match ctx.get_field(this, JN_TYPE) {
            Value::Int(v) => v,
            _ => -1,
        };
        Ok(Some(Value::Int(if ntype == -1 { 1 } else { 0 })))
    });

    r.register(JN, "isTextual", "()Z", |ctx, args| {
        let this = crate::obj_arg(args, 0)?;
        let ntype = match ctx.get_field(this, JN_TYPE) {
            Value::Int(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(if ntype == 3 { 1 } else { 0 })))
    });

    r.register(JN, "isNumber", "()Z", |ctx, args| {
        let this = crate::obj_arg(args, 0)?;
        let ntype = match ctx.get_field(this, JN_TYPE) {
            Value::Int(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(if ntype == 4 { 1 } else { 0 })))
    });

    r.register(JN, "isBoolean", "()Z", |ctx, args| {
        let this = crate::obj_arg(args, 0)?;
        let ntype = match ctx.get_field(this, JN_TYPE) {
            Value::Int(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(if ntype == 5 { 1 } else { 0 })))
    });

    r.register(JN, "isObject", "()Z", |ctx, args| {
        let this = crate::obj_arg(args, 0)?;
        let ntype = match ctx.get_field(this, JN_TYPE) {
            Value::Int(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(if ntype == 1 { 1 } else { 0 })))
    });

    r.register(JN, "isArray", "()Z", |ctx, args| {
        let this = crate::obj_arg(args, 0)?;
        let ntype = match ctx.get_field(this, JN_TYPE) {
            Value::Int(v) => v,
            _ => 0,
        };
        Ok(Some(Value::Int(if ntype == 2 { 1 } else { 0 })))
    });

    r.register(JN, "size", "()I", |ctx, args| {
        let this = crate::obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, JN_COUNT)))
    });

    // get(String fieldName) -> JsonNode — for object nodes
    r.register(
        JN,
        "get",
        "(Ljava/lang/String;)Lcom/fasterxml/jackson/databind/JsonNode;",
        |ctx, args| {
            let this = crate::obj_arg(args, 0)?;
            let name_ref = match args.get(1) {
                Some(Value::Object(Some(s))) => *s,
                _ => return Ok(Some(Value::Object(None))),
            };
            let name = ctx.read_string(name_ref).unwrap_or_default();
            let count = match ctx.get_field(this, JN_COUNT) {
                Value::Int(v) => v as usize,
                _ => 0,
            };
            let keys_arr = match ctx.get_field(this, JN_KEYS) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let children_arr = match ctx.get_field(this, JN_CHILDREN) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Object(None))),
            };
            for i in 0..count {
                if let Value::Object(Some(key_ref)) = ctx.get_array_element(keys_arr, i) {
                    if let Some(key_str) = ctx.read_string(key_ref) {
                        if key_str == name {
                            return Ok(Some(ctx.get_array_element(children_arr, i)));
                        }
                    }
                }
            }
            Ok(Some(Value::Object(None)))
        },
    );

    // get(int index) -> JsonNode — for array nodes
    r.register(
        JN,
        "get",
        "(I)Lcom/fasterxml/jackson/databind/JsonNode;",
        |ctx, args| {
            let this = crate::obj_arg(args, 0)?;
            let idx = match args.get(1) {
                Some(Value::Int(v)) => *v as usize,
                _ => 0,
            };
            let count = match ctx.get_field(this, JN_COUNT) {
                Value::Int(v) => v as usize,
                _ => 0,
            };
            if idx >= count {
                return Ok(Some(Value::Object(None)));
            }
            let children_arr = match ctx.get_field(this, JN_CHILDREN) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Object(None))),
            };
            Ok(Some(ctx.get_array_element(children_arr, idx)))
        },
    );

    // path(String) -> JsonNode (returns MissingNode instead of null)
    r.register(
        JN,
        "path",
        "(Ljava/lang/String;)Lcom/fasterxml/jackson/databind/JsonNode;",
        |ctx, args| {
            let this = crate::obj_arg(args, 0)?;
            let name_ref = match args.get(1) {
                Some(Value::Object(Some(s))) => *s,
                _ => {
                    let missing = alloc_json_node(ctx, -1)?; // missing node type
                    return Ok(Some(Value::Object(Some(missing))));
                }
            };
            let name = ctx.read_string(name_ref).unwrap_or_default();
            let count = match ctx.get_field(this, JN_COUNT) {
                Value::Int(v) => v as usize,
                _ => 0,
            };
            if let Value::Object(Some(keys_arr)) = ctx.get_field(this, JN_KEYS) {
                if let Value::Object(Some(children_arr)) = ctx.get_field(this, JN_CHILDREN) {
                    for i in 0..count {
                        if let Value::Object(Some(key_ref)) = ctx.get_array_element(keys_arr, i) {
                            if let Some(key_str) = ctx.read_string(key_ref) {
                                if key_str == name {
                                    return Ok(Some(ctx.get_array_element(children_arr, i)));
                                }
                            }
                        }
                    }
                }
            }
            let missing = alloc_json_node(ctx, -1)?;
            Ok(Some(Value::Object(Some(missing))))
        },
    );

    // has(String) -> boolean
    r.register(JN, "has", "(Ljava/lang/String;)Z", |ctx, args| {
        let this = crate::obj_arg(args, 0)?;
        let name_ref = match args.get(1) {
            Some(Value::Object(Some(s))) => *s,
            _ => return Ok(Some(Value::Int(0))),
        };
        let name = ctx.read_string(name_ref).unwrap_or_default();
        let count = match ctx.get_field(this, JN_COUNT) {
            Value::Int(v) => v as usize,
            _ => 0,
        };
        if let Value::Object(Some(keys_arr)) = ctx.get_field(this, JN_KEYS) {
            for i in 0..count {
                if let Value::Object(Some(key_ref)) = ctx.get_array_element(keys_arr, i) {
                    if let Some(key_str) = ctx.read_string(key_ref) {
                        if key_str == name {
                            return Ok(Some(Value::Int(1)));
                        }
                    }
                }
            }
        }
        Ok(Some(Value::Int(0)))
    });

    // toString() -> String
    r.register(JN, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = crate::obj_arg(args, 0)?;
        let json = json_node_to_string(ctx, this);
        let s = ctx.create_string(&json);
        Ok(Some(Value::Object(Some(s))))
    });

    // Also register for ObjectNode (extends JsonNode)
    let on = "com/fasterxml/jackson/databind/node/ObjectNode";
    // A fresh `ObjectNode` is an EMPTY OBJECT node, not a blank slate. Leaving
    // the slots at their allocation defaults leaves JN_TYPE unset, so every
    // accessor above (`isObject`, `size`, `get`, `toString`) reads it as 0 and
    // reports a NULL node. Stamp the same shape `alloc_json_node(ctx, 1)`
    // produces; the keys/children arrays stay null until the first entry is
    // added, since JN_COUNT is what drives the walks.
    r.register(on, "<init>", "()V", |ctx, args| {
        let this = crate::obj_arg(args, 0)?;
        if ctx.object_num_fields(this) <= JN_DBL {
            // A real Jackson `ObjectNode` (its own, much smaller layout) —
            // leave it entirely alone rather than writing over real fields.
            return Ok(None);
        }
        ctx.set_field(this, JN_TYPE, Value::Int(1));
        ctx.set_field(this, JN_TEXT, Value::Object(None));
        ctx.set_field(this, JN_NUM, Value::Long(0));
        ctx.set_field(this, JN_BOOL, Value::Int(0));
        ctx.set_field(this, JN_CHILDREN, Value::Object(None));
        ctx.set_field(this, JN_KEYS, Value::Object(None));
        ctx.set_field(this, JN_COUNT, Value::Int(0));
        ctx.set_field(this, JN_DBL, Value::Double(0.0));
        Ok(None)
    });
    r.set_category(__prev_cat);
}

/// Allocate a JsonNode synthetic object with the given type.
pub(crate) fn alloc_json_node(
    ctx: &mut dyn NativeContext,
    node_type: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let node = try_alloc_concurrent_synthetic(ctx, "com/fasterxml/jackson/databind/JsonNode", 8)?;
    ctx.set_field(node, 0, Value::Int(node_type));
    ctx.set_field(node, 2, Value::Long(0));
    ctx.set_field(node, 3, Value::Int(0));
    ctx.set_field(node, 6, Value::Int(0));
    ctx.set_field(node, 7, Value::Double(0.0));
    Ok(node)
}

/// Build a JsonNode tree from a raw JSON string.
pub(crate) fn build_json_tree_node(
    ctx: &mut dyn NativeContext,
    json: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    Ok(build_json_tree_node_depth(ctx, json, 0)?)
}

pub(crate) fn build_json_tree_node_depth(
    ctx: &mut dyn NativeContext,
    json: &str,
    depth: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    if depth > JSON_MAX_DEPTH || json.len() > JSON_MAX_INPUT_SIZE {
        return Ok(alloc_json_node(ctx, 0)?); // null node
    }
    let trimmed = json.trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(alloc_json_node(ctx, 0)?);
    }
    if trimmed.starts_with('{') {
        // Object node
        let pairs = parse_json_object(trimmed);
        let count = pairs.len();
        // The node and its two arrays survive a string and a whole recursive
        // subtree per pair — many allocations each — so they are held in the
        // scope and re-read at every store.
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let node_obj = alloc_json_node(&mut *scope, 1)?;
        let node_h = scope.root(node_obj);
        let children_obj = scope.new_array(cratonvm_types::ArrayElementType::Reference, count);
        let children_h = scope.root(children_obj);
        let keys_obj = scope.new_array(cratonvm_types::ArrayElementType::Reference, count);
        let keys_h = scope.root(keys_obj);
        for (i, (key, val_str)) in pairs.iter().enumerate() {
            let key_obj = scope.create_string(key);
            let keys = scope.get(&keys_h);
            scope.set_array_element(keys, i, Value::Object(Some(key_obj)));
            let child_json = if val_str.starts_with('{') || val_str.starts_with('[') {
                val_str.clone()
            } else if val_str == "true"
                || val_str == "false"
                || val_str == "null"
                || val_str.parse::<f64>().is_ok()
            {
                val_str.clone()
            } else {
                format!("\"{}\"", json_escape(val_str))
            };
            let child = build_json_tree_node_depth(&mut *scope, &child_json, depth + 1)?;
            let children = scope.get(&children_h);
            scope.set_array_element(children, i, Value::Object(Some(child)));
        }
        let node = scope.get(&node_h);
        let children = scope.get(&children_h);
        let keys = scope.get(&keys_h);
        scope.set_field(node, 4, Value::Object(Some(children)));
        scope.set_field(node, 5, Value::Object(Some(keys)));
        scope.set_field(node, 6, Value::Int(count as i32));
        Ok(node)
    } else if trimmed.starts_with('[') {
        // Array node
        let inner = &trimmed[1..trimmed.len().saturating_sub(1)];
        let mut elements = Vec::new();
        let mut i = 0;
        while i < inner.len() {
            // skip whitespace/commas
            while i < inner.len()
                && (inner.as_bytes()[i] == b' '
                    || inner.as_bytes()[i] == b','
                    || inner.as_bytes()[i] == b'\n'
                    || inner.as_bytes()[i] == b'\r'
                    || inner.as_bytes()[i] == b'\t')
            {
                i += 1;
            }
            if i >= inner.len() {
                break;
            }
            let val = read_json_value(inner, &mut i);
            if val.is_empty() && i >= inner.len() {
                break;
            }
            elements.push(val);
        }
        let count = elements.len();
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let node_obj = alloc_json_node(&mut *scope, 2)?;
        let node_h = scope.root(node_obj);
        let children_obj = scope.new_array(cratonvm_types::ArrayElementType::Reference, count);
        let children_h = scope.root(children_obj);
        for (idx, elem) in elements.iter().enumerate() {
            let child_json = if elem.starts_with('{') || elem.starts_with('[') {
                elem.clone()
            } else if *elem == "true"
                || *elem == "false"
                || *elem == "null"
                || elem.parse::<f64>().is_ok()
            {
                elem.clone()
            } else {
                format!("\"{}\"", json_escape(elem))
            };
            let child = build_json_tree_node_depth(&mut *scope, &child_json, depth + 1)?;
            let children = scope.get(&children_h);
            scope.set_array_element(children, idx, Value::Object(Some(child)));
        }
        let node = scope.get(&node_h);
        let children = scope.get(&children_h);
        scope.set_field(node, 4, Value::Object(Some(children)));
        scope.set_field(node, 6, Value::Int(count as i32));
        Ok(node)
    } else if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
        // String node
        let inner_str = json_unescape(&trimmed[1..trimmed.len() - 1]);
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let node_obj = alloc_json_node(&mut *scope, 3)?;
        let node_h = scope.root(node_obj);
        let s = scope.create_string(&inner_str);
        let node = scope.get(&node_h);
        scope.set_field(node, 1, Value::Object(Some(s)));
        Ok(node)
    } else if trimmed == "true" || trimmed == "false" {
        let node = alloc_json_node(ctx, 5)?;
        ctx.set_field(node, 3, Value::Int(if trimmed == "true" { 1 } else { 0 }));
        Ok(node)
    } else if let Ok(n) = trimmed.parse::<i64>() {
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let node_obj = alloc_json_node(&mut *scope, 4)?;
        let node_h = scope.root(node_obj);
        scope.set_field(node_obj, 2, Value::Long(n));
        scope.set_field(node_obj, 7, Value::Double(n as f64));
        let s = scope.create_string(trimmed);
        let node = scope.get(&node_h);
        scope.set_field(node, 1, Value::Object(Some(s)));
        Ok(node)
    } else if let Ok(f) = trimmed.parse::<f64>() {
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let node_obj = alloc_json_node(&mut *scope, 4)?;
        let node_h = scope.root(node_obj);
        scope.set_field(node_obj, 2, Value::Long(f as i64)); // truncated for asInt()/asLong()
        scope.set_field(node_obj, 7, Value::Double(f)); // full precision for asDouble()
        let s = scope.create_string(trimmed);
        let node = scope.get(&node_h);
        scope.set_field(node, 1, Value::Object(Some(s)));
        Ok(node)
    } else {
        // Bare string (unquoted) — treat as string
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let node_obj = alloc_json_node(&mut *scope, 3)?;
        let node_h = scope.root(node_obj);
        let s = scope.create_string(trimmed);
        let node = scope.get(&node_h);
        scope.set_field(node, 1, Value::Object(Some(s)));
        Ok(node)
    }
}

/// Convert a JsonNode back to a JSON string.
pub(crate) fn json_node_to_string(ctx: &mut dyn NativeContext, node: ObjectRef) -> String {
    let ntype = match ctx.get_field(node, 0) {
        Value::Int(v) => v,
        _ => 0,
    };
    match ntype {
        0 => "null".to_string(),
        1 => {
            // object
            let count = match ctx.get_field(node, 6) {
                Value::Int(v) => v as usize,
                _ => 0,
            };
            let mut entries = Vec::new();
            if let (Value::Object(Some(keys)), Value::Object(Some(children))) =
                (ctx.get_field(node, 5), ctx.get_field(node, 4))
            {
                for i in 0..count {
                    let key_str = if let Value::Object(Some(kr)) = ctx.get_array_element(keys, i) {
                        ctx.read_string(kr).unwrap_or_default()
                    } else {
                        String::new()
                    };
                    let child_str =
                        if let Value::Object(Some(cr)) = ctx.get_array_element(children, i) {
                            json_node_to_string(ctx, cr)
                        } else {
                            "null".to_string()
                        };
                    entries.push(format!("\"{}\":{}", json_escape(&key_str), child_str));
                }
            }
            format!("{{{}}}", entries.join(","))
        }
        2 => {
            // array
            let count = match ctx.get_field(node, 6) {
                Value::Int(v) => v as usize,
                _ => 0,
            };
            let mut elems = Vec::new();
            if let Value::Object(Some(children)) = ctx.get_field(node, 4) {
                for i in 0..count {
                    if let Value::Object(Some(cr)) = ctx.get_array_element(children, i) {
                        elems.push(json_node_to_string(ctx, cr));
                    } else {
                        elems.push("null".to_string());
                    }
                }
            }
            format!("[{}]", elems.join(","))
        }
        3 => {
            // string
            if let Value::Object(Some(sr)) = ctx.get_field(node, 1) {
                let s = ctx.read_string(sr).unwrap_or_default();
                format!("\"{}\"", json_escape(&s))
            } else {
                "\"\"".to_string()
            }
        }
        4 => {
            // number
            if let Value::Object(Some(sr)) = ctx.get_field(node, 1) {
                ctx.read_string(sr).unwrap_or_else(|| "0".to_string())
            } else {
                match ctx.get_field(node, 2) {
                    Value::Long(v) => v.to_string(),
                    _ => "0".to_string(),
                }
            }
        }
        5 => {
            // boolean
            let b = match ctx.get_field(node, 3) {
                Value::Int(v) => v,
                _ => 0,
            };
            if b != 0 {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        _ => "null".to_string(), // missing node
    }
}
