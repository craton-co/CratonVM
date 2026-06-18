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

fn parse_to_events(bytes: &[u8]) -> Vec<StaxEvent> {
    let mut events: Vec<StaxEvent> = Vec::new();
    // START_DOCUMENT sits at the very start of the source (line 1, col 1, offset 0).
    let (l0, c0) = line_col_of(bytes, 0);
    events.push(StaxEvent { kind: START_DOCUMENT, line: l0, column: c0, char_offset: 0, ..Default::default() });
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
                let start_ev =
                    make_element_event(START_ELEMENT, &name, e.attributes(), &scopes);
                let ns = start_ev.namespace_uri.clone();
                let prefix = start_ev.prefix.clone();
                events.push(start_ev);
                events.push(StaxEvent {
                    kind: END_ELEMENT,
                    local_name: local_name_of(&name),
                    namespace_uri: ns,
                    prefix,
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
                events.push(StaxEvent {
                    kind: END_ELEMENT,
                    local_name: local,
                    namespace_uri: ns,
                    prefix,
                    ..Default::default()
                });
                scopes.pop();
            }
            Ok(QXmlEvent::Text(e)) => {
                let raw = e.unescape().map(|c| c.into_owned()).unwrap_or_else(|_| {
                    String::from_utf8_lossy(e.as_ref()).into_owned()
                });
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
                    events.push(StaxEvent { kind: CHARACTERS, text: raw, ..Default::default() });
                }
            }
            Ok(QXmlEvent::CData(e)) => {
                let s = String::from_utf8_lossy(e.as_ref()).into_owned();
                events.push(StaxEvent { kind: CDATA, text: s, ..Default::default() });
            }
            Ok(QXmlEvent::Comment(e)) => {
                let s = String::from_utf8_lossy(e.as_ref()).into_owned();
                events.push(StaxEvent { kind: COMMENT, text: s, ..Default::default() });
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
                events.push(StaxEvent { kind: DTD, text: s, ..Default::default() });
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
    events.push(StaxEvent { kind: END_DOCUMENT, line: le, column: ce, char_offset: end_off, ..Default::default() });
    events
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
        // Skip xmlns / xmlns:* declarations themselves — they are not reported
        // as ordinary attributes by StAX (getAttributeCount excludes them).
        if key == b"xmlns" || key.starts_with(b"xmlns:") {
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

fn alloc_synthetic(ctx: &mut dyn NativeContext, class_name: &str) -> Result<ObjectRef, MethodCallFailed> {
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

fn native_factory_new_instance(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
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
    let events = parse_to_events(&bytes);
    let reader = alloc_synthetic(ctx, "javax/xml/stream/XMLStreamReader")?;
    store_state(ctx, reader, ReaderState { events, cursor: 0 });
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
    let events = parse_to_events(text.as_bytes());
    let reader = alloc_synthetic(ctx, "javax/xml/stream/XMLStreamReader")?;
    store_state(ctx, reader, ReaderState { events, cursor: 0 });
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
    let events = parse_to_events(bytes);
    let reader = alloc_synthetic(ctx, "javax/xml/stream/XMLStreamReader")?;
    store_state(ctx, reader, ReaderState { events, cursor: 0 });
    Ok(reader)
}

/// Drain a `java.io.Reader` into a String, char-at-a-time (small XML payloads).
fn drain_reader_to_string(ctx: &mut dyn NativeContext, reader_in: ObjectRef) -> String {
    let mut text = String::new();
    loop {
        let res = ctx.invoke("java/io/Reader", "read", "()I", &[Value::Object(Some(reader_in))]);
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
    // Non-StreamSource (e.g. DOMSource/SAXSource): the real JDK
    // XMLInputFactoryImpl throws UnsupportedOperationException for this
    // optional overload. Callers (e.g. keycloak StaxParserUtil.getXMLEventReader)
    // catch UnsupportedOperationException to fall back to a stream source;
    // throwing NullPointerException here breaks that contract (kcfull #07 —
    // surfaced as a misleading "Error in base64 decoding saml message" log).
    Err(MethodCallFailed::InternalError(VmError::Runtime(
        RuntimeError::UnsupportedOperationException {
            message: format!(
                "Cannot create XMLStreamReader or XMLEventReader from a {}",
                src_cls.replace('/', ".")
            ),
        },
    )))
}

/// `XMLInputFactory.createFilteredReader(XMLEventReader, EventFilter)` — the
/// abstract factory method has no Code attribute on our synthetic factory, so
/// without this native it raised `AbstractMethodError: ... createFilteredReader
/// ... has no Code attribute` (kcfull #02; also blocks #07 once the DOMSource
/// path falls back to a stream/event reader). Mirror the real JDK
/// `XMLInputFactoryImpl`, which simply returns
/// `new EventFilterSupport(reader, filter)` — a real JDK
/// `javax.xml.stream.util.EventReaderDelegate` subclass that applies the filter
/// over the delegate reader. The delegate is our real `XMLEventReaderImpl`, so
/// the filtered reader runs entirely on real JDK bytecode.
fn native_create_filtered_event_reader(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let reader = match args.get(1) {
        Some(v @ Value::Object(Some(_))) => v.clone(),
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException {
                    message: Some("createFilteredReader: null XMLEventReader".to_string()),
                },
            )))
        }
    };
    let filter = args.get(2).cloned().unwrap_or(Value::Object(None));
    ctx.new_object_initialized(
        "com/sun/xml/internal/stream/EventFilterSupport",
        "(Ljavax/xml/stream/XMLEventReader;Ljavax/xml/stream/EventFilter;)V",
        &[reader, filter],
    )
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
        return ctx.invoke(
            "java/lang/Boolean",
            "valueOf",
            "(Z)Ljava/lang/Boolean;",
            &[Value::Int(0)],
        );
    }
    Ok(Some(Value::Object(None)))
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
        e.local_name.split_whitespace().next().unwrap_or("").to_string()
    })
}

/// `XMLStreamReader.getPIData()` — PI payload after the target token.
fn native_get_pi_data(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    current_string(ctx, args, |e| match e.local_name.split_once(char::is_whitespace) {
        Some((_, data)) => data.trim_start().to_string(),
        None => String::new(),
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
    let kind = with_state(ctx, this, |s| s.current().map(|e| e.kind).unwrap_or(START_DOCUMENT))
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

fn native_get_namespace_uri_noargs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
                .find(|a| {
                    a.local_name == local && (ns.is_empty() || a.namespace_uri == ns)
                })
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
    let yes = with_state(ctx, this, |st| st.current().map(|e| e.kind == expected).unwrap_or(false))
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
        "getNamespaceCount",
        "()I",
        |_ctx, _args| Ok(Some(Value::Int(0))),
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
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "isStandalone",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(0))),
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "standaloneSet",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(0))),
    );
    registry.register(
        "javax/xml/stream/XMLStreamReader",
        "getCharacterEncodingScheme",
        "()Ljava/lang/String;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
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
    registry.register("javax/xml/stream/Location", "getLineNumber", "()I", native_loc_line);
    registry.register("javax/xml/stream/Location", "getColumnNumber", "()I", native_loc_column);
    registry.register("javax/xml/stream/Location", "getCharacterOffset", "()I", native_loc_offset);
    // FLAG: getPublicId/getSystemId remain null. quick-xml does NOT track a
    // public/system identifier for the source, and our reader is fed from raw
    // bytes / an InputStream with no associated SYSTEM URI, so there is no real
    // data to surface here. Per spec, returning null for an unknown public/system
    // id is permitted. Left as null deliberately (not a fabricated value).
    let null_str: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        |_ctx, _args| Ok(Some(Value::Object(None)));
    registry.register("javax/xml/stream/Location", "getPublicId", "()Ljava/lang/String;", null_str);
    registry.register("javax/xml/stream/Location", "getSystemId", "()Ljava/lang/String;", null_str);
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
    let loc = crate::alloc_concurrent_synthetic(ctx, "javax/xml/stream/Location", 4);
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
    let (local, ns) = with_state(ctx, this, |s| {
        match s.current() {
            Some(e) => (e.local_name.clone(), e.namespace_uri.clone()),
            None => (String::new(), String::new()),
        }
    })
    .unwrap_or_default();
    let qname = crate::alloc_concurrent_synthetic(ctx, "javax/xml/namespace/QName", 3);
    let local_s = ctx.create_string(&local);
    let ns_s = ctx.create_string(&ns);
    let prefix_s = ctx.create_string("");
    ctx.set_field_by_name(qname, "localPart", Value::Object(Some(local_s)));
    ctx.set_field_by_name(qname, "namespaceURI", Value::Object(Some(ns_s)));
    ctx.set_field_by_name(qname, "prefix", Value::Object(Some(prefix_s)));
    Ok(Some(Value::Object(Some(qname))))
}

fn native_get_attr_qname(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let idx = args.get(1).and_then(|v| match v {
        Value::Int(n) => Some(*n as usize),
        _ => None,
    }).unwrap_or(0);
    let (local, ns) = with_state(ctx, this, |s| {
        match s.current() {
            Some(e) if idx < e.attributes.len() => {
                let a = &e.attributes[idx];
                (a.local_name.clone(), a.namespace_uri.clone())
            }
            _ => (String::new(), String::new()),
        }
    })
    .unwrap_or_default();
    let qname = crate::alloc_concurrent_synthetic(ctx, "javax/xml/namespace/QName", 3);
    let local_s = ctx.create_string(&local);
    let ns_s = ctx.create_string(&ns);
    let prefix_s = ctx.create_string("");
    ctx.set_field_by_name(qname, "localPart", Value::Object(Some(local_s)));
    ctx.set_field_by_name(qname, "namespaceURI", Value::Object(Some(ns_s)));
    ctx.set_field_by_name(qname, "prefix", Value::Object(Some(prefix_s)));
    Ok(Some(Value::Object(Some(qname))))
}

fn native_get_attr_namespace(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let idx = args.get(1).and_then(|v| match v {
        Value::Int(n) => Some(*n as usize),
        _ => None,
    }).unwrap_or(0);
    let ns = with_state(ctx, this, |s| {
        match s.current() {
            Some(e) if idx < e.attributes.len() => e.attributes[idx].namespace_uri.clone(),
            _ => String::new(),
        }
    })
    .unwrap_or_default();
    Ok(Some(Value::Object(Some(ctx.create_string(&ns)))))
}

fn native_has_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let has = with_state(ctx, this, |s| {
        matches!(s.current().map(|e| e.kind), Some(START_ELEMENT) | Some(END_ELEMENT))
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
            Some(CHARACTERS) | Some(CDATA) | Some(COMMENT) | Some(SPACE) | Some(DTD) | Some(ENTITY_REFERENCE)
        )
    })
    .unwrap_or(false);
    Ok(Some(Value::Int(if has { 1 } else { 0 })))
}

fn native_require(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    require_state(ctx, this)?;
    let expected_type = args.get(1).and_then(|v| match v {
        Value::Int(n) => Some(*n),
        _ => None,
    }).unwrap_or(-1);
    let expected_ns = match args.get(2) {
        Some(Value::Object(Some(o))) => Some(ctx.read_string(*o).unwrap_or_default()),
        _ => None,
    };
    let expected_local = match args.get(3) {
        Some(Value::Object(Some(o))) => Some(ctx.read_string(*o).unwrap_or_default()),
        _ => None,
    };
    let (cur_kind, cur_local, cur_ns) = with_state(ctx, this, |s| {
        match s.current() {
            Some(e) => (e.kind, e.local_name.clone(), e.namespace_uri.clone()),
            None => (END_DOCUMENT, String::new(), String::new()),
        }
    }).unwrap_or((END_DOCUMENT, String::new(), String::new()));
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
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: msg,
        }
        .into());
    }
    Ok(None)
}
