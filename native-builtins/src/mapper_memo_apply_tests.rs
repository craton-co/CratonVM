// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Field-level witnesses for the `Mapper.map` memo-hit path.
//!
//! The memo-hit path rewrites a `MappingData` and its three `MessageBytes`
//! views through the batched by-name accessors. These tests drive
//! [`mapper_apply_fast_entry`] and [`message_bytes_to_chars_with`] against the
//! mock context and pin the field values a hit must leave behind — the values
//! Tomcat's own `Mapper.internalMapWrapper` produces for the same request.

use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
use cratonvm_native_api::{FieldMetadata, NativeHeapAccess};

/// Declare `name` with the given instance fields (slot order == list order).
fn declare(ctx: &mut MockNativeContext, name: &str, fields: &[&str]) -> ClassId {
    let class_id = ctx.declare_loaded_class(name);
    ctx.set_declared_fields(
        class_id,
        fields
            .iter()
            .enumerate()
            .map(|(slot_index, field)| FieldMetadata {
                name: (*field).to_string(),
                descriptor: "I".to_string(),
                access_flags: 0,
                slot_index,
                declaring_class_id: class_id,
                is_static: false,
            })
            .collect(),
    );
    class_id
}

struct World {
    ctx: MockNativeContext,
    mapping_data: ClassId,
    message_bytes: ClassId,
    char_chunk: ClassId,
    plain: ClassId,
}

const MAPPING_DATA_FIELDS: &[&str] = &[
    "host",
    "context",
    "contextSlashCount",
    "wrapper",
    "jspWildCard",
    "matchType",
    "requestPath",
    "wrapperPath",
    "pathInfo",
    "contexts",
];
const MESSAGE_BYTES_FIELDS: &[&str] = &["type", "charC", "strValue", "hasHashCode", "hasLongValue"];
const CHAR_CHUNK_FIELDS: &[&str] = &["buff", "start", "end", "isSet", "hasHashCode"];

impl World {
    fn new() -> Self {
        let mut ctx = mock_ctx();
        ctx.set_absent_field_answers_null(true);
        let mapping_data = declare(
            &mut ctx,
            "org/apache/catalina/mapper/MappingData",
            MAPPING_DATA_FIELDS,
        );
        let message_bytes = declare(
            &mut ctx,
            "org/apache/tomcat/util/buf/MessageBytes",
            MESSAGE_BYTES_FIELDS,
        );
        let char_chunk = declare(
            &mut ctx,
            "org/apache/tomcat/util/buf/CharChunk",
            CHAR_CHUNK_FIELDS,
        );
        let plain = declare(&mut ctx, "java/lang/Object", &[]);
        World {
            ctx,
            mapping_data,
            message_bytes,
            char_chunk,
            plain,
        }
    }

    fn plain(&mut self) -> ObjectRef {
        self.ctx.alloc_object(self.plain, 0)
    }

    /// A recycled `MessageBytes` (`type == T_NULL`) owning a recycled chunk.
    fn message_bytes(&mut self) -> (ObjectRef, ObjectRef) {
        let chunk = self
            .ctx
            .alloc_object(self.char_chunk, CHAR_CHUNK_FIELDS.len());
        let mb = self
            .ctx
            .alloc_object(self.message_bytes, MESSAGE_BYTES_FIELDS.len());
        self.ctx
            .set_field_by_name(mb, "charC", Value::Object(Some(chunk)));
        self.ctx.set_field_by_name(mb, "type", Value::Int(0));
        self.ctx.set_field_by_name(chunk, "isSet", Value::Int(0));
        (mb, chunk)
    }

    fn int(&self, o: ObjectRef, name: &str) -> i32 {
        self.ctx.get_field_by_name(o, name).as_int().expect(name)
    }

    fn obj(&self, o: ObjectRef, name: &str) -> Option<ObjectRef> {
        match self.ctx.get_field_by_name(o, name) {
            Value::Object(r) => r,
            // The mock reads a resolvable, never-written slot as `Int(0)`.
            Value::Int(0) => None,
            other => panic!("{name}: expected a reference, got {other:?}"),
        }
    }
}

struct Fixture {
    mapping_data: ObjectRef,
    request: (ObjectRef, ObjectRef),
    wrapper_path: (ObjectRef, ObjectRef),
    path_info: (ObjectRef, ObjectRef),
    uri_buff: ObjectRef,
    entry: MapperInternalFastEntry,
}

impl Fixture {
    fn new(w: &mut World) -> Self {
        let mapping_data = w
            .ctx
            .alloc_object(w.mapping_data, MAPPING_DATA_FIELDS.len());
        let request = w.message_bytes();
        let wrapper_path = w.message_bytes();
        let path_info = w.message_bytes();
        w.ctx
            .set_field_by_name(mapping_data, "requestPath", Value::Object(Some(request.0)));
        w.ctx.set_field_by_name(
            mapping_data,
            "wrapperPath",
            Value::Object(Some(wrapper_path.0)),
        );
        w.ctx
            .set_field_by_name(mapping_data, "pathInfo", Value::Object(Some(path_info.0)));
        let uri_buff = w.plain();
        let entry = MapperInternalFastEntry {
            // gc-common w34-b: the row's inputs; the apply path under test
            // reads none of them.
            key: MapperMemoKey {
                epoch: 0,
                mapper: w.plain(),
                hosts: w.plain(),
                default_host: None,
                version: None,
                host_units: Vec::new().into_boxed_slice(),
                uri_units: Vec::new().into_boxed_slice(),
            },
            mapped_host: w.plain(),
            context_list: None,
            selected_context: None,
            versions: None,
            selected_version: None,
            dispatch: [None; 5],
            // "/foo/bar" is the context path of `/foo/bar/blah/bobou/foo`.
            context_path_len: 8,
            // "/blah/bobou" is the wildcard wrapper's matched prefix.
            wrapper_len: 11,
            host: Some(w.plain()),
            context: Some(w.plain()),
            wrapper: Some(w.plain()),
            wrapper_name: Some(w.plain()),
            path_match: Some(w.plain()),
            context_slash_count: 2,
            no_context: false,
            default_mapping: false,
        };
        Fixture {
            mapping_data,
            request,
            wrapper_path,
            path_info,
            uri_buff,
            entry,
        }
    }

    fn apply(&self, w: &mut World) {
        let mut views = [Value::Object(None); 3];
        w.ctx.get_fields_by_name(
            self.mapping_data,
            &["requestPath", "wrapperPath", "pathInfo"],
            &mut views,
        );
        // The URI `/foo/bar/blah/bobou/foo` occupies `[0, 23)` of the buffer.
        mapper_apply_fast_entry(
            &mut w.ctx,
            &self.entry,
            self.mapping_data,
            (self.uri_buff, 0, 23),
            views,
        );
    }
}

#[test]
fn wildcard_hit_rewrites_every_mapping_data_field() {
    let mut w = World::new();
    let f = Fixture::new(&mut w);

    f.apply(&mut w);

    let md = f.mapping_data;
    assert_eq!(w.obj(md, "host"), f.entry.host);
    assert_eq!(w.obj(md, "context"), f.entry.context);
    assert_eq!(w.int(md, "contextSlashCount"), 2);
    assert_eq!(w.obj(md, "wrapper"), f.entry.wrapper);
    assert_eq!(w.int(md, "jspWildCard"), 0);
    assert_eq!(w.obj(md, "matchType"), f.entry.path_match);

    // `wrapperPath` is the wrapper's NAME as a String (T_STR == 1) ...
    assert_eq!(w.obj(f.wrapper_path.0, "strValue"), f.entry.wrapper_name);
    assert_eq!(w.int(f.wrapper_path.0, "type"), 1);
    assert_eq!(w.int(f.wrapper_path.0, "hasHashCode"), 0);
    assert_eq!(w.int(f.wrapper_path.0, "hasLongValue"), 0);

    // ... `requestPath` is the whole remainder after the context path ...
    assert_eq!(w.int(f.request.0, "type"), 3);
    assert_eq!(w.obj(f.request.1, "buff"), Some(f.uri_buff));
    assert_eq!(w.int(f.request.1, "start"), 8);
    assert_eq!(w.int(f.request.1, "end"), 23);
    assert_eq!(w.int(f.request.1, "isSet"), 1);
    assert_eq!(w.int(f.request.1, "hasHashCode"), 0);

    // ... and `pathInfo` the part past the wrapper's matched prefix.
    assert_eq!(w.int(f.path_info.0, "type"), 3);
    assert_eq!(w.obj(f.path_info.1, "buff"), Some(f.uri_buff));
    assert_eq!(w.int(f.path_info.1, "start"), 8 + 11);
    assert_eq!(w.int(f.path_info.1, "end"), 23);
    assert_eq!(w.int(f.path_info.1, "isSet"), 1);
}

#[test]
fn wildcard_hit_leaves_path_info_alone_when_nothing_follows_the_prefix() {
    let mut w = World::new();
    let mut f = Fixture::new(&mut w);
    // The wrapper's prefix swallows the whole remainder: `uri_end - 8 == 15`.
    f.entry.wrapper_len = 15;

    f.apply(&mut w);

    assert_eq!(w.int(f.path_info.0, "type"), 0, "pathInfo stays null");
    assert_eq!(w.int(f.path_info.1, "isSet"), 0);
    assert_eq!(w.int(f.request.0, "type"), 3);
}

#[test]
fn default_wrapper_hit_publishes_the_path_as_request_and_wrapper_path() {
    let mut w = World::new();
    let mut f = Fixture::new(&mut w);
    f.entry.default_mapping = true;
    f.entry.wrapper_len = 0;

    f.apply(&mut w);

    let md = f.mapping_data;
    assert_eq!(w.obj(md, "wrapper"), f.entry.wrapper);
    assert_eq!(w.int(md, "jspWildCard"), 0);
    assert_eq!(w.obj(md, "matchType"), f.entry.path_match);
    for (mb, chunk) in [f.request, f.wrapper_path] {
        assert_eq!(w.int(mb, "type"), 3);
        assert_eq!(w.int(chunk, "start"), 8);
        assert_eq!(w.int(chunk, "end"), 23);
    }
    assert_eq!(w.int(f.path_info.0, "type"), 0, "pathInfo untouched");
}

#[test]
fn no_context_hit_sets_only_the_host() {
    let mut w = World::new();
    let mut f = Fixture::new(&mut w);
    f.entry.no_context = true;

    f.apply(&mut w);

    let md = f.mapping_data;
    assert_eq!(w.obj(md, "host"), f.entry.host);
    assert_eq!(w.obj(md, "context"), None);
    assert_eq!(w.obj(md, "wrapper"), None);
    assert_eq!(w.int(f.request.0, "type"), 0);
}

#[test]
fn a_chars_message_bytes_converts_to_itself_without_allocating() {
    let mut w = World::new();
    let (mb, chunk) = w.message_bytes();
    w.ctx.set_field_by_name(mb, "type", Value::Int(3));

    let (got, moved) = message_bytes_to_chars(&mut w.ctx, mb).expect("toChars");

    assert_eq!(got, Some(chunk), "the receiver's own chunk comes back");
    assert!(
        !moved,
        "T_CHARS needs no conversion, so nothing can have moved"
    );
}

#[test]
fn conversion_fields_are_type_chars_and_string_in_that_order() {
    let mut w = World::new();
    let (mb, chunk) = w.message_bytes();
    let s = w.plain();
    w.ctx.set_field_by_name(mb, "type", Value::Int(1));
    w.ctx
        .set_field_by_name(mb, "strValue", Value::Object(Some(s)));

    let fields = message_bytes_conversion_fields(&mut w.ctx, mb);

    assert_eq!(fields[0], Value::Int(1));
    assert_eq!(fields[1], Value::Object(Some(chunk)));
    assert_eq!(fields[2], Value::Object(Some(s)));
}

#[test]
fn batched_by_name_access_agrees_with_the_single_field_calls() {
    let mut w = World::new();
    let chunk = w.ctx.alloc_object(w.char_chunk, CHAR_CHUNK_FIELDS.len());

    w.ctx.set_fields_by_name(
        chunk,
        &[
            ("start", Value::Int(7)),
            ("end", Value::Int(-1)),
            ("buff", Value::Object(None)),
        ],
    );
    let mut out = [Value::Int(99); 4];
    w.ctx
        .get_fields_by_name(chunk, &["start", "end", "buff", "no-such-field"], &mut out);

    assert_eq!(out[0], w.ctx.get_field_by_name(chunk, "start"));
    assert_eq!(out[1], w.ctx.get_field_by_name(chunk, "end"));
    assert_eq!(out[2], w.ctx.get_field_by_name(chunk, "buff"));
    assert_eq!(out[3], Value::Object(None), "an unresolved name reads null");
    assert_eq!(out[0], Value::Int(7));

    // A short output slice bounds the batch rather than panicking.
    let mut short = [Value::Int(99); 1];
    w.ctx
        .get_fields_by_name(chunk, &["start", "end"], &mut short);
    assert_eq!(short[0], Value::Int(7));
}

// ---- gc-common w34-b: what a memo row is keyed by --------------------------
//
// `common-w33a-tomcat-mapper-memo-ignores-the-uri-text`: the row was matched
// by the chunks' identity hashes and offsets, never by the URI text, so a
// recycled request whose URI had the previous one's LENGTH was served the
// previous one's context. These tests drive `native_mapper_internal_map`
// end to end on a small Mapper model (one host, contexts `/app1` and `/app2`,
// each with only a default servlet), each on its own VM identity so the
// process-wide memo rows are the test's own.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const MAPPER_FIELDS: &[&str] = &["hosts", "defaultHost", "defaultHostName"];
const MAPPED_HOST_FIELDS: &[&str] = &["name", "object", "contextList"];
const CONTEXT_LIST_FIELDS: &[&str] = &["contexts", "nesting"];
const MAPPED_CONTEXT_FIELDS: &[&str] = &["name", "versions"];
const CONTEXT_VERSION_FIELDS: &[&str] = &[
    "name",
    "path",
    "object",
    "slashCount",
    "paused",
    "exactWrappers",
    "wildcardWrappers",
    "extensionWrappers",
    "welcomeResources",
    "resources",
    "defaultWrapper",
    "nesting",
];
const MAPPED_WRAPPER_FIELDS: &[&str] = &["name", "object", "jspWildCard"];

/// What [`java_decides`] writes into `MappingData.jspWildCard`: the witness
/// that the native left the wrapper mapping to Tomcat's `internalMapWrapper`.
const JAVA_DECIDED: i32 = 77;

fn java_decides(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "internalMapWrapper" {
        return None;
    }
    if let Some(Value::Object(Some(mapping_data))) = args.get(2) {
        ctx.set_field_by_name(*mapping_data, "jspWildCard", Value::Int(JAVA_DECIDED));
    }
    Some(Ok(None))
}

fn private_vm() -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(0x0B34_0001);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Drops the memo rows of ONE test's VM on the way out, panicking or not.
struct ForgetMemoRows(usize);

impl Drop for ForgetMemoRows {
    fn drop(&mut self) {
        let vm = self.0;
        mapper_internal_fast_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|&(owner, _), _| owner != vm);
    }
}

struct App {
    version: ObjectRef,
    context_object: ObjectRef,
    default_wrapper_object: ObjectRef,
}

struct Tomcat {
    w: World,
    vm: usize,
    mapper_class: ClassId,
    wrapper_class: ClassId,
    mapper: ObjectRef,
    hosts: ObjectRef,
    host: ObjectRef,
    host_chunk: ObjectRef,
    uri_chunk: ObjectRef,
    uri_buff: ObjectRef,
    apps: Vec<App>,
    _rows: ForgetMemoRows,
}

/// The recycled URI buffer's capacity: larger than any URI below, like the
/// `char[]` a coyote `CharChunk` keeps across requests.
const URI_CAPACITY: usize = 16;

impl Tomcat {
    fn new() -> Self {
        let mut w = World::new();
        let vm = private_vm();
        w.ctx.set_vm_identity(vm);
        w.ctx.set_invoke_virtual_hook(java_decides);
        let rows = ForgetMemoRows(vm);
        let mapper_class = declare(
            &mut w.ctx,
            "org/apache/catalina/mapper/Mapper",
            MAPPER_FIELDS,
        );
        let host_class = declare(
            &mut w.ctx,
            "org/apache/catalina/mapper/Mapper$MappedHost",
            MAPPED_HOST_FIELDS,
        );
        let list_class = declare(
            &mut w.ctx,
            "org/apache/catalina/mapper/Mapper$ContextList",
            CONTEXT_LIST_FIELDS,
        );
        let context_class = declare(
            &mut w.ctx,
            "org/apache/catalina/mapper/Mapper$MappedContext",
            MAPPED_CONTEXT_FIELDS,
        );
        let version_class = declare(
            &mut w.ctx,
            "org/apache/catalina/mapper/Mapper$ContextVersion",
            CONTEXT_VERSION_FIELDS,
        );
        let wrapper_class = declare(
            &mut w.ctx,
            "org/apache/catalina/mapper/Mapper$MappedWrapper",
            MAPPED_WRAPPER_FIELDS,
        );

        let mut apps = Vec::new();
        let mut contexts = Vec::new();
        for path in ["/app1", "/app2"] {
            let default_wrapper_object = w.plain();
            let default_wrapper =
                Self::wrapper_in(&mut w, wrapper_class, "/", default_wrapper_object, false);
            let context_object = w.plain();
            let version_name = w.ctx.create_string("");
            let version_path = w.ctx.create_string(path);
            let version = w
                .ctx
                .alloc_object(version_class, CONTEXT_VERSION_FIELDS.len());
            w.ctx.set_fields_by_name(
                version,
                &[
                    ("name", Value::Object(Some(version_name))),
                    ("path", Value::Object(Some(version_path))),
                    ("object", Value::Object(Some(context_object))),
                    ("slashCount", Value::Int(1)),
                    ("paused", Value::Int(0)),
                    ("defaultWrapper", Value::Object(Some(default_wrapper))),
                    ("nesting", Value::Int(0)),
                ],
            );
            let versions = ref_array(&mut w, &[version]);
            let context_name = w.ctx.create_string(path);
            let context = w
                .ctx
                .alloc_object(context_class, MAPPED_CONTEXT_FIELDS.len());
            w.ctx.set_fields_by_name(
                context,
                &[
                    ("name", Value::Object(Some(context_name))),
                    ("versions", Value::Object(Some(versions))),
                ],
            );
            contexts.push(context);
            apps.push(App {
                version,
                context_object,
                default_wrapper_object,
            });
        }
        let contexts = ref_array(&mut w, &contexts);
        let list = w.ctx.alloc_object(list_class, CONTEXT_LIST_FIELDS.len());
        w.ctx.set_fields_by_name(
            list,
            &[
                ("contexts", Value::Object(Some(contexts))),
                ("nesting", Value::Int(1)),
            ],
        );
        let host_name = w.ctx.create_string("localhost");
        let host_object = w.plain();
        let host = w.ctx.alloc_object(host_class, MAPPED_HOST_FIELDS.len());
        w.ctx.set_fields_by_name(
            host,
            &[
                ("name", Value::Object(Some(host_name))),
                ("object", Value::Object(Some(host_object))),
                ("contextList", Value::Object(Some(list))),
            ],
        );
        let hosts = ref_array(&mut w, &[host]);
        let mapper = Self::mapper_in(&mut w, mapper_class, hosts, host);

        let host_units: Vec<u16> = "localhost".encode_utf16().collect();
        let host_buff = w
            .ctx
            .new_array(cratonvm_types::ArrayElementType::Char, host_units.len());
        for (i, unit) in host_units.iter().enumerate() {
            w.ctx
                .set_array_element(host_buff, i, Value::Int(*unit as i32));
        }
        let host_chunk = w.ctx.alloc_object(w.char_chunk, CHAR_CHUNK_FIELDS.len());
        w.ctx.set_fields_by_name(
            host_chunk,
            &[
                ("buff", Value::Object(Some(host_buff))),
                ("start", Value::Int(0)),
                ("end", Value::Int(host_units.len() as i32)),
                ("isSet", Value::Int(1)),
            ],
        );
        let uri_buff = w
            .ctx
            .new_array(cratonvm_types::ArrayElementType::Char, URI_CAPACITY);
        let uri_chunk = w.ctx.alloc_object(w.char_chunk, CHAR_CHUNK_FIELDS.len());
        w.ctx.set_fields_by_name(
            uri_chunk,
            &[
                ("buff", Value::Object(Some(uri_buff))),
                ("isSet", Value::Int(1)),
            ],
        );
        Tomcat {
            w,
            vm,
            mapper_class,
            wrapper_class,
            mapper,
            hosts,
            host,
            host_chunk,
            uri_chunk,
            uri_buff,
            apps,
            _rows: rows,
        }
    }

    fn wrapper_in(
        w: &mut World,
        wrapper_class: ClassId,
        name: &str,
        object: ObjectRef,
        jsp_wildcard: bool,
    ) -> ObjectRef {
        let name = w.ctx.create_string(name);
        let wrapper = w
            .ctx
            .alloc_object(wrapper_class, MAPPED_WRAPPER_FIELDS.len());
        w.ctx.set_fields_by_name(
            wrapper,
            &[
                ("name", Value::Object(Some(name))),
                ("object", Value::Object(Some(object))),
                ("jspWildCard", Value::Int(jsp_wildcard as i32)),
            ],
        );
        wrapper
    }

    fn mapper_in(
        w: &mut World,
        mapper_class: ClassId,
        hosts: ObjectRef,
        host: ObjectRef,
    ) -> ObjectRef {
        let mapper = w.ctx.alloc_object(mapper_class, MAPPER_FIELDS.len());
        w.ctx.set_fields_by_name(
            mapper,
            &[
                ("hosts", Value::Object(Some(hosts))),
                ("defaultHost", Value::Object(Some(host))),
            ],
        );
        mapper
    }

    /// A second Mapper over the same hosts.
    fn another_mapper(&mut self) -> ObjectRef {
        Self::mapper_in(&mut self.w, self.mapper_class, self.hosts, self.host)
    }

    /// A single-element `MappedWrapper[]` holding a fresh wrapper `name`.
    fn wrappers(&mut self, name: &str, jsp_wildcard: bool) -> ObjectRef {
        let object = self.w.plain();
        let wrapper = Self::wrapper_in(&mut self.w, self.wrapper_class, name, object, jsp_wildcard);
        ref_array(&mut self.w, &[wrapper])
    }

    /// Recycle the request: refill the SAME chunk and `char[]` with `uri`.
    fn set_uri(&mut self, uri: &str) {
        let units: Vec<u16> = uri.encode_utf16().collect();
        assert!(units.len() <= URI_CAPACITY);
        for (i, unit) in units.iter().enumerate() {
            self.w
                .ctx
                .set_array_element(self.uri_buff, i, Value::Int(*unit as i32));
        }
        self.w.ctx.set_fields_by_name(
            self.uri_chunk,
            &[
                ("start", Value::Int(0)),
                ("end", Value::Int(units.len() as i32)),
            ],
        );
    }

    fn map(&mut self) -> ObjectRef {
        let mapper = self.mapper;
        self.map_with(mapper)
    }

    /// One `Mapper.internalMap` call into a fresh `MappingData`; no collection
    /// happens between calls (the mock's collection count is constant).
    fn map_with(&mut self, mapper: ObjectRef) -> ObjectRef {
        let mapping_data = self
            .w
            .ctx
            .alloc_object(self.w.mapping_data, MAPPING_DATA_FIELDS.len());
        let request = self.w.message_bytes();
        let wrapper_path = self.w.message_bytes();
        let path_info = self.w.message_bytes();
        self.w.ctx.set_fields_by_name(
            mapping_data,
            &[
                ("host", Value::Object(None)),
                ("requestPath", Value::Object(Some(request.0))),
                ("wrapperPath", Value::Object(Some(wrapper_path.0))),
                ("pathInfo", Value::Object(Some(path_info.0))),
            ],
        );
        let result = native_mapper_internal_map(
            &mut self.w.ctx,
            &[
                Value::Object(Some(mapper)),
                Value::Object(Some(self.host_chunk)),
                Value::Object(Some(self.uri_chunk)),
                Value::Object(None),
                Value::Object(Some(mapping_data)),
            ],
        );
        assert!(result.is_ok(), "internalMap must not throw here");
        mapping_data
    }

    /// The memo row in `mapper`'s bucket of this test's VM.
    fn row_of(&self, mapper: ObjectRef) -> Option<Arc<MapperInternalFastEntry>> {
        let bucket = self.w.ctx.identity_hash_code(mapper);
        mapper_internal_fast_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(self.vm, bucket))
            .cloned()
    }

    fn row(&self) -> Option<Arc<MapperInternalFastEntry>> {
        self.row_of(self.mapper)
    }

    /// `[start, end)` of the chunk behind `mapping_data.<view>`.
    fn view_range(&self, mapping_data: ObjectRef, view: &str) -> (i32, i32) {
        let mb = self.w.obj(mapping_data, view).expect(view);
        let chunk = self.w.obj(mb, "charC").expect("charC");
        (self.w.int(chunk, "start"), self.w.int(chunk, "end"))
    }

    fn java_decided(&self, mapping_data: ObjectRef) -> bool {
        self.w.int(mapping_data, "jspWildCard") == JAVA_DECIDED
    }
}

fn ref_array(w: &mut World, elems: &[ObjectRef]) -> ObjectRef {
    let arr = w
        .ctx
        .new_array(cratonvm_types::ArrayElementType::Reference, elems.len());
    for (i, e) in elems.iter().enumerate() {
        w.ctx.set_array_element(arr, i, Value::Object(Some(*e)));
    }
    arr
}

#[test]
fn equal_length_uris_on_one_recycled_chunk_map_separately() {
    let mut t = Tomcat::new();

    t.set_uri("/app1/x");
    let first = t.map();
    assert_eq!(t.w.obj(first, "context"), Some(t.apps[0].context_object));
    assert_eq!(
        t.w.obj(first, "wrapper"),
        Some(t.apps[0].default_wrapper_object)
    );
    let first_row = t.row().expect("a default-servlet mapping is memoised");

    // Same chunk, same buffer, same offsets, same length, no collection: only
    // the text differs.
    t.set_uri("/app2/y");
    let second = t.map();
    assert_eq!(
        t.w.obj(second, "context"),
        Some(t.apps[1].context_object),
        "the second URI was served the first one's context"
    );
    assert_eq!(
        t.w.obj(second, "wrapper"),
        Some(t.apps[1].default_wrapper_object)
    );
    let second_row = t.row().expect("the second mapping is memoised too");
    assert!(
        !Arc::ptr_eq(&first_row, &second_row),
        "a different URI must miss and write its own row"
    );
}

#[test]
fn the_same_uri_twice_is_still_a_hit() {
    let mut t = Tomcat::new();
    t.set_uri("/app2/y");
    let first = t.map();
    let row = t.row().expect("memoised");

    t.set_uri("/app2/y");
    let again = t.map();

    assert!(
        Arc::ptr_eq(&row, &t.row().expect("still memoised")),
        "the repeat must be served from the row, not rebuild it"
    );
    assert_eq!(t.w.obj(again, "host"), t.w.obj(first, "host"));
    assert_eq!(t.w.obj(again, "context"), Some(t.apps[1].context_object));
    assert_eq!(
        t.w.obj(again, "wrapper"),
        Some(t.apps[1].default_wrapper_object)
    );
    // The views are cut from THIS request's chunk: past the context path.
    assert_eq!(t.view_range(again, "requestPath"), (5, 7));
    assert_eq!(t.view_range(again, "wrapperPath"), (5, 7));
}

#[test]
fn a_row_is_bound_to_its_mapper_not_to_its_hash_bucket() {
    let mut t = Tomcat::new();
    t.set_uri("/app1/x");
    t.map();
    let row = t.row().expect("memoised");
    // Model two Mappers whose identity hashes collide: file A's row in B's
    // bucket. B has the same hosts and sees the same text.
    let other = t.another_mapper();
    let other_bucket = t.w.ctx.identity_hash_code(other);
    mapper_internal_fast_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((t.vm, other_bucket), Arc::clone(&row));

    let mapped = t.map_with(other);

    assert!(
        !Arc::ptr_eq(&row, &t.row_of(other).expect("B memoises its own row")),
        "B must not be served A's row"
    );
    assert_eq!(t.w.obj(mapped, "context"), Some(t.apps[0].context_object));
}

#[test]
fn an_exact_servlet_mapping_beats_the_default_short_cut() {
    let mut t = Tomcat::new();
    let exact = t.wrappers("/x", false);
    t.w.ctx.set_field_by_name(
        t.apps[0].version,
        "exactWrappers",
        Value::Object(Some(exact)),
    );

    t.set_uri("/app1/x");
    let mapped = t.map();

    assert!(
        t.java_decided(mapped),
        "Rule 1 (exact) runs before Rule 7 (default)"
    );
    assert_eq!(
        t.w.obj(mapped, "wrapper"),
        None,
        "the default servlet was not applied"
    );
    assert_eq!(t.w.obj(mapped, "context"), Some(t.apps[0].context_object));
    assert!(
        t.row().is_none(),
        "nothing is memoised for a Java-decided mapping"
    );
}

#[test]
fn a_servlet_mapping_added_after_the_row_invalidates_it() {
    let mut t = Tomcat::new();
    t.set_uri("/app1/x");
    let before = t.map();
    assert_eq!(
        t.w.obj(before, "wrapper"),
        Some(t.apps[0].default_wrapper_object)
    );
    assert!(t.row().is_some());

    // `Mapper.addWrapper` replaces the array copy-on-write.
    let exact = t.wrappers("/x", false);
    t.w.ctx.set_field_by_name(
        t.apps[0].version,
        "exactWrappers",
        Value::Object(Some(exact)),
    );
    let after = t.map();

    assert!(t.java_decided(after), "the row's dispatch tables are stale");
    assert_eq!(t.w.obj(after, "wrapper"), None);
}

#[test]
fn a_jsp_wildcard_match_is_left_to_java() {
    let mut t = Tomcat::new();
    let wildcard = t.wrappers("/x", true);
    t.w.ctx.set_field_by_name(
        t.apps[0].version,
        "wildcardWrappers",
        Value::Object(Some(wildcard)),
    );

    t.set_uri("/app1/x");
    let mapped = t.map();

    assert!(
        t.java_decided(mapped),
        "Rule 2's JSP case never reaches Rule 7"
    );
    assert_eq!(t.w.obj(mapped, "wrapper"), None);
    assert!(t.row().is_none());
}

#[test]
fn a_plain_wildcard_match_is_mapped_natively_and_memoised() {
    let mut t = Tomcat::new();
    let wildcard = t.wrappers("/x", false);
    t.w.ctx.set_field_by_name(
        t.apps[0].version,
        "wildcardWrappers",
        Value::Object(Some(wildcard)),
    );

    t.set_uri("/app1/x");
    let mapped = t.map();

    assert!(!t.java_decided(mapped));
    assert!(t.w.obj(mapped, "wrapper").is_some());
    assert_ne!(
        t.w.obj(mapped, "wrapper"),
        Some(t.apps[0].default_wrapper_object)
    );
    let row = t.row().expect("memoised");
    assert!(!row.default_mapping && !row.no_context);
}

#[test]
fn the_context_root_itself_is_left_to_java() {
    let mut t = Tomcat::new();
    t.set_uri("/app1");
    let mapped = t.map();

    // Tomcat redirects `/app1` to `/app1/` when the Context asks for it; only
    // Java can ask.
    assert!(t.java_decided(mapped));
    assert_eq!(t.w.obj(mapped, "wrapper"), None);
    assert_eq!(t.w.obj(mapped, "context"), Some(t.apps[0].context_object));
}

// ---- gc-common w36-e: parallel deployment --------------------------------
//
// `common-w34b-tomcat-mapper-native-never-sets-mapping-data-contexts-FIXED-20260926`: for a
// context with more than one version Tomcat's `internalMap` publishes every
// version's `Context` in `MappingData.contexts` (CoyoteAdapter walks it to
// route a session of an older version back to that version). The native
// never wrote it.

#[allow(unused_imports)]
use cratonvm_native_api::NativeClassAccess as _;

impl Tomcat {
    /// Redeploy `/app1` as `/app1##1` + `/app1##2`: the existing version is
    /// renamed `1`, a new version `2` (with its own default servlet) is added
    /// after it, and `MappedContext.versions` is replaced copy-on-write, as
    /// `Mapper.addContextVersion` does. Answers version 2's `Context` object.
    fn deploy_second_version_of_app1(&mut self) -> ObjectRef {
        let list = self.w.obj(self.host, "contextList").expect("contextList");
        let contexts = self.w.obj(list, "contexts").expect("contexts");
        let context = match self.w.ctx.get_array_element(contexts, 0) {
            Value::Object(Some(o)) => o,
            other => panic!("contexts[0]: {other:?}"),
        };
        let first = self.apps[0].version;
        let one = self.w.ctx.create_string("1");
        self.w
            .ctx
            .set_field_by_name(first, "name", Value::Object(Some(one)));

        let version_class = self.w.ctx.class_id_of_object(first);
        let default_wrapper_object = self.w.plain();
        let default_wrapper = Tomcat::wrapper_in(
            &mut self.w,
            self.wrapper_class,
            "/",
            default_wrapper_object,
            false,
        );
        let context_object = self.w.plain();
        let two = self.w.ctx.create_string("2");
        let path = self.w.ctx.create_string("/app1");
        let second = self
            .w
            .ctx
            .alloc_object(version_class, CONTEXT_VERSION_FIELDS.len());
        self.w.ctx.set_fields_by_name(
            second,
            &[
                ("name", Value::Object(Some(two))),
                ("path", Value::Object(Some(path))),
                ("object", Value::Object(Some(context_object))),
                ("slashCount", Value::Int(1)),
                ("paused", Value::Int(0)),
                ("defaultWrapper", Value::Object(Some(default_wrapper))),
                ("nesting", Value::Int(0)),
            ],
        );
        let versions = ref_array(&mut self.w, &[first, second]);
        self.w
            .ctx
            .set_field_by_name(context, "versions", Value::Object(Some(versions)));
        context_object
    }

    /// `internalMap` with an explicit requested version.
    fn map_version(&mut self, version: Option<&str>) -> ObjectRef {
        let mapping_data = self
            .w
            .ctx
            .alloc_object(self.w.mapping_data, MAPPING_DATA_FIELDS.len());
        let request = self.w.message_bytes();
        let wrapper_path = self.w.message_bytes();
        let path_info = self.w.message_bytes();
        self.w.ctx.set_fields_by_name(
            mapping_data,
            &[
                ("host", Value::Object(None)),
                ("requestPath", Value::Object(Some(request.0))),
                ("wrapperPath", Value::Object(Some(wrapper_path.0))),
                ("pathInfo", Value::Object(Some(path_info.0))),
            ],
        );
        let version = version.map(|v| self.w.ctx.create_string(v));
        let result = native_mapper_internal_map(
            &mut self.w.ctx,
            &[
                Value::Object(Some(self.mapper)),
                Value::Object(Some(self.host_chunk)),
                Value::Object(Some(self.uri_chunk)),
                Value::Object(version),
                Value::Object(Some(mapping_data)),
            ],
        );
        assert!(result.is_ok(), "internalMap must not throw here");
        mapping_data
    }

    fn contexts_of(&self, mapping_data: ObjectRef) -> Option<Vec<Option<ObjectRef>>> {
        let arr = self.w.obj(mapping_data, "contexts")?;
        let len = self.w.ctx.array_length(arr);
        Some(
            (0..len)
                .map(|i| match self.w.ctx.get_array_element(arr, i) {
                    Value::Object(o) => o,
                    other => panic!("contexts[{i}]: {other:?}"),
                })
                .collect(),
        )
    }
}

#[test]
fn a_versioned_context_publishes_every_version_and_maps_the_latest() {
    let mut t = Tomcat::new();
    // Loaded under the name the lookup asks for (the mock's `class_id_by_name`).
    let context_class =
        t.w.ctx
            .ensure_class_initialized(MAPPER_CONTEXT_CLASS)
            .expect("the mock registers the class");
    let second = t.deploy_second_version_of_app1();
    let pins_before = t.w.ctx.native_pin_count_for_test();

    t.set_uri("/app1/x");
    let mapped = t.map_version(None);

    assert_eq!(
        t.contexts_of(mapped),
        Some(vec![Some(t.apps[0].context_object), Some(second)]),
        "MappingData.contexts lists every version, oldest first"
    );
    let arr = t.w.obj(mapped, "contexts").expect("contexts");
    assert_eq!(
        t.w.ctx.ref_array_component(arr),
        Some(context_class),
        "a Context[], as `new Context[n]` builds it"
    );
    assert_eq!(
        t.w.obj(mapped, "context"),
        Some(second),
        "no version: the latest"
    );
    assert!(
        !t.java_decided(mapped),
        "the default servlet is still mapped natively"
    );
    assert!(
        t.row().is_none(),
        "a versioned mapping is not memoised (a row would have to carry the array)"
    );
    assert_eq!(
        t.w.ctx.native_pin_count_for_test(),
        pins_before,
        "every pin taken across the allocation is released"
    );
}

#[test]
fn a_requested_older_version_is_selected_and_still_sees_every_version() {
    let mut t = Tomcat::new();
    let _ = t.w.ctx.ensure_class_initialized(MAPPER_CONTEXT_CLASS);
    let second = t.deploy_second_version_of_app1();

    t.set_uri("/app1/x");
    let mapped = t.map_version(Some("1"));

    assert_eq!(t.w.obj(mapped, "context"), Some(t.apps[0].context_object));
    assert_eq!(
        t.contexts_of(mapped),
        Some(vec![Some(t.apps[0].context_object), Some(second)])
    );
    // A version that is not deployed falls back to the latest, as
    // `exactFind` returning null does.
    t.set_uri("/app1/x");
    let unknown = t.map_version(Some("9"));
    assert_eq!(t.w.obj(unknown, "context"), Some(second));
    assert_eq!(t.contexts_of(unknown).map(|c| c.len()), Some(2));
}

#[test]
fn a_single_version_context_leaves_contexts_null_and_is_memoised() {
    let mut t = Tomcat::new();
    t.set_uri("/app2/y");
    let mapped = t.map();
    assert_eq!(
        t.contexts_of(mapped),
        None,
        "Tomcat writes it only for versionCount > 1"
    );
    assert!(t.row().is_some());
    // The memo hit leaves it null too.
    t.set_uri("/app2/y");
    let again = t.map();
    assert_eq!(t.contexts_of(again), None);
}

/// `uri.isNull()` (a recycled chunk: `end == 0`, `isSet == false`, buffer
/// kept): Tomcat maps the host and returns; the native went on as if the URI
/// were empty. Neither is memoised or served from the memo.
#[test]
fn a_null_uri_maps_only_the_host_and_is_not_memoised() {
    let mut t = Tomcat::new();
    t.w.ctx.set_fields_by_name(
        t.uri_chunk,
        &[
            ("start", Value::Int(0)),
            ("end", Value::Int(0)),
            ("isSet", Value::Int(0)),
        ],
    );
    let mapped = t.map();
    assert!(
        t.w.obj(mapped, "host").is_some(),
        "the host is still mapped"
    );
    assert_eq!(t.w.obj(mapped, "context"), None);
    assert!(!t.java_decided(mapped));
    assert!(t.row().is_none(), "a null URI writes no row");

    // The same chunk SET to the empty string is an empty URI, not a null one:
    // it is mapped (to no context here) and memoised ...
    t.w.ctx
        .set_field_by_name(t.uri_chunk, "isSet", Value::Int(1));
    let empty = t.map();
    assert!(t.w.obj(empty, "host").is_some());
    let row = t.row().expect("an empty URI is memoised");
    assert!(row.no_context);
    // ... and a null URI after it maps the host again and leaves the row be.
    t.w.ctx
        .set_field_by_name(t.uri_chunk, "isSet", Value::Int(0));
    let null_again = t.map();
    assert!(t.w.obj(null_again, "host").is_some());
    assert!(Arc::ptr_eq(&row, &t.row().expect("row kept")));
}

#[test]
fn range_equality_compares_every_unit_in_pieces() {
    let mut w = World::new();
    let text: Vec<u16> = (0..150u16).map(|i| b'a' as u16 + (i % 26)).collect();
    let buff = w
        .ctx
        .new_array(cratonvm_types::ArrayElementType::Char, text.len());
    for (i, unit) in text.iter().enumerate() {
        w.ctx.set_array_element(buff, i, Value::Int(*unit as i32));
    }
    let ctx: &dyn NativeContext = &w.ctx;

    assert!(char_array_range_equals_units(ctx, buff, 0, 150, &text));
    assert!(char_array_range_equals_units(
        ctx,
        buff,
        3,
        140,
        &text[3..140]
    ));
    assert!(char_array_range_equals_units(ctx, buff, 7, 7, &[]));
    // The last unit of a range longer than one read piece differs.
    let mut last_differs = text.clone();
    last_differs[149] ^= 1;
    assert!(!char_array_range_equals_units(
        ctx,
        buff,
        0,
        150,
        &last_differs
    ));
    // Lengths differ; the range runs past the array; the range is inverted.
    assert!(!char_array_range_equals_units(ctx, buff, 0, 149, &text));
    assert!(!char_array_range_equals_units(ctx, buff, 100, 250, &text));
    assert!(!char_array_range_equals_units(ctx, buff, 9, 3, &[]));

    // A snapshot that runs past the array's end is shortened, so it can never
    // match the range it was asked for.
    let short = char_array_range_units(ctx, buff, 140, 160);
    assert_eq!(&*short, &text[140..150]);
    assert!(!char_array_range_equals_units(ctx, buff, 140, 160, &short));
}
