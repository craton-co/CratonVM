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
            host_chunk_key: 1,
            uri_chunk_key: 2,
            version_key: 0,
            hosts_key: 3,
            host_start: 0,
            host_end: 14,
            uri_start: 0,
            uri_end: 23,
            selected_context: None,
            versions_key: 0,
            selected_version: None,
            exact_wrappers_key: 0,
            wildcard_wrappers_key: 0,
            mapped_host: None,
            contexts_key: 0,
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
        mapper_apply_fast_entry(
            &mut w.ctx,
            &self.entry,
            self.mapping_data,
            self.uri_buff,
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
