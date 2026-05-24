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
//! `ObjectRef` pointer (mirrors `t27_tls::sock_alpn_table`).

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
    /// Character / CDATA / comment / DTD payload.
    text: String,
    /// Attribute table for START_ELEMENT events.
    attributes: Vec<StaxAttr>,
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
// Side-table keyed by reader ObjectRef pointer.
// ---------------------------------------------------------------------------

fn reader_table() -> &'static Mutex<HashMap<usize, ReaderState>> {
    static T: OnceLock<Mutex<HashMap<usize, ReaderState>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn obj_key(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

fn store_state(reader: ObjectRef, state: ReaderState) {
    reader_table().lock().insert(obj_key(reader), state);
}

fn with_state<F, R>(reader: ObjectRef, f: F) -> Option<R>
where
    F: FnOnce(&mut ReaderState) -> R,
{
    let mut tbl = reader_table().lock();
    tbl.get_mut(&obj_key(reader)).map(f)
}

fn drop_state(reader: ObjectRef) {
    reader_table().lock().remove(&obj_key(reader));
}

// ---------------------------------------------------------------------------
// quick-xml driver: drains the entire byte source into a `Vec<StaxEvent>`.
// We pre-materialise to keep the native callbacks free of mutable parser
// state shared across invocations (the lifetime of a quick-xml `Reader` is
// otherwise awkward to thread through a `fn(...)`-typed callback).
// ---------------------------------------------------------------------------

fn parse_to_events(bytes: &[u8]) -> Vec<StaxEvent> {
    let mut events: Vec<StaxEvent> = Vec::new();
    events.push(StaxEvent { kind: START_DOCUMENT, ..Default::default() });

    let mut reader = Reader::from_reader(bytes);
    reader.trim_text(false);
    reader.expand_empty_elements(false);
    reader.check_end_names(false);

    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        match reader.read_event_into(&mut buf) {
            Ok(QXmlEvent::Start(e)) => {
                events.push(make_element_event(START_ELEMENT, e.name().as_ref(), e.attributes()));
            }
            Ok(QXmlEvent::Empty(e)) => {
                let name = e.name().as_ref().to_vec();
                events.push(make_element_event(START_ELEMENT, &name, e.attributes()));
                events.push(StaxEvent {
                    kind: END_ELEMENT,
                    local_name: local_name_of(&name),
                    ..Default::default()
                });
            }
            Ok(QXmlEvent::End(e)) => {
                events.push(StaxEvent {
                    kind: END_ELEMENT,
                    local_name: local_name_of(e.name().as_ref()),
                    ..Default::default()
                });
            }
            Ok(QXmlEvent::Text(e)) => {
                let raw = e.unescape().map(|c| c.into_owned()).unwrap_or_else(|_| {
                    String::from_utf8_lossy(e.as_ref()).into_owned()
                });
                events.push(StaxEvent { kind: CHARACTERS, text: raw, ..Default::default() });
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
    }

    events.push(StaxEvent { kind: END_DOCUMENT, ..Default::default() });
    events
}

fn make_element_event(
    kind: i32,
    qname: &[u8],
    attrs: quick_xml::events::attributes::Attributes,
) -> StaxEvent {
    let local = local_name_of(qname);
    let mut ev = StaxEvent { kind, local_name: local, ..Default::default() };
    for a in attrs.flatten() {
        let key = a.key.as_ref().to_vec();
        let val = a
            .unescape_value()
            .map(|c| c.into_owned())
            .unwrap_or_else(|_| String::from_utf8_lossy(&a.value).into_owned());
        // Skip xmlns / xmlns:* declarations for attribute enumeration; mirror them
        // into namespace_uri when the prefix matches.
        if key == b"xmlns" || key.starts_with(b"xmlns:") {
            if key == b"xmlns" {
                ev.namespace_uri = val;
            }
            continue;
        }
        ev.attributes.push(StaxAttr {
            local_name: local_name_of(&key),
            namespace_uri: String::new(),
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
    store_state(reader, ReaderState { events, cursor: 0 });
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
    store_state(reader, ReaderState { events, cursor: 0 });
    Ok(Some(Value::Object(Some(reader))))
}

fn native_has_next(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    let has = with_state(this, |s| {
        let next = (s.cursor + 1) as usize;
        next < s.events.len()
    })
    .unwrap_or(false);
    Ok(Some(Value::Int(if has { 1 } else { 0 })))
}

fn native_next(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    let kind = with_state(this, |s| {
        s.cursor += 1;
        s.current().map(|e| e.kind).unwrap_or(END_DOCUMENT)
    })
    .unwrap_or(END_DOCUMENT);
    Ok(Some(Value::Int(kind)))
}

fn native_get_event_type(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    let kind = with_state(this, |s| s.current().map(|e| e.kind).unwrap_or(START_DOCUMENT))
        .unwrap_or(START_DOCUMENT);
    Ok(Some(Value::Int(kind)))
}

fn current_string<F: FnOnce(&StaxEvent) -> String>(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    f: F,
) -> MethodCallResult {
    let this = this_obj(args)?;
    let s = with_state(this, |st| st.current().map(f).unwrap_or_default()).unwrap_or_default();
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
    // args: [this, namespaceURI (nullable), localName]
    let ns = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let local = match args.get(2) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    let found = with_state(this, |st| {
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
    let idx = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => return Ok(Some(Value::Object(None))),
    };
    let found = with_state(this, |st| {
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
    let idx = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => return Ok(Some(Value::Object(None))),
    };
    let found = with_state(this, |st| {
        st.current()
            .and_then(|e| e.attributes.get(idx as usize).map(|a| a.local_name.clone()))
    })
    .flatten();
    match found {
        Some(s) => Ok(Some(Value::Object(Some(ctx.create_string(&s))))),
        None => Ok(Some(Value::Object(None))),
    }
}

fn native_get_attribute_count(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    let n = with_state(this, |st| {
        st.current().map(|e| e.attributes.len() as i32).unwrap_or(0)
    })
    .unwrap_or(0);
    Ok(Some(Value::Int(n)))
}

fn native_close(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_obj(args)?;
    drop_state(this);
    Ok(None)
}

fn native_is_kind(args: &[Value], expected: i32) -> MethodCallResult {
    let this = this_obj(args)?;
    let yes = with_state(this, |st| st.current().map(|e| e.kind == expected).unwrap_or(false))
        .unwrap_or(false);
    Ok(Some(Value::Int(if yes { 1 } else { 0 })))
}

fn native_is_start_element(_c: &mut dyn NativeContext, a: &[Value]) -> MethodCallResult {
    native_is_kind(a, START_ELEMENT)
}
fn native_is_end_element(_c: &mut dyn NativeContext, a: &[Value]) -> MethodCallResult {
    native_is_kind(a, END_ELEMENT)
}
fn native_is_characters(_c: &mut dyn NativeContext, a: &[Value]) -> MethodCallResult {
    native_is_kind(a, CHARACTERS)
}
fn native_is_whitespace(_c: &mut dyn NativeContext, a: &[Value]) -> MethodCallResult {
    let this = this_obj(a)?;
    let yes = with_state(this, |st| {
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
}
