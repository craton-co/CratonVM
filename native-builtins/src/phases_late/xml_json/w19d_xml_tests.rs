//! gc-common w19-d. The mock never moves an object at an allocation, so
//! the allocation-only fixes are checked for shape and root balance; the
//! SAX handler, held across the stream's `read()` calls (Java), is moved
//! by the `read` hook and every callback must reach the new address.
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::cell::{Cell, RefCell};

/// `<a>é</a>` in UTF-8: one element holding one non-ASCII character.
const W19D_XML: &[u8] = b"<a>\xc3\xa9</a>";

thread_local! {
    static W19D_CURSOR: Cell<usize> = const { Cell::new(0) };
    /// (old, new): the handler address the first `read` moves.
    static W19D_MOVE: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
    /// (method, receiver address) of every handler callback.
    static W19D_RECEIVERS: RefCell<Vec<(String, usize)>> = const { RefCell::new(Vec::new()) };
    /// (length argument, array length) of `characters`.
    static W19D_CHARS: Cell<(i32, usize)> = const { Cell::new((-1, 0)) };
}

fn w19d_sax_hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    match method_name {
        "read" => {
            let i = W19D_CURSOR.with(|c| {
                let i = c.get();
                c.set(i + 1);
                i
            });
            if i == 0 {
                let (old, new) = W19D_MOVE.with(|m| m.get());
                ctx.remap_native_pin_addr_for_test(old, new);
            }
            let byte = W19D_XML.get(i).map_or(-1, |b| i32::from(*b));
            Some(Ok(Some(Value::Int(byte))))
        }
        "startDocument" | "endDocument" | "startElement" | "endElement" | "characters" => {
            if method_name == "characters" {
                let len = args.get(2).and_then(|v| v.as_int()).unwrap_or(-1);
                let arr_len = match args.first() {
                    Some(Value::Object(Some(a))) => ctx.array_length(*a),
                    _ => 0,
                };
                W19D_CHARS.with(|c| c.set((len, arr_len)));
            }
            W19D_RECEIVERS.with(|r| {
                r.borrow_mut()
                    .push((method_name.to_string(), receiver.as_ptr() as usize))
            });
            Some(Ok(None))
        }
        _ => None,
    }
}

fn w19d_find(class: &str, name: &str, desc: &str) -> cratonvm_native_api::NativeCallback {
    let mut r = NativeMethodRegistry::new();
    register_p68_xml(&mut r);
    r.find(class, name, desc)
        .unwrap_or_else(|| panic!("{class}.{name}{desc} must be registered"))
}

/// `SAXParser.parse(InputStream, DefaultHandler)` read the handler from
/// `args` after draining the stream through `read()`: a collection there
/// left every callback dispatched on the vacated address. Also pins the
/// `characters` fix: one UTF-16 unit for `é`, not its two UTF-8 bytes.
#[test]
fn w19d_sax_parse_calls_the_handler_where_the_stream_read_moved_it() {
    let mut ctx = MockNativeContext::new();
    let parser = ctx.fresh_object_ref();
    let stream = ctx.fresh_object_ref();
    let handler = ctx.fresh_object_ref();
    let moved = ctx.fresh_object_ref();
    W19D_CURSOR.with(|c| c.set(0));
    W19D_MOVE.with(|m| m.set((handler.as_ptr() as usize, moved.as_ptr() as usize)));
    W19D_RECEIVERS.with(|r| r.borrow_mut().clear());
    W19D_CHARS.with(|c| c.set((-1, 0)));
    ctx.set_invoke_virtual_hook(w19d_sax_hook);
    let parse = w19d_find(
        "javax/xml/parsers/SAXParser",
        "parse",
        "(Ljava/io/InputStream;Lorg/xml/sax/helpers/DefaultHandler;)V",
    );

    let out = parse(
        &mut ctx,
        &[
            Value::Object(Some(parser)),
            Value::Object(Some(stream)),
            Value::Object(Some(handler)),
        ],
    )
    .expect("parse must not fail");

    assert_eq!(out, None);
    let receivers = W19D_RECEIVERS.with(|r| r.borrow().clone());
    let methods: Vec<&str> = receivers.iter().map(|(m, _)| m.as_str()).collect();
    assert_eq!(
        methods,
        [
            "startDocument",
            "startElement",
            "characters",
            "endElement",
            "endDocument"
        ]
    );
    for (method, addr) in &receivers {
        assert_eq!(
            *addr,
            moved.as_ptr() as usize,
            "{method} must be dispatched on the handler's current address"
        );
    }
    assert_eq!(
        W19D_CHARS.with(|c| c.get()),
        (1, 1),
        "characters gets one UTF-16 unit for `é`, not its UTF-8 byte count"
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}

/// `Document.createElement`: the tag argument and two distinct empty
/// arrays, stored after the rooted allocations.
#[test]
fn w19d_create_element_holds_its_tag_and_two_empty_arrays() {
    let mut ctx = MockNativeContext::new();
    let doc = ctx.fresh_object_ref();
    let tag = ctx.create_string("item");
    let create = w19d_find(
        "org/w3c/dom/Document",
        "createElement",
        "(Ljava/lang/String;)Lorg/w3c/dom/Element;",
    );

    let elem = match create(
        &mut ctx,
        &[Value::Object(Some(doc)), Value::Object(Some(tag))],
    )
    .expect("createElement must not fail")
    {
        Some(Value::Object(Some(e))) => e,
        other => panic!("expected an Element, got {other:?}"),
    };

    assert_eq!(ctx.get_field(elem, 0), Value::Object(Some(tag)));
    let attrs = ctx.get_field(elem, 1);
    let children = ctx.get_field(elem, 2);
    for (name, v) in [("attributes", attrs), ("children", children)] {
        match v {
            Value::Object(Some(a)) => assert_eq!(ctx.array_length(a), 0, "{name}"),
            other => panic!("{name}: expected an empty array, got {other:?}"),
        }
    }
    assert_ne!(attrs, children, "two arrays, not one stored twice");
    assert_eq!(ctx.get_field(elem, 3), Value::Int(0));
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}

/// `Element.getChildNodes` / `getAttributes` / an empty
/// `getElementsByTagName`: the list/map carries the element's own array.
#[test]
fn w19d_node_lists_carry_the_elements_arrays() {
    let mut ctx = MockNativeContext::new();
    let doc = ctx.fresh_object_ref();
    let tag = ctx.create_string("item");
    let create = w19d_find(
        "org/w3c/dom/Document",
        "createElement",
        "(Ljava/lang/String;)Lorg/w3c/dom/Element;",
    );
    let Some(Value::Object(Some(elem))) = create(
        &mut ctx,
        &[Value::Object(Some(doc)), Value::Object(Some(tag))],
    )
    .unwrap() else {
        panic!("expected an Element");
    };

    let child_nodes = w19d_find(
        "org/w3c/dom/Element",
        "getChildNodes",
        "()Lorg/w3c/dom/NodeList;",
    );
    let Some(Value::Object(Some(nl))) =
        child_nodes(&mut ctx, &[Value::Object(Some(elem))]).unwrap()
    else {
        panic!("expected a NodeList");
    };
    assert_eq!(ctx.get_field(nl, 0), ctx.get_field(elem, 2));
    assert_eq!(ctx.get_field(nl, 1), Value::Int(0));

    let attributes = w19d_find(
        "org/w3c/dom/Element",
        "getAttributes",
        "()Lorg/w3c/dom/NamedNodeMap;",
    );
    let Some(Value::Object(Some(nm))) =
        attributes(&mut ctx, &[Value::Object(Some(elem))]).unwrap()
    else {
        panic!("expected a NamedNodeMap");
    };
    assert_eq!(ctx.get_field(nm, 0), ctx.get_field(elem, 1));
    assert_eq!(ctx.get_field(nm, 1), Value::Int(0));

    let empty = xml_empty_node_list(&mut ctx).unwrap();
    match ctx.get_field(empty, 0) {
        Value::Object(Some(a)) => assert_eq!(ctx.array_length(a), 0),
        other => panic!("expected an empty array, got {other:?}"),
    }
    assert_eq!(ctx.get_field(empty, 1), Value::Int(0));
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}
