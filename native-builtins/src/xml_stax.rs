// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave 1, Task D — JAXP / StAX (`javax.xml.stream`) native overrides.
//!
//! Provides a minimal but spec-faithful StAX cursor API backed by the
//! `quick-xml` Rust parser. Required by Maven (`pom.xml`), Spring
//! XML config, Hibernate (`hibernate.cfg.xml`) and effectively every
//! Java build/runtime that needs to consume an XML document.
//!
//! Surface registered:
//!   * `javax/xml/stream/XMLInputFactory.newInstance()` (and the
//!     `newFactory()` alias) — returns a synthetic factory object.
//!   * `XMLInputFactory.createXMLStreamReader(InputStream)` /
//!     `(InputStream, String)` / `(Reader)` — eagerly drains the
//!     source into an in-memory event vector and returns a synthetic
//!     reader object pointing at the first event.
//!   * `XMLStreamReader.{hasNext, next, getEventType, getLocalName,
//!     getName, getAttributeValue(String,String), getAttributeCount,
//!     getAttributeLocalName, getAttributeValue(int), getText, close,
//!     isStartElement, isEndElement, isCharacters, isWhiteSpace,
//!     getNamespaceURI()}` — all driven from the precomputed event
//!     vector (no streaming I/O after construction).
//!
//! Synthetic objects:
//!   * Both the factory and the reader are allocated as instances of
//!     their abstract / interface declared class (the heap allows
//!     this; `alloc_object` is layout-only). Because our native
//!     dispatch is keyed on the *declaring* class, registering on
//!     `javax/xml/stream/XMLInputFactory` and
//!     `javax/xml/stream/XMLStreamReader` directly is sufficient for
//!     `invokevirtual` / `invokeinterface` to find the override.
//!
//! State is held in a process-wide side-table keyed by the reader's
//! GC-stable identity hash code (see `NativeContext::identity_hash_code`,
//! remapped across compaction by `HashCodeTable::update_after_gc` in
//! `gc/src/compact_header.rs`).  The earlier `obj.as_ptr() as usize`
//! keying was orphaned by a moving collector, after which subsequent
//! `next()` / `getEventType()` calls silently fell back to defaults —
//! mirrors the WP4.2 / Round-9 fix in `lang_invoke::VH_META_TABLE`
//! (`native-builtins/src/lang_invoke.rs:178-203`).

use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::Mutex;
use quick_xml::events::Event as QXmlEvent;
use quick_xml::reader::Reader;

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::{ObjectRef, Value};

// ---------------------------------------------------------------------------
// StAX event constants (mirror `javax.xml.stream.XMLStreamConstants`).
// ---------------------------------------------------------------------------

const START_ELEMENT: i32 = 1;
const END_ELEMENT: i32 = 2;
const PROCESSING_INSTRUCTION: i32 = 3;
const CHARACTERS: i32 = 4;
const COMMENT: i32 = 5;
const START_DOCUMENT: i32 = 7;
const END_DOCUMENT: i32 = 8;
const DTD: i32 = 11;
const CDATA: i32 = 12;
const SPACE: i32 = 6;
const ENTITY_REFERENCE: i32 = 9;

// ---------------------------------------------------------------------------
// Reader event model.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
struct StaxEvent {
    kind: i32,
    /// Element local name (for START/END_ELEMENT) or PI target.
    local_name: String,
    /// Element namespace URI (best-effort: prefix's URI from xmlns* attrs).
    namespace_uri: String,
    /// Element prefix for START/END_ELEMENT ("" when unprefixed / in the
    /// default namespace). Needed by the event API: `XMLEventAllocatorImpl`
    /// builds `new QName(namespaceURI, localName, prefix)` and the QName ctor
    /// throws `IllegalArgumentException` on a null prefix, so `getPrefix()`
    /// must return non-null.
    prefix: String,
    /// Character / CDATA / comment / DTD payload.
    text: String,
    /// Attribute table for START_ELEMENT events.
    attributes: Vec<StaxAttr>,
    /// Namespace declarations (`xmlns` / `xmlns:p`) that appear LITERALLY on
    /// this START_ELEMENT, as `(prefix, uri)` in source order (the default
    /// namespace uses prefix == ""). The event API's
    /// `XMLEventAllocatorImpl.fillNamespaceAttributes` reads these via
    /// `getNamespaceCount()`/`getNamespaceURI(i)`/`getNamespacePrefix(i)` to
    /// attach `Namespace` events, which an `XMLEventWriter` re-emits — without
    /// them an inline `xmlns="…"`/`xmlns:p="…"` on a re-serialized element is
    /// dropped.
    namespaces: Vec<(String, String)>,
    /// 1-based source line of the event start (StAX `Location.getLineNumber`).
    line: i32,
    /// 1-based source column of the event start (StAX `Location.getColumnNumber`).
    column: i32,
    /// 0-based byte offset of the event start (StAX `Location.getCharacterOffset`).
    char_offset: i32,
}

#[derive(Clone, Debug)]
struct StaxAttr {
    local_name: String,
    /// Attribute prefix ("" when unprefixed). Needed by the event API:
    /// `XMLEventAllocatorImpl.fillAttributes` builds the attribute QName from
    /// `getAttributeName(i)` and a downstream `XMLEventWriter` rejects an
    /// attribute whose QName has an empty prefix but a non-empty namespace URI
    /// ("prefix cannot be null or empty").
    prefix: String,
    namespace_uri: String,
    value: String,
}

#[derive(Debug)]
struct ReaderState {
    events: Vec<StaxEvent>,
    /// Index of the *current* event. Starts at -1 so `next()` advances
    /// to event 0 on the first call (matches StAX semantics where
    /// `getEventType()` after construction returns START_DOCUMENT).
    cursor: isize,
    /// XML declaration encoding, as reported by
    /// XMLStreamReader.getCharacterEncodingScheme().
    character_encoding_scheme: Option<String>,
    /// `standalone` pseudo-attribute of the XML declaration.
    /// `None` = the declaration omitted it (so `standaloneSet()` is false).
    standalone: Option<bool>,
}

impl ReaderState {
    fn current(&self) -> Option<&StaxEvent> {
        if self.cursor < 0 {
            None
        } else {
            self.events.get(self.cursor as usize)
        }
    }
}

// ---------------------------------------------------------------------------
// Side-table keyed by reader identity hash code (GC-stable; see module docs).
// ---------------------------------------------------------------------------

fn reader_table() -> &'static Mutex<HashMap<i32, ReaderState>> {
    static T: OnceLock<Mutex<HashMap<i32, ReaderState>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// (line, column, char_offset) captured for a `javax/xml/stream/Location`
/// object at the moment `XMLStreamReader.getLocation()` produced it. `Location`
/// is a bare interface with no instance fields, so we cannot stash the position
/// on the object itself; instead we key a side-table by the Location object's
/// GC-stable identity hash, mirroring the reader-state table above. The three
/// int accessors (getLineNumber/getColumnNumber/getCharacterOffset) read it back.
fn location_table() -> &'static Mutex<HashMap<i32, (i32, i32, i32)>> {
    static T: OnceLock<Mutex<HashMap<i32, (i32, i32, i32)>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn obj_key(ctx: &dyn NativeContext, o: ObjectRef) -> i32 {
    // `identity_hash_code` is GC-stable: `HashCodeTable::update_after_gc`
    // remaps the table when the GC relocates an object, so the key the
    // side-table was inserted under continues to resolve the same
    // ReaderState entry after a compaction cycle.
    ctx.identity_hash_code(o)
}

fn store_state(ctx: &dyn NativeContext, reader: ObjectRef, state: ReaderState) {
    reader_table().lock().insert(obj_key(ctx, reader), state);
}

fn with_state<F, R>(ctx: &dyn NativeContext, reader: ObjectRef, f: F) -> Option<R>
where
    F: FnOnce(&mut ReaderState) -> R,
{
    let key = obj_key(ctx, reader);
    let mut tbl = reader_table().lock();
    tbl.get_mut(&key).map(f)
}

fn drop_state(ctx: &dyn NativeContext, reader: ObjectRef) {
    let key = obj_key(ctx, reader);
    reader_table().lock().remove(&key);
}

/// Loud post-GC missing-state guard.  Returns Err with a clear
/// IllegalStateException when the receiver has no entry — after the
/// identity-hash-code re-key the only way to hit this is when the
/// receiver was never produced by our `createXMLStreamReader`, i.e. a
/// caller bug rather than a GC artefact.  Matches the C14
/// MessageDigest precedent (loud `IllegalStateException` over silent
/// fallback).
fn require_state(ctx: &dyn NativeContext, reader: ObjectRef) -> Result<(), MethodCallFailed> {
    let key = obj_key(ctx, reader);
    if reader_table().lock().contains_key(&key) {
        Ok(())
    } else {
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IllegalStateException {
                message: "XMLStreamReader state missing post-GC or never initialized".into(),
            },
        )))
    }
}

// ---------------------------------------------------------------------------
// quick-xml driver: drains the entire byte source into a `Vec<StaxEvent>`.
// We pre-materialise to keep the native callbacks free of mutable parser
// state shared across invocations (the lifetime of a quick-xml `Reader` is
// otherwise awkward to thread through a `fn(...)`-typed callback).
// ---------------------------------------------------------------------------

/// Compute the 1-based (line, column) and 0-based byte offset for a position
/// `byte_pos` in the original source `bytes`. Newlines are counted as `\n`
/// (a `\r\n` pair advances the line on the `\n`, matching how StAX reference
/// readers report positions). Column is in bytes-since-line-start + 1, which
/// matches ASCII/Latin XML; for multibyte UTF-8 it is a best-effort byte
/// column (the JDK readers themselves report char columns, but byte columns
/// are a faithful-enough approximation for the diagnostic use of Location).
fn line_col_of(bytes: &[u8], byte_pos: usize) -> (i32, i32) {
    let end = byte_pos.min(bytes.len());
    let mut line: i32 = 1;
    let mut col: i32 = 1;
    for &b in &bytes[..end] {
        if b == b'\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// Namespace-scope stack used while walking the document so that an element's
/// (and its attributes') namespace URI is resolved against ALL in-scope
/// `xmlns`/`xmlns:prefix` declarations — not just declarations that appear on
/// the element itself. XML namespaces are lexically scoped: a default namespace
/// (`xmlns="…"`) declared on an ancestor applies to every descendant element
/// that does not redeclare it. Without this, WildFly's `standalone.xml`
/// (`xmlns="urn:jboss:domain:20.0"` on `<server>`) reports the correct URI for
/// `<server>` but an empty URI for `<extensions>`, `<extension>`, … which makes
/// staxmapper's `QName`-keyed parser lookup fail and aborts the boot with an
/// `XMLStreamException` (then WildFly's vdx error reporter hangs).
#[derive(Default)]
struct NsScopes {
    /// One frame per open element. Each frame holds the `(prefix, uri)` pairs
    /// DECLARED on that element. The default namespace uses prefix == "".
    frames: Vec<Vec<(String, String)>>,
}

impl NsScopes {
    /// Build a frame from an element's attribute list (the xmlns / xmlns:p
    /// declarations) and push it. Call once per START_ELEMENT, BEFORE resolving
    /// the element's own namespace.
    fn push_from_attrs(&mut self, attrs: quick_xml::events::attributes::Attributes) {
        let mut frame: Vec<(String, String)> = Vec::new();
        for a in attrs.flatten() {
            let key = a.key.as_ref();
            let val = a
                .unescape_value()
                .map(|c| c.into_owned())
                .unwrap_or_else(|_| String::from_utf8_lossy(&a.value).into_owned());
            if key == b"xmlns" {
                frame.push((String::new(), val));
            } else if let Some(prefix) = key.strip_prefix(b"xmlns:") {
                frame.push((String::from_utf8_lossy(prefix).into_owned(), val));
            }
        }
        self.frames.push(frame);
    }

    fn pop(&mut self) {
        self.frames.pop();
    }

    /// Resolve a prefix to a namespace URI against the current scope stack
    /// (innermost frame wins). An empty `prefix` resolves the default
    /// namespace. Returns "" when no binding is in scope (unbound prefix or no
    /// default namespace), matching StAX's `getNamespaceURI()==null/""`.
    fn resolve(&self, prefix: &str) -> String {
        for frame in self.frames.iter().rev() {
            for (p, uri) in frame.iter().rev() {
                if p == prefix {
                    return uri.clone();
                }
            }
        }
        String::new()
    }
}

/// Split a possibly-prefixed qualified name into `(prefix, local)`. An absent
/// prefix yields `""`.
fn split_qname(qname: &[u8]) -> (String, String) {
    let s = String::from_utf8_lossy(qname);
    match s.find(':') {
        Some(i) => (s[..i].to_string(), s[i + 1..].to_string()),
        None => (String::new(), s.into_owned()),
    }
}

struct PreparedXml {
    bytes: Vec<u8>,
    character_encoding_scheme: Option<String>,
    standalone: Option<bool>,
}

fn prepare_xml_bytes(bytes: &[u8]) -> PreparedXml {
    let text = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        String::from_utf8_lossy(&bytes[3..]).into_owned()
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        decode_utf16_bytes(&bytes[2..], true)
    } else if bytes.starts_with(&[0xFF, 0xFE]) {
        decode_utf16_bytes(&bytes[2..], false)
    } else if bytes.len() >= 4
        && bytes[0] == 0x00
        && bytes[1] == 0x3C
        && bytes[2] == 0x00
        && bytes[3] == 0x3F
    {
        decode_utf16_bytes(bytes, true)
    } else if bytes.len() >= 4
        && bytes[0] == 0x3C
        && bytes[1] == 0x00
        && bytes[2] == 0x3F
        && bytes[3] == 0x00
    {
        decode_utf16_bytes(bytes, false)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    };

    let declared = xml_decl_encoding(&text);
    let standalone = xml_decl_standalone(&text);
    PreparedXml {
        bytes: text.into_bytes(),
        character_encoding_scheme: declared,
        standalone,
    }
}

fn decode_utf16_bytes(bytes: &[u8], big_endian: bool) -> String {
    let mut units = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let unit = if big_endian {
            u16::from_be_bytes([pair[0], pair[1]])
        } else {
            u16::from_le_bytes([pair[0], pair[1]])
        };
        units.push(unit);
    }
    String::from_utf16_lossy(&units)
}

fn xml_decl_encoding(text: &str) -> Option<String> {
    xml_decl_attr(text, "encoding")
}

/// `standalone` pseudo-attribute of the XML declaration, per XML 1.0 §2.9.
/// `None` means the declaration was absent or omitted the attribute, which is
/// what `XMLStreamReader.standaloneSet()` reports as `false`.
fn xml_decl_standalone(text: &str) -> Option<bool> {
    match xml_decl_attr(text, "standalone")?.as_str() {
        "yes" => Some(true),
        "no" => Some(false),
        // Any other literal is not well-formed; treat it as "not declared"
        // rather than guessing, matching a non-validating parser's leniency.
        _ => None,
    }
}

/// Pull one quoted pseudo-attribute out of the `<?xml … ?>` declaration.
fn xml_decl_attr(text: &str, key: &str) -> Option<String> {
    let s = text.strip_prefix('\u{FEFF}').unwrap_or(text).trim_start();
    let rest = s.strip_prefix("<?xml")?;
    let end = rest.find("?>").unwrap_or(rest.len());
    let decl = &rest[..end];
    let key_pos = decl.find(key)?;
    let mut tail = &decl[key_pos + key.len()..];
    tail = tail.trim_start();
    if !tail.starts_with('=') {
        return None;
    }
    tail = tail[1..].trim_start();
    let quote = tail.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let after = &tail[quote.len_utf8()..];
    let end = after.find(quote)?;
    let value = &after[..end];
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn parse_to_events(bytes: &[u8]) -> Vec<StaxEvent> {
    let mut events: Vec<StaxEvent> = Vec::new();
    // START_DOCUMENT sits at the very start of the source (line 1, col 1, offset 0).
    let (l0, c0) = line_col_of(bytes, 0);
    events.push(StaxEvent {
        kind: START_DOCUMENT,
        line: l0,
        column: c0,
        char_offset: 0,
        ..Default::default()
    });
    // Lexical namespace-scope stack (default + prefixed) maintained across the
    // whole document so descendant elements inherit ancestor xmlns decls.
    let mut scopes = NsScopes::default();

    let mut reader = Reader::from_reader(bytes);
    reader.trim_text(false);
    reader.expand_empty_elements(false);
    reader.check_end_names(false);

    let mut buf: Vec<u8> = Vec::new();
    loop {
        // `buffer_position()` after the previous read is the byte offset where
        // the *next* event begins — i.e. the start of the event we are about to
        // read. quick-xml exposes byte offset only (no native line/column), so
        // we derive line/column from the source bytes.
        let start_pos = reader.buffer_position();
        // Index of the first event produced by this iteration; used to stamp
        // position onto every event pushed below (Empty produces two).
        let first_new = events.len();
        buf.clear();
        match reader.read_event_into(&mut buf) {
            Ok(QXmlEvent::Start(e)) => {
                // Push this element's xmlns declarations, then resolve its
                // namespace (and its attributes') against the full scope stack.
                scopes.push_from_attrs(e.attributes());
                events.push(make_element_event(
                    START_ELEMENT,
                    e.name().as_ref(),
                    e.attributes(),
                    &scopes,
                ));
            }
            Ok(QXmlEvent::Empty(e)) => {
                let name = e.name().as_ref().to_vec();
                // Self-closing: push the scope only for the duration of
                // resolving this element, then pop immediately (no children).
                scopes.push_from_attrs(e.attributes());
                let start_ev = make_element_event(START_ELEMENT, &name, e.attributes(), &scopes);
                let ns = start_ev.namespace_uri.clone();
                let prefix = start_ev.prefix.clone();
                // StAX reports the same xmlns declarations on END_ELEMENT as on
                // the matching START_ELEMENT (they go out of scope here); a
                // self-closing tag closes its own start, so carry its decls.
                let namespaces = start_ev.namespaces.clone();
                events.push(start_ev);
                events.push(StaxEvent {
                    kind: END_ELEMENT,
                    local_name: local_name_of(&name),
                    namespace_uri: ns,
                    prefix,
                    namespaces,
                    ..Default::default()
                });
                scopes.pop();
            }
            Ok(QXmlEvent::End(e)) => {
                let name = e.name().as_ref().to_vec();
                // Resolve the end tag's namespace in its still-open scope, then
                // pop the frame the matching START pushed.
                let (prefix, local) = split_qname(&name);
                let ns = scopes.resolve(&prefix);
                // StAX reports getNamespaceCount()/Prefix/URI on END_ELEMENT too:
                // the xmlns declarations on the MATCHING START_ELEMENT (the frame
                // this end tag closes), which go out of scope here. Spring's
                // StaxStreamXMLReader.handleEndElement (and the event API's
                // EndElement.getNamespaces()) relies on this to emit
                // endPrefixMapping for each declared prefix; an empty list drops
                // those SAX callbacks. The innermost still-open frame holds
                // exactly the matching start's decls, in source order.
                let namespaces = scopes.frames.last().cloned().unwrap_or_default();
                events.push(StaxEvent {
                    kind: END_ELEMENT,
                    local_name: local,
                    namespace_uri: ns,
                    prefix,
                    namespaces,
                    ..Default::default()
                });
                scopes.pop();
            }
            Ok(QXmlEvent::Text(e)) => {
                let raw = e
                    .unescape()
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| String::from_utf8_lossy(e.as_ref()).into_owned());
                // Suppress prolog/epilog whitespace — text that appears OUTSIDE
                // the root element (scope stack empty). The JDK StAX reader does
                // not report misc/epilog whitespace as a CHARACTERS event, so
                // neither do we; this keeps the event stream byte-identical to
                // HotSpot (the trailing "\n" after the root close was an extra
                // event). Harmless for the cursor API: its consumers locate
                // START/END via nextTag(), which skips whitespace anyway.
                if scopes.frames.is_empty() && raw.trim().is_empty() {
                    // drop epilog/prolog whitespace
                } else {
                    events.push(StaxEvent {
                        kind: CHARACTERS,
                        text: raw,
                        ..Default::default()
                    });
                }
            }
            Ok(QXmlEvent::CData(e)) => {
                let s = String::from_utf8_lossy(e.as_ref()).into_owned();
                events.push(StaxEvent {
                    kind: CDATA,
                    text: s,
                    ..Default::default()
                });
            }
            Ok(QXmlEvent::Comment(e)) => {
                let s = String::from_utf8_lossy(e.as_ref()).into_owned();
                events.push(StaxEvent {
                    kind: COMMENT,
                    text: s,
                    ..Default::default()
                });
            }
            Ok(QXmlEvent::PI(e)) => {
                let s = String::from_utf8_lossy(e.as_ref()).into_owned();
                events.push(StaxEvent {
                    kind: PROCESSING_INSTRUCTION,
                    local_name: s,
                    ..Default::default()
                });
            }
            Ok(QXmlEvent::Decl(_)) => {
                // XML declaration is folded into START_DOCUMENT — already pushed.
            }
            Ok(QXmlEvent::DocType(e)) => {
                let s = String::from_utf8_lossy(e.as_ref()).into_owned();
                events.push(StaxEvent {
                    kind: DTD,
                    text: s,
                    ..Default::default()
                });
            }
            Ok(QXmlEvent::Eof) => break,
            Err(_) => break,
        }
        // Stamp real position (line/column/byte offset) onto every event this
        // iteration produced. quick-xml's byte offset is the load-bearing datum;
        // line/column are derived from it against the original source bytes.
        let (line, column) = line_col_of(bytes, start_pos);
        for ev in &mut events[first_new..] {
            ev.line = line;
            ev.column = column;
            ev.char_offset = start_pos as i32;
        }
    }

    let end_off = bytes.len() as i32;
    let (le, ce) = line_col_of(bytes, bytes.len());
    events.push(StaxEvent {
        kind: END_DOCUMENT,
        line: le,
        column: ce,
        char_offset: end_off,
        ..Default::default()
    });
    events
}

fn make_reader_state(bytes: &[u8]) -> ReaderState {
    let prepared = prepare_xml_bytes(bytes);
    let events = parse_to_events(&prepared.bytes);
    ReaderState {
        events,
        cursor: 0,
        character_encoding_scheme: prepared.character_encoding_scheme,
        standalone: prepared.standalone,
    }
}

fn make_element_event(
    kind: i32,
    qname: &[u8],
    attrs: quick_xml::events::attributes::Attributes,
    scopes: &NsScopes,
) -> StaxEvent {
    let (el_prefix, local) = split_qname(qname);
    // Resolve the element's namespace from its prefix (default ns when none).
    let el_ns = scopes.resolve(&el_prefix);
    let mut ev = StaxEvent {
        kind,
        local_name: local,
        namespace_uri: el_ns,
        prefix: el_prefix,
        ..Default::default()
    };
    for a in attrs.flatten() {
        let key = a.key.as_ref().to_vec();
        let val = a
            .unescape_value()
            .map(|c| c.into_owned())
            .unwrap_or_else(|_| String::from_utf8_lossy(&a.value).into_owned());
        // xmlns / xmlns:* declarations are NOT ordinary attributes
        // (getAttributeCount excludes them) — they are reported separately via
        // the getNamespace*(i) surface, in source order, so an XMLEventWriter
        // can re-emit a literal `xmlns(:p)="…"` on this element.
        if key == b"xmlns" {
            ev.namespaces.push((String::new(), val));
            continue;
        } else if let Some(p) = key.strip_prefix(b"xmlns:") {
            ev.namespaces
                .push((String::from_utf8_lossy(p).into_owned(), val));
            continue;
        }
        // Per the Namespaces-in-XML spec, an UNPREFIXED attribute has NO
        // namespace (the default namespace does NOT apply to attributes); a
        // prefixed attribute resolves its prefix against the scope stack.
        let (attr_prefix, attr_local) = split_qname(&key);
        let attr_ns = if attr_prefix.is_empty() {
            String::new()
        } else {
            scopes.resolve(&attr_prefix)
        };
        ev.attributes.push(StaxAttr {
            local_name: attr_local,
            prefix: attr_prefix,
            namespace_uri: attr_ns,
            value: val,
        });
    }
    ev
}

fn local_name_of(qname: &[u8]) -> String {
    let s = String::from_utf8_lossy(qname);
    match s.find(':') {
        Some(i) => s[i + 1..].to_string(),
        None => s.into_owned(),
    }
}

// ---------------------------------------------------------------------------
// InputStream draining helper.
//
// We support both real `java.io.FileInputStream` (read via the backing
// file descriptor through our FD table) and any `InputStream` whose
// `read()` method we can drive via `ctx.invoke`. The `FileInputStream`
// fast-path avoids reentering the interpreter for the (potentially
// thousands of) byte-at-a-time reads.
// ---------------------------------------------------------------------------

fn drain_input_stream(ctx: &mut dyn NativeContext, stream: ObjectRef) -> Vec<u8> {
    // Fast-path: FileInputStream with an accessible "path" string field.
    if let Some(path) = read_fis_path(ctx, stream) {
        if let Ok(bytes) = std::fs::read(&path) {
            return bytes;
        }
    }
    drain_via_invoke(ctx, stream)
}

fn read_fis_path(ctx: &mut dyn NativeContext, stream: ObjectRef) -> Option<String> {
    let cid = ctx.class_id_of_object(stream);
    let name = ctx.class_name_of_id(cid).unwrap_or_default();
    if !name.contains("FileInputStream") {
        return None;
    }
    // FileInputStream stores its source pathname on the `path` instance field.
    let v = ctx.get_field_by_name(stream, "path");
    if let Value::Object(Some(s)) = v {
        return ctx.read_string(s);
    }
    None
}

fn drain_via_invoke(ctx: &mut dyn NativeContext, stream: ObjectRef) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    // Loop over `int read()` calls until -1 is returned. This is O(n)
    // interpreter re-entries but matches the spec for arbitrary streams.
    loop {
        let res = ctx.invoke(
            "java/io/InputStream",
            "read",
            "()I",
            &[Value::Object(Some(stream))],
        );
        match res {
            Ok(Some(Value::Int(-1))) => break,
            Ok(Some(Value::Int(n))) => out.push((n & 0xff) as u8),
            _ => break,
        }
        if out.len() > 64 * 1024 * 1024 {
            // Safety bound: do not buffer more than 64 MiB from an
            // arbitrary stream. Real XML configs are tiny.
            break;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Shared helpers for native callbacks.
// ---------------------------------------------------------------------------

fn this_obj(args: &[Value]) -> Result<ObjectRef, MethodCallFailed> {
    match args.first() {
        Some(Value::Object(Some(o))) => Ok(*o),
        _ => Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NullPointerException {
                message: Some("StAX call on null receiver".to_string()),
            },
        ))),
    }
}

fn alloc_synthetic(
    ctx: &mut dyn NativeContext,
    class_name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    // Ensure the class is loaded so `class_id_by_name` returns a hit.
    let _ = ctx.load_class(class_name)?;
    let cid = ctx.class_id_by_name(class_name).ok_or_else(|| {
        MethodCallFailed::InternalError(VmError::Internal {
            message: format!("StAX: class {} not loadable", class_name),
        })
    })?;
    Ok(ctx.alloc_object(cid, 0))
}

// ---------------------------------------------------------------------------
// Native callback bodies.
// ---------------------------------------------------------------------------

/// `CRATONVM_REAL_STAX_FACTORY` gate (**default ON**; set `=0` to disable).
///
/// When ON, `XMLInputFactory.newInstance()` / `newFactory()` first try to obtain
/// a real, concrete third-party StAX provider registered on the classpath via
/// `ServiceLoader` (e.g. Woodstox's `com.ctc.wstx.stax.WstxInputFactory`). Such
/// a provider runs as real JDK bytecode and honours `setXMLResolver`, external
/// **general**-entity expansion, and `IS_SUPPORTING_EXTERNAL_ENTITIES` — none of
/// which the synthetic cursor factory below supports (it silently ignores the
/// resolver and leaves `&ext;` references unexpanded). This is exactly what
/// HotSpot does when Woodstox is on the classpath, so preferring the real
/// provider is the reference-JVM-faithful default (e.g. Hibernate's
/// `EntityResolverTest`, whose `Parent.hbm.xml` pulls in `child.xml` via a
/// `classpath://` SYSTEM entity).
///
/// The synthetic factory is retained as the fallback for when **no** concrete
/// third-party provider is registered: the JDK's own
/// `com.sun.xml.internal.stream.XMLInputFactoryImpl` does not run on CratonVM
/// (its `fEntityManager` is null → NPE on first parse), so WildFly's
/// `XMLInputFactoryUtil.create()` boot path stays on the stub when no provider
/// is present. Validated regression-free on the Hibernate XML-binding gauntlet
/// (12/12, `EntityResolverTest` FAIL→PASS, no others changed) and as a no-op on
/// no-provider classpaths; the escape hatch (`=0`) restores the synthetic stub
/// if a non-Woodstox provider ever misbehaves on CratonVM.
///
/// `"0"` ⇒ off (escape hatch); unset or any other value ⇒ on.
fn real_stax_factory_gate() -> bool {
    crate::nbflags().real_stax_factory
}

/// Extract a non-null `ObjectRef` from an `invoke` result, swallowing errors /
/// nulls into `None` (so the caller can fall back to the synthetic factory).
fn obj_result(r: MethodCallResult) -> Option<ObjectRef> {
    match r {
        Ok(Some(Value::Object(Some(o)))) => Some(o),
        _ => None,
    }
}

/// Resolve a real, concrete StAX `XMLInputFactory` provider from the classpath
/// via `ServiceLoader` — mirroring `FactoryFinder`'s service-provider step but
/// **without** its JDK fallback (which is broken on CratonVM). Returns the
/// provider object when a concrete third-party factory is registered; `None`
/// otherwise (no provider, or any step failing → caller uses the synthetic stub).
fn try_resolve_real_factory(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    // Class mirror for the service interface.
    let name_str = ctx.create_string("javax.xml.stream.XMLInputFactory");
    let clazz = obj_result(ctx.invoke(
        "java/lang/Class",
        "forName",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[Value::Object(Some(name_str))],
    ))?;

    // Thread-context class loader; fall back to the system loader when null so
    // `ServiceLoader` scans the application classpath rather than the bootstrap.
    let tccl = match ctx.invoke(
        "java/lang/Thread",
        "currentThread",
        "()Ljava/lang/Thread;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(t)))) => obj_result(ctx.invoke(
            "java/lang/Thread",
            "getContextClassLoader",
            "()Ljava/lang/ClassLoader;",
            &[Value::Object(Some(t))],
        )),
        _ => None,
    };
    let loader = tccl.or_else(|| {
        obj_result(ctx.invoke(
            "java/lang/ClassLoader",
            "getSystemClassLoader",
            "()Ljava/lang/ClassLoader;",
            &[],
        ))
    });
    let loader_val = loader.map_or(Value::Object(None), |l| Value::Object(Some(l)));

    // ServiceLoader.load(XMLInputFactory.class, loader).iterator()
    let sl = obj_result(ctx.invoke(
        "java/util/ServiceLoader",
        "load",
        "(Ljava/lang/Class;Ljava/lang/ClassLoader;)Ljava/util/ServiceLoader;",
        &[Value::Object(Some(clazz)), loader_val],
    ))?;
    let it = obj_result(ctx.invoke(
        "java/util/ServiceLoader",
        "iterator",
        "()Ljava/util/Iterator;",
        &[Value::Object(Some(sl))],
    ))?;

    // First registered provider, if any.
    match ctx.invoke(
        "java/util/Iterator",
        "hasNext",
        "()Z",
        &[Value::Object(Some(it))],
    ) {
        Ok(Some(Value::Int(1))) => {}
        _ => return None,
    }
    let provider = obj_result(ctx.invoke(
        "java/util/Iterator",
        "next",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(it))],
    ))?;

    // Guard: the provider must be a concrete subclass, never the abstract base
    // `javax/xml/stream/XMLInputFactory` itself — returning that would re-enter
    // our natives (newInstance et al. are keyed on the abstract class) and the
    // resolver/entity handling would be no better than the synthetic stub.
    let cid = ctx.class_id_of_object(provider);
    match ctx.class_name_of_id(cid) {
        Some(n) if n != "javax/xml/stream/XMLInputFactory" => Some(provider),
        _ => None,
    }
}

fn native_factory_new_instance(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Prefer a real, concrete third-party StAX provider (e.g. Woodstox) when one
    // is registered and the gate is on — it honours XMLResolver / external
    // general-entity expansion that the synthetic cursor factory ignores. See
    // `real_stax_factory_gate`.
    if real_stax_factory_gate() {
        if let Some(real) = try_resolve_real_factory(ctx) {
            return Ok(Some(Value::Object(Some(real))));
        }
    }
    let factory = alloc_synthetic(ctx, "javax/xml/stream/XMLInputFactory")?;
    Ok(Some(Value::Object(Some(factory))))
}

/// `XMLInputFactory.setProperty(String, Object)` / `setXMLResolver` /
/// `setEventAllocator` / `setXMLReporter` — no-ops on our synthetic factory.
///
/// `javax/xml/stream/XMLInputFactory` is an abstract class; in real StAX
/// these methods are implemented by the concrete factory subclass
/// (`com.sun.xml.internal.stream.XMLInputFactoryImpl`). CratonVM's
/// `newInstance()`/`newFactory()` natives return a *synthetic instance of
/// the abstract class itself*, which has no `Code` attribute for any
/// abstract method — so a virtual dispatch to `setProperty` raised
/// `AbstractMethodError: ... has no Code attribute` and aborted WildFly's
/// `XMLInputFactoryUtil.create()` boot path. Registering these natives
/// directly on the abstract class supplies a body. Property values only
/// influence parser leniency knobs our streaming reader does not honour, so
/// accepting-and-ignoring them is behavior-safe for config parsing.
fn native_factory_set_property(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// `XMLInputFactory.getProperty(String)` — report a benign default.
///
/// Real StAX returns the property's current value (or throws
/// `IllegalArgumentException` for an unsupported name). Since
/// `setProperty` is a no-op here, return `null`; callers treat an unset
/// property as "use the default".
fn native_factory_get_property(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

/// `XMLInputFactory.isPropertySupported(String)` — claim support so callers
/// proceed to `setProperty` (which we accept-and-ignore) instead of taking a
/// fallback path.
fn native_factory_is_property_supported(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

fn native_create_reader_from_input_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let stream = match args.get(1) {
        Some(Value::Object(Some(s))) => *s,
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException {
                    message: Some("createXMLStreamReader: null InputStream".to_string()),
                },
            )))
        }
    };
    let bytes = drain_input_stream(ctx, stream);
    let reader = alloc_synthetic(ctx, "javax/xml/stream/XMLStreamReader")?;
    store_state(ctx, reader, make_reader_state(&bytes));
    Ok(Some(Value::Object(Some(reader))))
}

fn native_create_reader_from_input_stream_enc(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // (InputStream, String encoding) — encoding is advisory; quick-xml
    // honours the XML declaration. Drop the encoding arg and reuse the
    // single-arg path.
    native_create_reader_from_input_stream(ctx, args)
}

fn native_create_reader_from_reader(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // (Reader) — read into a String via Reader.read(char[]) then re-encode
    // to UTF-8 bytes and parse. Use a simple character-at-a-time loop;
    // suitable for the small XML payloads StAX is typically pointed at.
    let reader_in = match args.get(1) {
        Some(Value::Object(Some(s))) => *s,
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException {
                    message: Some("createXMLStreamReader: null Reader".to_string()),
                },
            )))
        }
    };
    let mut text = String::new();
    loop {
        let res = ctx.invoke(
            "java/io/Reader",
            "read",
            "()I",
            &[Value::Object(Some(reader_in))],
        );
        match res {
            Ok(Some(Value::Int(-1))) => break,
            Ok(Some(Value::Int(c))) => {
                if let Some(ch) = char::from_u32(c as u32) {
                    text.push(ch);
                }
            }
            _ => break,
        }
        if text.len() > 64 * 1024 * 1024 {
            break;
        }
    }
    let reader = alloc_synthetic(ctx, "javax/xml/stream/XMLStreamReader")?;
    store_state(ctx, reader, make_reader_state(text.as_bytes()));
    Ok(Some(Value::Object(Some(reader))))
}

// ---------------------------------------------------------------------------
// Event API (`createXMLEventReader`).
//
// `XMLInputFactory` is abstract and `newInstance()` returns a synthetic
// instance of it, so without these natives `createXMLEventReader` dispatches to
// the abstract method and raises `AbstractMethodError: ... has no Code
// attribute`. We build the same cursor reader as the stream API, then wrap it
// in the REAL JDK `com.sun.xml.internal.stream.XMLEventReaderImpl` so the JDK's
// own event-model classes (`StartElementEvent`, `AttributeImpl`,
// `CharacterEvent`, …) are reused — no native event object model needed.
// ---------------------------------------------------------------------------

/// Build a synthetic cursor `XMLStreamReader` over the parsed events of `bytes`.
fn make_cursor_reader(
    ctx: &mut dyn NativeContext,
    bytes: &[u8],
) -> Result<ObjectRef, MethodCallFailed> {
    let reader = alloc_synthetic(ctx, "javax/xml/stream/XMLStreamReader")?;
    store_state(ctx, reader, make_reader_state(bytes));
    Ok(reader)
}

/// Drain a `java.io.Reader` into a String, char-at-a-time (small XML payloads).
fn drain_reader_to_string(ctx: &mut dyn NativeContext, reader_in: ObjectRef) -> String {
    let mut text = String::new();
    loop {
        let res = ctx.invoke(
            "java/io/Reader",
            "read",
            "()I",
            &[Value::Object(Some(reader_in))],
        );
        match res {
            Ok(Some(Value::Int(-1))) => break,
            Ok(Some(Value::Int(c))) => {
                if let Some(ch) = char::from_u32(c as u32) {
                    text.push(ch);
                }
            }
            _ => break,
        }
        if text.len() > 64 * 1024 * 1024 {
            break;
        }
    }
    text
}

/// Wrap a synthetic cursor reader in the real JDK `XMLEventReaderImpl` adapter.
/// `new_object_initialized` pins the wrapper across its `<init>` (which
/// allocates the default `XMLEventAllocatorImpl` and the first event); `cursor`
/// is rooted as an `<init>` argument and its ReaderState side-table entry is
/// keyed by GC-stable identity hash, so a collection during construction is safe.
fn wrap_in_event_reader(ctx: &mut dyn NativeContext, cursor: ObjectRef) -> MethodCallResult {
    ctx.new_object_initialized(
        "com/sun/xml/internal/stream/XMLEventReaderImpl",
        "(Ljavax/xml/stream/XMLStreamReader;)V",
        &[Value::Object(Some(cursor))],
    )
}

fn native_create_event_reader_from_input_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let stream = match args.get(1) {
        Some(Value::Object(Some(s))) => *s,
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException {
                    message: Some("createXMLEventReader: null InputStream".to_string()),
                },
            )))
        }
    };
    let bytes = drain_input_stream(ctx, stream);
    let cursor = make_cursor_reader(ctx, &bytes)?;
    wrap_in_event_reader(ctx, cursor)
}

fn native_create_event_reader_from_input_stream_enc(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // (InputStream, String encoding) — encoding advisory; reuse the 1-arg path.
    native_create_event_reader_from_input_stream(ctx, args)
}

fn native_create_event_reader_from_reader(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let reader_in = match args.get(1) {
        Some(Value::Object(Some(s))) => *s,
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException {
                    message: Some("createXMLEventReader: null Reader".to_string()),
                },
            )))
        }
    };
    let text = drain_reader_to_string(ctx, reader_in);
    let cursor = make_cursor_reader(ctx, text.as_bytes())?;
    wrap_in_event_reader(ctx, cursor)
}

/// `createXMLEventReader(javax.xml.transform.Source)`. Hibernate's
/// `PersistenceXmlParser` binds JAXB from `new StreamSource(inputStream)`,
/// whose JAXB unmarshaller calls this overload. Without it the call dispatches
/// to the abstract `XMLInputFactory.createXMLEventReader(Source)` and raises
/// `AbstractMethodError: … has no Code attribute` — 6 CV-only suite classes
/// (`jpa.persistenceunit.*`, `jpa.boot.*`, JAXB persistence.xml parsing).
///
/// We support `StreamSource` (the overwhelmingly common case): pull its
/// `InputStream`, else its `Reader`, else open its `systemId` as a URL — then
/// build the same cursor reader as the stream/reader overloads.
fn native_create_event_reader_from_source(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let source = match args.get(1) {
        Some(Value::Object(Some(s))) => *s,
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException {
                    message: Some("createXMLEventReader: null Source".to_string()),
                },
            )))
        }
    };
    let src_cls = ctx
        .class_name_of_id(ctx.class_id_of_object(source))
        .unwrap_or_default();
    // StreamSource path (and any subclass): InputStream → Reader → systemId.
    if src_cls == "javax/xml/transform/stream/StreamSource" {
        if let Ok(Some(Value::Object(Some(stream)))) = ctx.invoke(
            "javax/xml/transform/stream/StreamSource",
            "getInputStream",
            "()Ljava/io/InputStream;",
            &[Value::Object(Some(source))],
        ) {
            let bytes = drain_input_stream(ctx, stream);
            let cursor = make_cursor_reader(ctx, &bytes)?;
            return wrap_in_event_reader(ctx, cursor);
        }
        if let Ok(Some(Value::Object(Some(reader_in)))) = ctx.invoke(
            "javax/xml/transform/stream/StreamSource",
            "getReader",
            "()Ljava/io/Reader;",
            &[Value::Object(Some(source))],
        ) {
            let text = drain_reader_to_string(ctx, reader_in);
            let cursor = make_cursor_reader(ctx, text.as_bytes())?;
            return wrap_in_event_reader(ctx, cursor);
        }
        // Fall back to systemId: open it as a URL and drain the stream.
        if let Ok(Some(Value::Object(Some(sid)))) = ctx.invoke(
            "javax/xml/transform/stream/StreamSource",
            "getSystemId",
            "()Ljava/lang/String;",
            &[Value::Object(Some(source))],
        ) {
            let sid_str = ctx.read_string(sid).unwrap_or_default();
            if !sid_str.is_empty() {
                if let Ok(Some(Value::Object(Some(url)))) = ctx.new_object_initialized(
                    "java/net/URL",
                    "(Ljava/lang/String;)V",
                    &[Value::Object(Some(sid))],
                ) {
                    if let Ok(Some(Value::Object(Some(stream)))) = ctx.invoke(
                        "java/net/URL",
                        "openStream",
                        "()Ljava/io/InputStream;",
                        &[Value::Object(Some(url))],
                    ) {
                        let bytes = drain_input_stream(ctx, stream);
                        let cursor = make_cursor_reader(ctx, &bytes)?;
                        return wrap_in_event_reader(ctx, cursor);
                    }
                }
            }
        }
    }
    // DOMSource path: serialize the wrapped DOM node back to XML text via the
    // public DOM API, then build the same cursor reader. keycloak's
    // `SamlProtocolUtils` re-reads an already-parsed DOM element as a StAX event
    // stream (`createXMLEventReader(new DOMSource(element))`); without this the
    // call fell through to the unsupported-Source error.
    if src_cls == "javax/xml/transform/dom/DOMSource" {
        if let Ok(Some(Value::Object(Some(node)))) = ctx.invoke(
            "javax/xml/transform/dom/DOMSource",
            "getNode",
            "()Lorg/w3c/dom/Node;",
            &[Value::Object(Some(source))],
        ) {
            let xml = serialize_dom_node(ctx, node, 0);
            if !xml.trim().is_empty() {
                let cursor = make_cursor_reader(ctx, xml.as_bytes())?;
                return wrap_in_event_reader(ctx, cursor);
            }
        }
    }
    Err(MethodCallFailed::InternalError(VmError::Runtime(
        RuntimeError::NullPointerException {
            message: Some(format!(
                "createXMLEventReader(Source): unsupported / empty Source ({src_cls})"
            )),
        },
    )))
}

/// XML-escape text node content.
fn xml_escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// XML-escape an attribute value (also quotes).
fn xml_escape_attr(s: &str) -> String {
    xml_escape_text(s).replace('"', "&quot;")
}

/// Read a `()Ljava/lang/String;` DOM accessor, returning "" on null/failure.
fn dom_str(ctx: &mut dyn NativeContext, node: ObjectRef, method: &str) -> String {
    match ctx.invoke_virtual(node, method, "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    }
}

/// Read a `()I` DOM accessor, returning 0 on failure.
fn dom_int(ctx: &mut dyn NativeContext, node: ObjectRef, method: &str) -> i32 {
    match ctx.invoke_virtual(node, method, "()I", &[]) {
        Ok(Some(v)) => v.as_int().unwrap_or(0),
        _ => 0,
    }
}

/// Serialize a synthetic DOM node back to XML text using only the public DOM
/// API (so it does not depend on the internal node layout). Used by the
/// `createXMLEventReader(DOMSource)` path. `depth` guards pathological recursion.
fn serialize_dom_node(ctx: &mut dyn NativeContext, node: ObjectRef, depth: u32) -> String {
    if depth > 256 {
        return String::new();
    }
    // `node` is a parameter read by every arm below, and each intervening call
    // is real DOM Java that can collect. Pin once; re-derive before each use.
    let node_pin = ctx.pin_native_root(node);
    let ntype = match ctx.invoke_virtual(node, "getNodeType", "()S", &[]) {
        Ok(Some(v)) => v.as_int().unwrap_or(0),
        _ => 0,
    };
    let node = ctx.read_native_pin(node_pin, node);
    match ntype {
        1 => {
            // ELEMENT
            let mut tag = dom_str(ctx, node, "getTagName");
            let node = ctx.read_native_pin(node_pin, node);
            if tag.is_empty() {
                tag = dom_str(ctx, node, "getNodeName");
            }
            let node = ctx.read_native_pin(node_pin, node);
            if tag.is_empty() {
                return String::new();
            }
            let mut out = String::new();
            out.push('<');
            out.push_str(&tag);
            // attributes (xmlns declarations are ordinary attrs here)
            if let Ok(Some(Value::Object(Some(attrs)))) =
                ctx.invoke_virtual(node, "getAttributes", "()Lorg/w3c/dom/NamedNodeMap;", &[])
            {
                let n = dom_int(ctx, attrs, "getLength");
                for i in 0..n {
                    if let Ok(Some(Value::Object(Some(attr)))) =
                        ctx.invoke_virtual(attrs, "item", "(I)Lorg/w3c/dom/Node;", &[Value::Int(i)])
                    {
                        let an = dom_str(ctx, attr, "getNodeName");
                        let av = dom_str(ctx, attr, "getNodeValue");
                        if !an.is_empty() {
                            out.push(' ');
                            out.push_str(&an);
                            out.push_str("=\"");
                            out.push_str(&xml_escape_attr(&av));
                            out.push('"');
                        }
                    }
                }
            }
            // children
            let mut inner = String::new();
            if let Ok(Some(Value::Object(Some(nl)))) =
                ctx.invoke_virtual(node, "getChildNodes", "()Lorg/w3c/dom/NodeList;", &[])
            {
                let cn = dom_int(ctx, nl, "getLength");
                for i in 0..cn {
                    if let Ok(Some(Value::Object(Some(child)))) =
                        ctx.invoke_virtual(nl, "item", "(I)Lorg/w3c/dom/Node;", &[Value::Int(i)])
                    {
                        inner.push_str(&serialize_dom_node(ctx, child, depth + 1));
                    }
                }
            }
            if inner.is_empty() {
                out.push_str("></");
                out.push_str(&tag);
                out.push('>');
            } else {
                out.push('>');
                out.push_str(&inner);
                out.push_str("</");
                out.push_str(&tag);
                out.push('>');
            }
            out
        }
        // TEXT / CDATA
        3 | 4 => {
            let node = ctx.read_native_pin(node_pin, node);
            xml_escape_text(&dom_str(ctx, node, "getNodeValue"))
        }
        // DOCUMENT: serialize element children
        9 => {
            let mut out = String::new();
            let node = ctx.read_native_pin(node_pin, node);
            if let Ok(Some(Value::Object(Some(nl)))) =
                ctx.invoke_virtual(node, "getChildNodes", "()Lorg/w3c/dom/NodeList;", &[])
            {
                // `nl` is held across `getLength` and every `item(i)`.
                let nl_pin = ctx.pin_native_root(nl);
                let cn = dom_int(ctx, nl, "getLength");
                let mut nl = nl;
                for i in 0..cn {
                    nl = ctx.read_native_pin(nl_pin, nl);
                    if let Ok(Some(Value::Object(Some(child)))) =
                        ctx.invoke_virtual(nl, "item", "(I)Lorg/w3c/dom/Node;", &[Value::Int(i)])
                    {
                        out.push_str(&serialize_dom_node(ctx, child, depth + 1));
                    }
                }
            }
            out
        }
        // COMMENT and others: skip
        _ => String::new(),
    }
}

/// `XMLInputFactory.createFilteredReader(XMLEventReader, EventFilter)` — mirror the
/// real `XMLInputFactoryImpl`, which returns `new EventFilterSupport(reader, filter)`.
/// Our reader is already a real JDK `XMLEventReaderImpl` (see `wrap_in_event_reader`),
/// and `EventFilterSupport` is plain JDK bytecode that delegates to it and drops the
/// events the filter rejects — so this runs entirely on real classes. Without the
/// native the abstract base method raised `AbstractMethodError` (keycloak SAML parsing
/// via `StaxParserUtil`, 4 CV-only suite classes).
fn native_create_filtered_event_reader(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let reader = args.get(1).copied().unwrap_or(Value::Object(None));
    let filter = args.get(2).copied().unwrap_or(Value::Object(None));
    match ctx.new_object_initialized(
        "com/sun/xml/internal/stream/EventFilterSupport",
        "(Ljavax/xml/stream/XMLEventReader;Ljavax/xml/stream/EventFilter;)V",
        &[reader, filter],
    ) {
        Ok(Some(v)) => Ok(Some(v)),
        // Fall back to the unfiltered reader rather than AbstractMethodError.
        _ => Ok(Some(reader)),
    }
}

/// `XMLStreamReader.getPrefix()` — current element's prefix ("" when
/// unprefixed). Previously hard-coded to null, which made the event-API
/// `XMLEventAllocatorImpl.getQName` (`new QName(ns, local, prefix)`) throw
/// `IllegalArgumentException` on the null prefix.
fn native_get_prefix(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    current_string(ctx, args, |e| e.prefix.clone())
}

/// `XMLStreamReader.getProperty(String)` — the event allocator and the
/// `XMLEventReaderImpl` ctor probe a couple of StAX properties. Report
/// non-namespace-aware (`Boolean.FALSE`) so the allocator skips its
/// namespace-context path (which casts `getNamespaceContext()` to the internal
/// Xerces `NamespaceContextWrapper` we cannot synthesize). Element QNames still
/// carry their namespace via `getNamespaceURI()`, so JAXB binds by QName and the
/// (null) event namespace context is tolerated by `UnmarshallingContext` (every
/// use is ifnull-guarded). All other properties (notably `ALLOCATOR`) report
/// null, so the ctor defaults to a fresh `XMLEventAllocatorImpl`.
fn native_get_property(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let name = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    if name == "javax.xml.stream.isNamespaceAware" {
        // Report namespace-aware so `XMLEventAllocatorImpl` runs its
        // `fillNamespaceAttributes` path — attaching each START_ELEMENT's own
        // `xmlns`/`xmlns:p` declarations (via getNamespaceCount/URI/Prefix) as
        // Namespace events. Without this, an `XMLEventWriter` re-serializing the
        // events drops a literal default `xmlns="…"`. The sibling
        // `setNamespaceContext` cast is satisfied by `getNamespaceContext()`
        // below returning a real `NamespaceContextWrapper`.
        return ctx.invoke(
            "java/lang/Boolean",
            "valueOf",
            "(Z)Ljava/lang/Boolean;",
            &[Value::Int(1)],
        );
    }
    Ok(Some(Value::Object(None)))
}

/// `XMLStreamReader.getNamespaceContext()` — the event allocator's
/// `setNamespaceContext` casts the result to the concrete xerces
/// `NamespaceContextWrapper`, so return a real (empty) one:
/// `new NamespaceContextWrapper(new NamespaceSupport())`. The event stream's
/// own namespace declarations are carried separately (fillNamespaceAttributes →
/// getNamespaceCount/URI/Prefix); this context only needs to exist and be of
/// the expected type so the allocator does not throw.
fn native_get_namespace_context(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let ns = match ctx.new_object_initialized(
        "com/sun/org/apache/xerces/internal/util/NamespaceSupport",
        "()V",
        &[],
    ) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return Ok(Some(Value::Object(None))),
    };
    match ctx.new_object_initialized(
        "com/sun/org/apache/xerces/internal/util/NamespaceContextWrapper",
        "(Lcom/sun/org/apache/xerces/internal/util/NamespaceSupport;)V",
        &[Value::Object(Some(ns))],
    ) {
        Ok(Some(v)) => Ok(Some(v)),
        _ => Ok(Some(Value::Object(None))),
    }
}

/// `XMLStreamReader.getAttributeType(int)` — StAX reports "CDATA" for an
/// ordinary (non-DTD-typed) attribute. The allocator's `fillAttributes` calls
/// this for every attribute.
fn native_get_attribute_type(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(ctx.create_string("CDATA")))))
}

/// `XMLStreamReader.isAttributeSpecified(int)` — our reader only surfaces
/// attributes literally present in the source, so each one is "specified".
fn native_is_attribute_specified(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

/// `XMLStreamReader.getPITarget()` — leading token of a PROCESSING_INSTRUCTION
/// payload (we store the whole PI text in `local_name`).
fn native_get_pi_target(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    current_string(ctx, args, |e| {
        e.local_name
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string()
    })
}

/// `XMLStreamReader.getPIData()` — PI payload after the target token.
fn native_get_pi_data(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    current_string(ctx, args, |e| {
        match e.local_name.split_once(char::is_whitespace) {
            Some((_, data)) => data.trim_start().to_string(),
            None => String::new(),
        }
    })
}

fn native_has_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let has = with_state(ctx, this, |s| {
        let next = (s.cursor + 1) as usize;
        next < s.events.len()
    })
    .unwrap_or(false);
    Ok(Some(Value::Int(if has { 1 } else { 0 })))
}

fn native_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let kind = with_state(ctx, this, |s| {
        s.cursor += 1;
        s.current().map(|e| e.kind).unwrap_or(END_DOCUMENT)
    })
    .unwrap_or(END_DOCUMENT);
    Ok(Some(Value::Int(kind)))
}

fn native_get_event_type(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let kind = with_state(ctx, this, |s| {
        s.current().map(|e| e.kind).unwrap_or(START_DOCUMENT)
    })
    .unwrap_or(START_DOCUMENT);
    Ok(Some(Value::Int(kind)))
}

fn current_string<F: FnOnce(&StaxEvent) -> String>(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    f: F,
) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let s = with_state(ctx, this, |st| st.current().map(f).unwrap_or_default()).unwrap_or_default();
    let obj = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(obj))))
}

fn native_get_local_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    current_string(ctx, args, |e| e.local_name.clone())
}

fn native_get_text(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    current_string(ctx, args, |e| e.text.clone())
}

fn native_get_namespace_uri_noargs(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    current_string(ctx, args, |e| e.namespace_uri.clone())
}

fn native_get_attribute_value_named(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    // args: [this, namespaceURI (nullable), localName]
    let ns = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let local = match args.get(2) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    let found = with_state(ctx, this, |st| {
        st.current().and_then(|e| {
            e.attributes
                .iter()
                .find(|a| a.local_name == local && (ns.is_empty() || a.namespace_uri == ns))
                .map(|a| a.value.clone())
        })
    })
    .flatten();
    match found {
        Some(s) => Ok(Some(Value::Object(Some(ctx.create_string(&s))))),
        None => Ok(Some(Value::Object(None))),
    }
}

fn native_get_attribute_value_indexed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let idx = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => return Ok(Some(Value::Object(None))),
    };
    let found = with_state(ctx, this, |st| {
        st.current()
            .and_then(|e| e.attributes.get(idx as usize).map(|a| a.value.clone()))
    })
    .flatten();
    match found {
        Some(s) => Ok(Some(Value::Object(Some(ctx.create_string(&s))))),
        None => Ok(Some(Value::Object(None))),
    }
}

fn native_get_attribute_local_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let idx = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => return Ok(Some(Value::Object(None))),
    };
    let found = with_state(ctx, this, |st| {
        st.current()
            .and_then(|e| e.attributes.get(idx as usize).map(|a| a.local_name.clone()))
    })
    .flatten();
    match found {
        Some(s) => Ok(Some(Value::Object(Some(ctx.create_string(&s))))),
        None => Ok(Some(Value::Object(None))),
    }
}

fn native_get_attribute_count(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let n = with_state(ctx, this, |st| {
        st.current().map(|e| e.attributes.len() as i32).unwrap_or(0)
    })
    .unwrap_or(0);
    Ok(Some(Value::Int(n)))
}

fn native_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    // `close()` is allowed on a never-seen receiver (no-op) — spec
    // says close() must not throw, so we deliberately do not invoke
    // `require_state` here.
    drop_state(ctx, this);
    Ok(None)
}

fn native_is_kind(ctx: &mut dyn NativeContext, args: &[Value], expected: i32) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let yes = with_state(ctx, this, |st| {
        st.current().map(|e| e.kind == expected).unwrap_or(false)
    })
    .unwrap_or(false);
    Ok(Some(Value::Int(if yes { 1 } else { 0 })))
}

fn native_is_start_element(c: &mut dyn NativeContext, a: &[Value]) -> MethodCallResult {
    native_is_kind(c, a, START_ELEMENT)
}
fn native_is_end_element(c: &mut dyn NativeContext, a: &[Value]) -> MethodCallResult {
    native_is_kind(c, a, END_ELEMENT)
}
fn native_is_characters(c: &mut dyn NativeContext, a: &[Value]) -> MethodCallResult {
    native_is_kind(c, a, CHARACTERS)
}
fn native_is_whitespace(ctx: &mut dyn NativeContext, a: &[Value]) -> MethodCallResult {
    let this = this_obj(a)?;
    require_state(ctx, this)?;
    let yes = with_state(ctx, this, |st| {
        st.current()
            .map(|e| {
                (e.kind == CHARACTERS || e.kind == CDATA)
                    && !e.text.is_empty()
                    && e.text.chars().all(|c| c.is_whitespace())
            })
            .unwrap_or(false)
    })
    .unwrap_or(false);
    Ok(Some(Value::Int(if yes { 1 } else { 0 })))
}

// ---------------------------------------------------------------------------
// Registration entry point.
// ---------------------------------------------------------------------------

pub fn register(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    // The JDK Xerces StAX event impl's `EndElementEvent.getNamespaces()` is
    // hard-wired to discard `fNamespaces.iterator()` and always return an empty
    // `ReadOnlyIterator` (verified in the JDK 25 bytecode). The real default
    // provider on a Woodstox classpath (HotSpot's choice for the Spring suite)
    // returns the namespaces that go out of scope, which Spring's
    // StaxEventXMLReader.handleEndElement turns into `endPrefixMapping` callbacks.
    // Our synthetic cursor reports those namespaces (getNamespaceCount/Prefix/URI
    // on END_ELEMENT) and the allocator's `fillNamespaceAttributes` populates
    // `fNamespaces` correctly — only this final getter drops them. Shadow it with
    // the spec-correct behaviour (force-dispatched via
    // `interpreter::force_native_over_real_jdk_bytecode`).
    registry.register(
        "com/sun/xml/internal/stream/events/EndElementEvent",
        "getNamespaces",
        "()Ljava/util/Iterator;",
        native_end_element_get_namespaces,
    );
    // Factory entry points.
    registry.register(
        "javax/xml/stream/XMLInputFactory",
        "newInstance",
        "()Ljavax/xml/stream/XMLInputFactory;",
        native_factory_new_instance,
    );
    registry.register(
        "javax/xml/stream/XMLInputFactory",
        "newFactory",
        "()Ljavax/xml/stream/XMLInputFactory;",
        native_factory_new_instance,
    );

    // Factory configuration. `XMLInputFactory` is abstract — without these
    // its abstract `setProperty`/`getProperty`/`isPropertySupported` methods
    // have no Code attribute and a virtual dispatch raises
    // `AbstractMethodError`, aborting WildFly's `XMLInputFactoryUtil.create()`.
    registry.register(
        "javax/xml/stream/XMLInputFactory",
        "setProperty",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
        native_factory_set_property,
    );
    registry.register(
        "javax/xml/stream/XMLInputFactory",
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        native_factory_get_property,
    );
    registry.register(
        "javax/xml/stream/XMLInputFactory",
        "isPropertySupported",
        "(Ljava/lang/String;)Z",
        native_factory_is_property_supported,
    );
    registry.register(
        "javax/xml/stream/XMLInputFactory",
        "setXMLResolver",
        "(Ljavax/xml/stream/XMLResolver;)V",
        native_factory_set_property,
    );
    registry.register(
        "javax/xml/stream/XMLInputFactory",
        "setXMLReporter",
        "(Ljavax/xml/stream/XMLReporter;)V",
        native_factory_set_property,
    );
    registry.register(
        "javax/xml/stream/XMLInputFactory",
        "setEventAllocator",
        "(Ljavax/xml/stream/util/XMLEventAllocator;)V",
        native_factory_set_property,
    );

    registry.register(
        "javax/xml/stream/XMLInputFactory",
        "createXMLStreamReader",
        "(Ljava/io/InputStream;)Ljavax/xml/stream/XMLStreamReader;",
        native_create_reader_from_input_stream,
    );
    registry.register(
        "javax/xml/stream/XMLInputFactory",
        "createXMLStreamReader",
        "(Ljava/io/InputStream;Ljava/lang/String;)Ljavax/xml/stream/XMLStreamReader;",
        native_create_reader_from_input_stream_enc,
    );
    registry.register(
        "javax/xml/stream/XMLInputFactory",
        "createXMLStreamReader",
        "(Ljava/io/Reader;)Ljavax/xml/stream/XMLStreamReader;",
        native_create_reader_from_reader,
    );

    // Event API entry points — wrap the cursor reader in the real JDK
    // XMLEventReaderImpl. Without these, the abstract `createXMLEventReader`
    // raised AbstractMethodError (our synthetic factory has no Code attribute
    // for it), blocking JAXB unmarshalling from an XMLEventReader.
    registry.register(
        "javax/xml/stream/XMLInputFactory",
        "createXMLEventReader",
        "(Ljava/io/InputStream;)Ljavax/xml/stream/XMLEventReader;",
        native_create_event_reader_from_input_stream,
    );
    registry.register(
        "javax/xml/stream/XMLInputFactory",
        "createXMLEventReader",
        "(Ljava/io/InputStream;Ljava/lang/String;)Ljavax/xml/stream/XMLEventReader;",
        native_create_event_reader_from_input_stream_enc,
    );
    registry.register(
        "javax/xml/stream/XMLInputFactory",
        "createXMLEventReader",
        "(Ljava/io/Reader;)Ljavax/xml/stream/XMLEventReader;",
        native_create_event_reader_from_reader,
    );
    registry.register(
        "javax/xml/stream/XMLInputFactory",
        "createXMLEventReader",
        "(Ljavax/xml/transform/Source;)Ljavax/xml/stream/XMLEventReader;",
        native_create_event_reader_from_source,
    );
    // createFilteredReader(XMLEventReader, EventFilter) — abstract on the base
    // XMLInputFactory; without this native our synthetic factory raised
    // AbstractMethodError. Mirrors the real impl (`new EventFilterSupport(...)`).
    registry.register(
        "javax/xml/stream/XMLInputFactory",
        "createFilteredReader",
        "(Ljavax/xml/stream/XMLEventReader;Ljavax/xml/stream/EventFilter;)Ljavax/xml/stream/XMLEventReader;",
        native_create_filtered_event_reader,
    );

    // Reader cursor methods (interface-keyed; native dispatch matches on
    // the receiver's declared class which is XMLStreamReader for our
    // synthetic objects).
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "hasNext",
        "()Z",
        native_has_next,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "next",
        "()I",
        native_next,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getEventType",
        "()I",
        native_get_event_type,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getLocalName",
        "()Ljava/lang/String;",
        native_get_local_name,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getText",
        "()Ljava/lang/String;",
        native_get_text,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getNamespaceURI",
        "()Ljava/lang/String;",
        native_get_namespace_uri_noargs,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getAttributeValue",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        native_get_attribute_value_named,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getAttributeValue",
        "(I)Ljava/lang/String;",
        native_get_attribute_value_indexed,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getAttributeLocalName",
        "(I)Ljava/lang/String;",
        native_get_attribute_local_name,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getAttributePrefix",
        "(I)Ljava/lang/String;",
        native_get_attribute_prefix,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getTextCharacters",
        "()[C",
        native_get_text_characters,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getTextStart",
        "()I",
        native_get_text_start,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getTextLength",
        "()I",
        native_get_text_length,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getAttributeCount",
        "()I",
        native_get_attribute_count,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "close",
        "()V",
        native_close,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "isStartElement",
        "()Z",
        native_is_start_element,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "isEndElement",
        "()Z",
        native_is_end_element,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "isCharacters",
        "()Z",
        native_is_characters,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "isWhiteSpace",
        "()Z",
        native_is_whitespace,
    );
    // require(int type, String ns, String localName) — spec: validates the
    // current cursor matches the expected event. WildFly's
    // ParseUtils.requireSingleAttribute / standalone.xml parser invokes this
    // repeatedly during boot; the JDK declares it abstract on XMLStreamReader
    // so dispatch hits "XMLStreamReader.require(ILjava/lang/String;Ljava/lang/String;)V
    // has no Code attribute" without a native. Validate type+localName when
    // we have state; throw XMLStreamException with the standard message on
    // mismatch so callers' error paths work; treat null params as wildcards.
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "require",
        "(ILjava/lang/String;Ljava/lang/String;)V",
        native_require,
    );
    // Round of XMLStreamReader convenience APIs declared abstract on the
    // interface — WildFly / KC16 XML config parsing hits each of these in
    // turn during standalone.xml boot.
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "nextTag",
        "()I",
        native_next_tag,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getElementText",
        "()Ljava/lang/String;",
        native_get_element_text,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getName",
        "()Ljavax/xml/namespace/QName;",
        native_get_qname,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getAttributeName",
        "(I)Ljavax/xml/namespace/QName;",
        native_get_attr_qname,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getAttributeNamespace",
        "(I)Ljava/lang/String;",
        native_get_attr_namespace,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "hasName",
        "()Z",
        native_has_name,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "hasText",
        "()Z",
        native_has_text,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getPrefix",
        "()Ljava/lang/String;",
        native_get_prefix,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getNamespaceContext",
        "()Ljavax/xml/namespace/NamespaceContext;",
        native_get_namespace_context,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getNamespaceCount",
        "()I",
        native_get_namespace_count,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getNamespaceURI",
        "(I)Ljava/lang/String;",
        native_get_namespace_uri_indexed,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getNamespacePrefix",
        "(I)Ljava/lang/String;",
        native_get_namespace_prefix_indexed,
    );
    // Event-API support: XMLEventReaderImpl's ctor + XMLEventAllocatorImpl call
    // these on the wrapped cursor reader while building XMLEvent objects.
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        native_get_property,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getAttributeType",
        "(I)Ljava/lang/String;",
        native_get_attribute_type,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "isAttributeSpecified",
        "(I)Z",
        native_is_attribute_specified,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getPITarget",
        "()Ljava/lang/String;",
        native_get_pi_target,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getPIData",
        "()Ljava/lang/String;",
        native_get_pi_data,
    );
    // isStandalone / standaloneSet — read from the XML declaration instead of
    // reporting a flat `false`. A document declaring `standalone="yes"` tells
    // consumers no external markup declarations need to be honoured; answering
    // `false` (and `standaloneSet()==false`) made every such document look
    // like it had no declaration at all, so DTD-aware consumers took the
    // external-subset path for a document that had explicitly opted out.
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "isStandalone",
        "()Z",
        native_is_standalone,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "standaloneSet",
        "()Z",
        native_standalone_set,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getCharacterEncodingScheme",
        "()Ljava/lang/String;",
        native_get_character_encoding_scheme,
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getEncoding",
        "()Ljava/lang/String;",
        |ctx, _args| Ok(Some(Value::Object(Some(ctx.create_string("UTF-8"))))),
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getVersion",
        "()Ljava/lang/String;",
        |ctx, _args| Ok(Some(Value::Object(Some(ctx.create_string("1.0"))))),
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getLocation",
        "()Ljavax/xml/stream/Location;",
        native_get_location,
    );
    // Location is an interface — XMLStreamException.<init>(message, Location)
    // (WildFly's ParseUtils.unexpectedElement et al.) reads getLineNumber /
    // getColumnNumber off it for the formatted message. Without these
    // natives the constructor throws AbstractMethodError before the user's
    // exception even propagates, masking the real parsing failure.
    //
    // synthetic-stub reimplemented: getLineNumber/getColumnNumber/getCharacterOffset
    // previously returned a fixed -1 (placeholder). The StAX cursor is real
    // (quick-xml), and quick-xml exposes the byte offset of each event via
    // `Reader::buffer_position()`; we now capture that offset in `parse_to_events`
    // and derive 1-based line/column from the source bytes. `getLocation()` stamps
    // the current event's (line, column, offset) into a side-table keyed by the
    // Location object, and these accessors read it back — real position data.
    registry.register(
        "javax/xml/stream/Location",
        "getLineNumber",
        "()I",
        native_loc_line,
    );
    registry.register(
        "javax/xml/stream/Location",
        "getColumnNumber",
        "()I",
        native_loc_column,
    );
    registry.register(
        "javax/xml/stream/Location",
        "getCharacterOffset",
        "()I",
        native_loc_offset,
    );
    // FLAG: getPublicId/getSystemId remain null. quick-xml does NOT track a
    // public/system identifier for the source, and our reader is fed from raw
    // bytes / an InputStream with no associated SYSTEM URI, so there is no real
    // data to surface here. Per spec, returning null for an unknown public/system
    // id is permitted. Left as null deliberately (not a fabricated value).
    let null_str: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        |_ctx, _args| Ok(Some(Value::Object(None)));
    registry.register(
        "javax/xml/stream/Location",
        "getPublicId",
        "()Ljava/lang/String;",
        null_str,
    );
    registry.register(
        "javax/xml/stream/Location",
        "getSystemId",
        "()Ljava/lang/String;",
        null_str,
    );
    registry.set_category(__prev_cat);
}

/// `XMLStreamReader.getLocation()` — allocate a Location object and record the
/// current event's real (line, column, byte-offset) in the Location side-table,
/// keyed by the Location object's GC-stable identity hash. The receiver
/// (`args[0]`) is the reader, whose current event carries the position captured
/// during `parse_to_events`.
fn native_get_location(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let reader = this_obj(args)?;
    // Best-effort: if the reader has no state (never initialized), fall back to
    // (-1,-1,-1) which matches the "unknown location" convention.
    let pos = with_state(ctx, reader, |s| {
        s.current()
            .map(|e| (e.line, e.column, e.char_offset))
            .unwrap_or((-1, -1, -1))
    })
    .unwrap_or((-1, -1, -1));
    let loc = crate::try_alloc_concurrent_synthetic(ctx, "javax/xml/stream/Location", 4)?;
    location_table().lock().insert(obj_key(ctx, loc), pos);
    Ok(Some(Value::Object(Some(loc))))
}

/// Look up the (line, column, offset) recorded for a Location object; returns
/// (-1,-1,-1) for a Location we did not produce (StAX "unknown" convention).
fn location_pos(ctx: &dyn NativeContext, loc: ObjectRef) -> (i32, i32, i32) {
    location_table()
        .lock()
        .get(&obj_key(ctx, loc))
        .copied()
        .unwrap_or((-1, -1, -1))
}

fn native_loc_line(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    Ok(Some(Value::Int(location_pos(ctx, this).0)))
}

fn native_loc_column(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    Ok(Some(Value::Int(location_pos(ctx, this).1)))
}

fn native_loc_offset(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    Ok(Some(Value::Int(location_pos(ctx, this).2)))
}

fn native_next_tag(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    // Spec: skip whitespace/comment/PI/CDATA-only-whitespace until
    // START_ELEMENT or END_ELEMENT. Throw on encountering anything else.
    loop {
        let kind = with_state(ctx, this, |s| {
            s.cursor += 1;
            s.current().map(|e| e.kind).unwrap_or(END_DOCUMENT)
        })
        .unwrap_or(END_DOCUMENT);
        match kind {
            START_ELEMENT | END_ELEMENT => return Ok(Some(Value::Int(kind))),
            CHARACTERS | CDATA | COMMENT | SPACE | PROCESSING_INSTRUCTION => continue,
            END_DOCUMENT => return Ok(Some(Value::Int(END_DOCUMENT))),
            _ => continue,
        }
    }
}

fn native_get_element_text(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    // Spec: collects text content until the matching END_ELEMENT.
    let mut text = String::new();
    loop {
        let (kind, t) = with_state(ctx, this, |s| {
            s.cursor += 1;
            s.current()
                .map(|e| (e.kind, e.text.clone()))
                .unwrap_or((END_DOCUMENT, String::new()))
        })
        .unwrap_or((END_DOCUMENT, String::new()));
        match kind {
            CHARACTERS | CDATA | SPACE => text.push_str(&t),
            END_ELEMENT | END_DOCUMENT => break,
            _ => continue,
        }
    }
    Ok(Some(Value::Object(Some(ctx.create_string(&text)))))
}

fn native_get_qname(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let (local, ns, prefix) = with_state(ctx, this, |s| match s.current() {
        Some(e) => (
            e.local_name.clone(),
            e.namespace_uri.clone(),
            e.prefix.clone(),
        ),
        None => (String::new(), String::new(), String::new()),
    })
    .unwrap_or_default();
    let qname = crate::try_alloc_concurrent_synthetic(ctx, "javax/xml/namespace/QName", 3)?;
    let local_s = ctx.create_string(&local);
    let ns_s = ctx.create_string(&ns);
    // Carry the real element prefix (mirrors native_get_attr_qname): SAX qName
    // construction (AbstractStaxXMLReader.toQualifiedName) needs prefix:localPart,
    // so hardcoding "" dropped the prefix from StaxEventXMLReader/StaxStreamXMLReader
    // output (StaxEventXMLReaderTests / StaxStreamXMLReaderTests namespace tests).
    let prefix_s = ctx.create_string(&prefix);
    ctx.set_field_by_name(qname, "localPart", Value::Object(Some(local_s)));
    ctx.set_field_by_name(qname, "namespaceURI", Value::Object(Some(ns_s)));
    ctx.set_field_by_name(qname, "prefix", Value::Object(Some(prefix_s)));
    Ok(Some(Value::Object(Some(qname))))
}

fn native_get_attr_qname(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let idx = args
        .get(1)
        .and_then(|v| match v {
            Value::Int(n) => Some(*n as usize),
            _ => None,
        })
        .unwrap_or(0);
    let (local, ns, prefix) = with_state(ctx, this, |s| match s.current() {
        Some(e) if idx < e.attributes.len() => {
            let a = &e.attributes[idx];
            (
                a.local_name.clone(),
                a.namespace_uri.clone(),
                a.prefix.clone(),
            )
        }
        _ => (String::new(), String::new(), String::new()),
    })
    .unwrap_or_default();
    let qname = crate::try_alloc_concurrent_synthetic(ctx, "javax/xml/namespace/QName", 3)?;
    let local_s = ctx.create_string(&local);
    let ns_s = ctx.create_string(&ns);
    // Carry the real prefix (not ""): a prefixed attribute in a non-empty
    // namespace must round-trip its prefix or an XMLEventWriter rejects it.
    let prefix_s = ctx.create_string(&prefix);
    ctx.set_field_by_name(qname, "localPart", Value::Object(Some(local_s)));
    ctx.set_field_by_name(qname, "namespaceURI", Value::Object(Some(ns_s)));
    ctx.set_field_by_name(qname, "prefix", Value::Object(Some(prefix_s)));
    Ok(Some(Value::Object(Some(qname))))
}

fn native_get_attr_namespace(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let idx = args
        .get(1)
        .and_then(|v| match v {
            Value::Int(n) => Some(*n as usize),
            _ => None,
        })
        .unwrap_or(0);
    let ns = with_state(ctx, this, |s| match s.current() {
        Some(e) if idx < e.attributes.len() => e.attributes[idx].namespace_uri.clone(),
        _ => String::new(),
    })
    .unwrap_or_default();
    Ok(Some(Value::Object(Some(ctx.create_string(&ns)))))
}

/// `XMLStreamReader.getAttributePrefix(int)` — prefix of the i-th attribute on the
/// current START_ELEMENT ("" when unprefixed). Mirrors `native_get_attr_namespace`.
/// Spring's `StaxStreamXMLReader.handleStartElement` calls it for prefixed
/// attributes; without it the abstract interface method had no body
/// (AbstractMethodError) — StaxStreamXMLReaderTests namespace tests.
fn native_get_attribute_prefix(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let idx = args
        .get(1)
        .and_then(|v| match v {
            Value::Int(n) => Some(*n as usize),
            _ => None,
        })
        .unwrap_or(0);
    let prefix = with_state(ctx, this, |s| match s.current() {
        Some(e) if idx < e.attributes.len() => e.attributes[idx].prefix.clone(),
        _ => String::new(),
    })
    .unwrap_or_default();
    Ok(Some(Value::Object(Some(ctx.create_string(&prefix)))))
}

/// `XMLStreamReader.getTextCharacters()` — current CHARACTERS/CDATA/COMMENT payload
/// as a fresh exact-length char[]. Spring's `StaxStreamXMLReader.handleCharacters`/
/// `handleComment` use this (+ getTextStart/getTextLength) instead of getText();
/// the interface method had no body (AbstractMethodError) before this.
fn native_get_text_characters(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let text = with_state(ctx, this, |s| match s.current() {
        Some(e) => e.text.clone(),
        None => String::new(),
    })
    .unwrap_or_default();
    let units: Vec<u16> = text.encode_utf16().collect();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Char, units.len());
    for (i, &ch) in units.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(ch as i32));
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// `XMLStreamReader.getTextStart()` — offset into the char[] from getTextCharacters().
/// We return a fresh exact-length array, so the start is always 0.
fn native_get_text_start(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    Ok(Some(Value::Int(0)))
}

/// `XMLStreamReader.getTextLength()` — length (UTF-16 code units) of the current
/// event's text payload (pairs with getTextCharacters/getTextStart).
fn native_get_text_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let len = with_state(ctx, this, |s| match s.current() {
        Some(e) => e.text.encode_utf16().count(),
        None => 0,
    })
    .unwrap_or(0);
    Ok(Some(Value::Int(len as i32)))
}

fn native_get_character_encoding_scheme(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let scheme = with_state(ctx, this, |s| s.character_encoding_scheme.clone()).flatten();
    Ok(Some(Value::Object(
        scheme.map(|encoding| ctx.create_string(&encoding)),
    )))
}

/// `XMLStreamReader.isStandalone()` — the value of the declaration's
/// `standalone` pseudo-attribute; false when it was not declared.
fn native_is_standalone(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let standalone = with_state(ctx, this, |s| s.standalone).flatten();
    Ok(Some(Value::Int(i32::from(standalone == Some(true)))))
}

/// `XMLStreamReader.standaloneSet()` — whether the declaration carried a
/// `standalone` pseudo-attribute at all.
fn native_standalone_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let declared = with_state(ctx, this, |s| s.standalone.is_some()).unwrap_or(false);
    Ok(Some(Value::Int(i32::from(declared))))
}

/// `com.sun.xml.internal.stream.events.EndElementEvent.getNamespaces()` — return
/// the namespaces that go out of scope at this end tag. The JDK body always
/// returns an empty `ReadOnlyIterator` (it computes `fNamespaces.iterator()` then
/// discards it); that drops the `endPrefixMapping` callbacks Spring's
/// StaxEventXMLReader derives from `EndElement.getNamespaces()`. Return the real
/// `fNamespaces` list's iterator instead (empty iterator when the field is
/// null/absent), matching a correct StAX provider (Woodstox on HotSpot).
fn native_end_element_get_namespaces(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = this_obj(args)?;
    if let Value::Object(Some(list)) = ctx.get_field_by_name(this, "fNamespaces") {
        return ctx.invoke(
            "java/util/List",
            "iterator",
            "()Ljava/util/Iterator;",
            &[Value::Object(Some(list))],
        );
    }
    // No backing list — mirror the JDK's empty-iterator return.
    ctx.invoke(
        "java/util/Collections",
        "emptyIterator",
        "()Ljava/util/Iterator;",
        &[],
    )
}

/// `XMLStreamReader.getNamespaceCount()` — number of `xmlns`/`xmlns:p`
/// declarations that appear LITERALLY on the current START/END_ELEMENT (not the
/// in-scope total). Used by `XMLEventAllocatorImpl.fillNamespaceAttributes`.
fn native_get_namespace_count(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let n = with_state(ctx, this, |s| {
        s.current().map(|e| e.namespaces.len()).unwrap_or(0)
    })
    .unwrap_or(0);
    Ok(Some(Value::Int(n as i32)))
}

/// `XMLStreamReader.getNamespaceURI(int)` — URI of the i-th namespace
/// declaration on the current element.
fn native_get_namespace_uri_indexed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as usize;
    let uri = with_state(ctx, this, |s| match s.current() {
        Some(e) if idx < e.namespaces.len() => e.namespaces[idx].1.clone(),
        _ => String::new(),
    })
    .unwrap_or_default();
    Ok(Some(Value::Object(Some(ctx.create_string(&uri)))))
}

/// `XMLStreamReader.getNamespacePrefix(int)` — prefix of the i-th namespace
/// declaration on the current element. The default namespace (`xmlns="…"`) is
/// reported as `""`; `fillNamespaceAttributes` treats `""`/null identically.
fn native_get_namespace_prefix_indexed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as usize;
    let prefix = with_state(ctx, this, |s| match s.current() {
        Some(e) if idx < e.namespaces.len() => e.namespaces[idx].0.clone(),
        _ => String::new(),
    })
    .unwrap_or_default();
    Ok(Some(Value::Object(Some(ctx.create_string(&prefix)))))
}

fn native_has_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let has = with_state(ctx, this, |s| {
        matches!(
            s.current().map(|e| e.kind),
            Some(START_ELEMENT) | Some(END_ELEMENT)
        )
    })
    .unwrap_or(false);
    Ok(Some(Value::Int(if has { 1 } else { 0 })))
}

fn native_has_text(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let has = with_state(ctx, this, |s| {
        matches!(
            s.current().map(|e| e.kind),
            Some(CHARACTERS)
                | Some(CDATA)
                | Some(COMMENT)
                | Some(SPACE)
                | Some(DTD)
                | Some(ENTITY_REFERENCE)
        )
    })
    .unwrap_or(false);
    Ok(Some(Value::Int(if has { 1 } else { 0 })))
}

fn native_require(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let expected_type = args
        .get(1)
        .and_then(|v| match v {
            Value::Int(n) => Some(*n),
            _ => None,
        })
        .unwrap_or(-1);
    let expected_ns = match args.get(2) {
        Some(Value::Object(Some(o))) => Some(ctx.read_string(*o).unwrap_or_default()),
        _ => None,
    };
    let expected_local = match args.get(3) {
        Some(Value::Object(Some(o))) => Some(ctx.read_string(*o).unwrap_or_default()),
        _ => None,
    };
    let (cur_kind, cur_local, cur_ns) = with_state(ctx, this, |s| match s.current() {
        Some(e) => (e.kind, e.local_name.clone(), e.namespace_uri.clone()),
        None => (END_DOCUMENT, String::new(), String::new()),
    })
    .unwrap_or((END_DOCUMENT, String::new(), String::new()));
    let mismatched_type = expected_type >= 0 && cur_kind != expected_type;
    let mismatched_local = matches!(&expected_local, Some(l) if !l.is_empty() && *l != cur_local);
    let mismatched_ns = matches!(&expected_ns, Some(n) if !n.is_empty() && *n != cur_ns);
    if mismatched_type || mismatched_local || mismatched_ns {
        // Build an XMLStreamException whose message mirrors the JDK Xerces
        // formatting so WildFly's ParseUtils logs are readable.
        let msg = format!(
            "Required type={} got type={} localName={} ns={}",
            expected_type, cur_kind, cur_local, cur_ns,
        );
        // We don't have a constructor helper for XMLStreamException at hand;
        // surface as IllegalStateException so callers see a clear failure
        // (XMLStreamException is checked but WildFly's catch blocks wrap it
        // into ParseException anyway).
        return Err(
            cratonvm_types::error::RuntimeError::IllegalStateException { message: msg }.into(),
        );
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;

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
}
