// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The VM-side JFR chunk writer (SBR5 lane `jfr`).
//!
//! Java's `EventWriter` writes finished events (already in the on-disk
//! encoding: compressed integers, tagged strings) into native buffers. This
//! module owns everything after that: the chunk file, its 68-byte header, the
//! checkpoint events that carry the constant pools, the metadata event, and
//! the header protocol `jdk.jfr.internal.consumer.ChunkHeader` follows so that
//! `EventDirectoryStream` can read a chunk that is still being written.
//!
//! Everything here is derived from the JDK 25 sources and checked against
//! HotSpot's own `jfr print`/`jfr summary` (see `tools/probes/Sbr5Jfr*.java`);
//! the rule each piece obeys names the class in `jdk.jfr.internal.consumer`
//! that enforces it.
//!
//! * Header (`ChunkHeader`): `FLR\0`, major 2, minor 1, then big-endian
//!   chunk size @8, constant-pool position @16, metadata position @24, start
//!   nanos @32, duration nanos @40, start ticks @48, ticks/second @56, the file
//!   state byte @64 (`255` while the header is being rewritten, `0` when the
//!   chunk is finished, otherwise a generation counter) and the flag byte @67
//!   (bit 0: compressed integers, bit 1: final chunk). A reader accepts a
//!   header only when the state byte read before and after the other fields is
//!   the same, so a rewrite is `state=255`, fields, `state=generation`.
//! * Positions in the header are relative to the chunk start.
//! * Checkpoint events (`ChunkParser.fillConstantPools`): `size`, type `1`,
//!   start ticks, duration, `delta` (a signed long: the distance back to the
//!   previous checkpoint, `0` for the first, so the reader can walk the chain
//!   from the header's position), a flag byte (bit 0 = flush segment end), the
//!   pool count, then per pool the type id, the entry count (never zero) and
//!   `key` + fields of each entry.
//! * Integers are unsigned LEB128, 9 bytes at most with the ninth carrying 8
//!   raw bits (`RecordingInput.readLong`).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};

use crate::jfr_meta::{MetaField, MetaType, Metadata};

pub const HEADER_SIZE: u64 = 68;
const MAGIC: [u8; 4] = [b'F', b'L', b'R', 0];
const MAJOR: u16 = 2;
const MINOR: u16 = 1;
const STATE_UPDATING: u8 = 255;
const STATE_FINISHED: u8 = 0;
const FLAG_COMPRESSED_INTS: u8 = 1;
const FLAG_FINAL_CHUNK: u8 = 1 << 1;
/// `ChunkParser.CheckpointType.FLUSH`.
const CP_FLUSH: u8 = 1;
const EVENT_TYPE_METADATA: u64 = 0;
const EVENT_TYPE_CHECKPOINT: u64 = 1;

/// Append `value` as the JDK's compressed integer.
pub fn put_varint(buf: &mut Vec<u8>, value: u64) {
    let mut v = value;
    for _ in 0..8 {
        if v < 0x80 {
            buf.push(v as u8);
            return;
        }
        buf.push(((v & 0x7F) as u8) | 0x80);
        v >>= 7;
    }
    buf.push((v & 0xFF) as u8);
}

pub fn varint_len(value: u64) -> usize {
    let mut v = value;
    for n in 1..=8 {
        if v < 0x80 {
            return n;
        }
        v >>= 7;
    }
    9
}

/// Prefix `body` with its self-inclusive size, as every JFR event is framed.
pub fn frame_event(body: &[u8]) -> Vec<u8> {
    let mut total = body.len() + 1;
    loop {
        let need = varint_len(total as u64) + body.len();
        if need == total {
            break;
        }
        total = need;
    }
    let mut out = Vec::with_capacity(total);
    put_varint(&mut out, total as u64);
    out.extend_from_slice(body);
    out
}

/// A value for one field of an event or constant-pool entry written by the VM.
#[derive(Debug, Clone)]
pub enum Val {
    Long(i64),
    Int(i32),
    Float(f32),
    Double(f64),
    Bool(bool),
    Str(Option<String>),
    /// A constant-pool reference (the key of the entry in that field's pool).
    Ref(i64),
    /// An inline (non-pool) struct field, as a field map of its type.
    Struct(Vec<(String, Val)>),
    /// An array of structs, each a field map of the element type.
    Structs(Vec<Vec<(String, Val)>>),
}

/// zs s8/jf2: `CRATONVM_JFR_NATIVE_THREAD_NULL_JAVA_NAME` -- default ON: a
/// thread-pool entry with no Java thread id and no Java name is written with a
/// null `javaName` (HotSpot's non-Java threads); `=0` writes `""`. Read once
/// per process (a writer spelling, not compatibility state).
fn native_thread_null_java_name() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JFR_NATIVE_THREAD_NULL_JAVA_NAME")
    })
}

pub fn s(text: &str) -> Val {
    Val::Str(Some(text.to_owned()))
}

fn put_string(out: &mut Vec<u8>, value: &Option<String>) {
    match value {
        None => out.push(0),
        Some(t) if t.is_empty() => out.push(1),
        Some(t) => {
            out.push(3);
            put_varint(out, t.len() as u64);
            out.extend_from_slice(t.as_bytes());
        }
    }
}

fn lookup<'a>(values: &'a [(String, Val)], name: &str) -> Option<&'a Val> {
    values.iter().find(|(n, _)| n == name).map(|(_, v)| v)
}

/// Serialise the fields of `ty` in metadata order, taking each value from
/// `values` by field name (absent = the type's zero).
pub fn write_fields(meta: &Metadata, ty: &MetaType, values: &[(String, Val)], out: &mut Vec<u8>) {
    for field in &ty.fields {
        write_field(meta, field, lookup(values, &field.name), out);
    }
}

fn write_scalar(type_name: &str, value: Option<&Val>, out: &mut Vec<u8>) -> bool {
    match type_name {
        "long" => {
            let v = match value {
                Some(Val::Long(v)) => *v,
                Some(Val::Int(v)) => i64::from(*v),
                _ => 0,
            };
            put_varint(out, v as u64);
        }
        "int" | "short" | "char" => {
            let v: i64 = match value {
                Some(Val::Int(v)) => i64::from(*v),
                Some(Val::Long(v)) => *v,
                _ => 0,
            };
            // `writeInt`/`putInt` widen through the unsigned 32-bit value.
            let v = match type_name {
                "int" => u64::from(v as i32 as u32),
                "short" => u64::from(v as i16 as u16),
                _ => u64::from(v as u16),
            };
            put_varint(out, v);
        }
        "byte" => {
            let v = match value {
                Some(Val::Int(v)) => *v as u8,
                Some(Val::Long(v)) => *v as u8,
                _ => 0,
            };
            out.push(v);
        }
        "boolean" => {
            let v = matches!(value, Some(Val::Bool(true)));
            out.push(u8::from(v));
        }
        "float" => {
            let v = match value {
                Some(Val::Float(v)) => *v,
                Some(Val::Double(v)) => *v as f32,
                _ => 0.0,
            };
            out.extend_from_slice(&v.to_bits().to_be_bytes());
        }
        "double" => {
            let v = match value {
                Some(Val::Double(v)) => *v,
                Some(Val::Float(v)) => f64::from(*v),
                _ => 0.0,
            };
            out.extend_from_slice(&v.to_bits().to_be_bytes());
        }
        "java.lang.String" => {
            match value {
                Some(Val::Str(t)) => put_string(out, t),
                _ => put_string(out, &None),
            };
        }
        _ => return false,
    }
    true
}

fn write_field(meta: &Metadata, field: &MetaField, value: Option<&Val>, out: &mut Vec<u8>) {
    if field.array {
        match value {
            Some(Val::Structs(items)) => {
                put_varint(out, items.len() as u64);
                if let Some(elem) = meta.by_id(field.type_id) {
                    for item in items {
                        write_fields(meta, elem, item, out);
                    }
                }
            }
            _ => put_varint(out, 0),
        }
        return;
    }
    if field.constant_pool {
        let id = match value {
            Some(Val::Ref(id)) | Some(Val::Long(id)) => *id,
            _ => 0,
        };
        put_varint(out, id as u64);
        return;
    }
    let Some(ty) = meta.by_id(field.type_id) else {
        return;
    };
    if !write_scalar(&ty.name, value, out) {
        // an embedded struct: its own values when given, zeros otherwise
        match value {
            Some(Val::Struct(inner)) => write_fields(meta, ty, inner, out),
            _ => write_fields(meta, ty, &[], out),
        }
    }
}

/// A complete event of type `name`, framed. `None` when the metadata does not
/// declare it.
pub fn encode_event(meta: &Metadata, name: &str, values: &[(String, Val)]) -> Option<Vec<u8>> {
    let ty = meta.by_name(name)?;
    let mut body = Vec::with_capacity(64);
    put_varint(&mut body, ty.id as u64);
    write_fields(meta, ty, values, &mut body);
    Some(frame_event(&body))
}

// ---------------------------------------------------------------------------
// Constant pools
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ThreadRec {
    pub os_name: String,
    pub os_tid: i64,
    pub java_name: String,
    pub java_tid: i64,
    pub group: i64,
    pub is_virtual: bool,
}

#[derive(Debug, Clone, Default)]
pub struct GroupRec {
    pub parent: i64,
    pub name: String,
}

#[derive(Debug, Clone, Default)]
pub struct ClassRec {
    pub loader: i64,
    pub name: String,
    pub package: i64,
    pub modifiers: i32,
    pub hidden: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LoaderRec {
    pub type_class: i64,
    pub name: String,
}

#[derive(Debug, Clone, Default)]
pub struct PackageRec {
    pub name: String,
    pub module: i64,
    pub exported: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ModuleRec {
    pub name: String,
    pub version: String,
    pub location: String,
    pub loader: i64,
}

#[derive(Debug, Clone, Default)]
pub struct MethodRec {
    pub class: i64,
    pub name: String,
    pub descriptor: String,
    pub modifiers: i32,
    pub hidden: bool,
}

#[derive(Debug, Clone, Default)]
pub struct FrameRec {
    pub method: i64,
    pub line: i32,
    pub bci: i32,
    /// 0 interpreted, 1 JIT compiled, 2 inlined, 3 native
    pub frame_type: i64,
}

#[derive(Debug, Clone, Default)]
pub struct StackRec {
    pub truncated: bool,
    pub frames: Vec<FrameRec>,
}

/// Everything the VM has handed out an id for. Ids stay valid for the life of
/// the VM; what a chunk carries is the subset its events referenced.
#[derive(Debug, Default)]
pub struct PoolTables {
    pub threads: BTreeMap<i64, ThreadRec>,
    pub groups: BTreeMap<i64, GroupRec>,
    pub classes: BTreeMap<i64, ClassRec>,
    pub loaders: BTreeMap<i64, LoaderRec>,
    pub packages: BTreeMap<i64, PackageRec>,
    pub modules: BTreeMap<i64, ModuleRec>,
    pub methods: BTreeMap<i64, MethodRec>,
    pub stacks: BTreeMap<i64, StackRec>,
    pub symbols: BTreeMap<i64, String>,
    pub symbol_ids: HashMap<String, i64>,
    /// `java.lang.String` constants added by `JVM.addStringConstant`.
    pub strings: BTreeMap<i64, String>,
    pub next_symbol: i64,
}

impl PoolTables {
    /// The id of `text` in the `jdk.types.Symbol` pool (`0` is "no symbol").
    pub fn symbol(&mut self, text: &str) -> i64 {
        if let Some(&id) = self.symbol_ids.get(text) {
            return id;
        }
        self.next_symbol += 1;
        let id = self.next_symbol;
        self.symbols.insert(id, text.to_owned());
        self.symbol_ids.insert(text.to_owned(), id);
        id
    }
}

/// The ids a chunk's events referenced since its last checkpoint.
#[derive(Debug, Default, Clone)]
pub struct Needs {
    pub threads: BTreeSet<i64>,
    pub classes: BTreeSet<i64>,
    pub methods: BTreeSet<i64>,
    pub stacks: BTreeSet<i64>,
    pub strings: BTreeSet<i64>,
    pub modules: BTreeSet<i64>,
    pub packages: BTreeSet<i64>,
    /// pools of constants some other VM-side producer registered:
    /// (type name, key) -> field values
    pub extra: BTreeMap<String, BTreeMap<i64, Vec<(String, Val)>>>,
}

impl Needs {
    pub fn is_empty(&self) -> bool {
        self.threads.is_empty()
            && self.classes.is_empty()
            && self.methods.is_empty()
            && self.stacks.is_empty()
            && self.strings.is_empty()
            && self.modules.is_empty()
            && self.packages.is_empty()
            && self.extra.is_empty()
    }

    pub fn merge(&mut self, other: Needs) {
        self.threads.extend(other.threads);
        self.classes.extend(other.classes);
        self.methods.extend(other.methods);
        self.stacks.extend(other.stacks);
        self.strings.extend(other.strings);
        self.modules.extend(other.modules);
        self.packages.extend(other.packages);
        for (k, v) in other.extra {
            self.extra.entry(k).or_default().extend(v);
        }
    }
}

/// What a chunk has already written, so a later checkpoint of the same chunk
/// writes only what is new.
#[derive(Debug, Default)]
pub struct Written {
    threads: HashSet<i64>,
    groups: HashSet<i64>,
    classes: HashSet<i64>,
    loaders: HashSet<i64>,
    packages: HashSet<i64>,
    modules: HashSet<i64>,
    methods: HashSet<i64>,
    stacks: HashSet<i64>,
    symbols: HashSet<i64>,
    strings: HashSet<i64>,
    extra: HashSet<(String, i64)>,
    frame_types: bool,
}

fn map(pairs: Vec<(&str, Val)>) -> Vec<(String, Val)> {
    pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
}

struct PoolOut<'a> {
    meta: &'a Metadata,
    /// per pool type id: encoded entries
    pools: BTreeMap<i64, Vec<Vec<u8>>>,
}

impl PoolOut<'_> {
    fn entry(&mut self, type_name: &str, key: i64, values: Vec<(String, Val)>) {
        let Some(ty) = self.meta.by_name(type_name) else {
            return;
        };
        let mut body = Vec::with_capacity(32);
        put_varint(&mut body, key as u64);
        if ty.fields.is_empty() && type_name == "java.lang.String" {
            // The string pool's entry is the string itself: the metadata
            // declares `java.lang.String` with no fields.
            match lookup(&values, "string") {
                Some(Val::Str(text)) => put_string(&mut body, text),
                _ => put_string(&mut body, &None),
            }
        } else {
            write_fields(self.meta, ty, &values, &mut body);
        }
        self.pools.entry(ty.id).or_default().push(body);
    }
}

/// Resolve `needs` into the encoded pool sections of one checkpoint,
/// following references to a fixed point and skipping what `written` has.
/// Returns the sections, `(type id, entries)`, in type-id order.
fn collect_pools(
    meta: &Metadata,
    tables: &PoolTables,
    needs: &Needs,
    written: &mut Written,
) -> Vec<(i64, Vec<Vec<u8>>)> {
    let mut stacks: BTreeSet<i64> = needs.stacks.clone();
    let mut methods: BTreeSet<i64> = needs.methods.clone();
    let mut classes: BTreeSet<i64> = needs.classes.clone();
    let mut threads: BTreeSet<i64> = needs.threads.clone();
    let mut groups: BTreeSet<i64> = BTreeSet::new();
    let mut loaders: BTreeSet<i64> = BTreeSet::new();
    let mut packages: BTreeSet<i64> = needs.packages.clone();
    let mut modules: BTreeSet<i64> = needs.modules.clone();
    let mut symbols: BTreeSet<i64> = BTreeSet::new();

    // Close over the references, in dependency order, until nothing is added.
    loop {
        let before = (
            stacks.len(),
            methods.len(),
            classes.len(),
            threads.len(),
            groups.len(),
            loaders.len(),
            packages.len(),
            modules.len(),
            symbols.len(),
        );
        for id in stacks.clone() {
            if let Some(st) = tables.stacks.get(&id) {
                for f in &st.frames {
                    methods.insert(f.method);
                }
            }
        }
        for id in methods.clone() {
            if let Some(m) = tables.methods.get(&id) {
                if m.class != 0 {
                    classes.insert(m.class);
                }
                for name in [&m.name, &m.descriptor] {
                    if let Some(&sid) = tables.symbol_ids.get(name.as_str()) {
                        symbols.insert(sid);
                    }
                }
            }
        }
        for id in classes.clone() {
            if let Some(c) = tables.classes.get(&id) {
                if c.loader != 0 {
                    loaders.insert(c.loader);
                }
                if c.package != 0 {
                    packages.insert(c.package);
                }
                if let Some(&sid) = tables.symbol_ids.get(c.name.as_str()) {
                    symbols.insert(sid);
                }
            }
        }
        for id in loaders.clone() {
            if let Some(l) = tables.loaders.get(&id) {
                if l.type_class != 0 {
                    classes.insert(l.type_class);
                }
                if let Some(&sid) = tables.symbol_ids.get(l.name.as_str()) {
                    symbols.insert(sid);
                }
            }
        }
        for id in packages.clone() {
            if let Some(p) = tables.packages.get(&id) {
                if p.module != 0 {
                    modules.insert(p.module);
                }
                if let Some(&sid) = tables.symbol_ids.get(p.name.as_str()) {
                    symbols.insert(sid);
                }
            }
        }
        for id in modules.clone() {
            if let Some(m) = tables.modules.get(&id) {
                if m.loader != 0 {
                    loaders.insert(m.loader);
                }
                for t in [&m.name, &m.version, &m.location] {
                    if let Some(&sid) = tables.symbol_ids.get(t.as_str()) {
                        symbols.insert(sid);
                    }
                }
            }
        }
        for id in threads.clone() {
            if let Some(t) = tables.threads.get(&id) {
                if t.group != 0 {
                    groups.insert(t.group);
                }
            }
        }
        for id in groups.clone() {
            if let Some(g) = tables.groups.get(&id) {
                if g.parent != 0 {
                    groups.insert(g.parent);
                }
            }
        }
        let after = (
            stacks.len(),
            methods.len(),
            classes.len(),
            threads.len(),
            groups.len(),
            loaders.len(),
            packages.len(),
            modules.len(),
            symbols.len(),
        );
        if before == after {
            break;
        }
    }

    let mut out = PoolOut {
        meta,
        pools: BTreeMap::new(),
    };
    for id in &threads {
        if !written.threads.insert(*id) {
            continue;
        }
        if let Some(t) = tables.threads.get(id) {
            out.entry(
                "java.lang.Thread",
                *id,
                map(vec![
                    ("osName", s(&t.os_name)),
                    ("osThreadId", Val::Long(t.os_tid)),
                    (
                        "javaName",
                        // zs s8/jf2: a VM-internal thread (`VM Thread`,
                        // `ZDriverMajor`, `ZUncommitter`: no Java thread
                        // id, no Java name) has a null `javaName`, as
                        // HotSpot's (it wrote `""`).
                        // `CRATONVM_JFR_NATIVE_THREAD_NULL_JAVA_NAME=0`: `""`.
                        if t.is_virtual
                            || (t.java_tid == 0
                                && t.java_name.is_empty()
                                && native_thread_null_java_name())
                        {
                            Val::Str(None)
                        } else {
                            s(&t.java_name)
                        },
                    ),
                    ("javaThreadId", Val::Long(t.java_tid)),
                    ("group", Val::Ref(t.group)),
                    ("virtual", Val::Bool(t.is_virtual)),
                ]),
            );
        }
    }
    for id in &groups {
        if !written.groups.insert(*id) {
            continue;
        }
        if let Some(g) = tables.groups.get(id) {
            out.entry(
                "jdk.types.ThreadGroup",
                *id,
                map(vec![("parent", Val::Ref(g.parent)), ("name", s(&g.name))]),
            );
        }
    }
    for id in &classes {
        if !written.classes.insert(*id) {
            continue;
        }
        if let Some(c) = tables.classes.get(id) {
            let name = tables.symbol_ids.get(c.name.as_str()).copied().unwrap_or(0);
            out.entry(
                "java.lang.Class",
                *id,
                map(vec![
                    ("classLoader", Val::Ref(c.loader)),
                    ("name", Val::Ref(name)),
                    ("package", Val::Ref(c.package)),
                    ("modifiers", Val::Int(c.modifiers)),
                    ("hidden", Val::Bool(c.hidden)),
                ]),
            );
        }
    }
    for id in &loaders {
        if !written.loaders.insert(*id) {
            continue;
        }
        if let Some(l) = tables.loaders.get(id) {
            let name = tables.symbol_ids.get(l.name.as_str()).copied().unwrap_or(0);
            out.entry(
                "jdk.types.ClassLoader",
                *id,
                map(vec![
                    ("type", Val::Ref(l.type_class)),
                    ("name", Val::Ref(name)),
                ]),
            );
        }
    }
    for id in &packages {
        if !written.packages.insert(*id) {
            continue;
        }
        if let Some(p) = tables.packages.get(id) {
            let name = tables.symbol_ids.get(p.name.as_str()).copied().unwrap_or(0);
            out.entry(
                "jdk.types.Package",
                *id,
                map(vec![
                    ("name", Val::Ref(name)),
                    ("module", Val::Ref(p.module)),
                    ("exported", Val::Bool(p.exported)),
                ]),
            );
        }
    }
    for id in &modules {
        if !written.modules.insert(*id) {
            continue;
        }
        if let Some(m) = tables.modules.get(id) {
            let sym = |t: &str| tables.symbol_ids.get(t).copied().unwrap_or(0);
            out.entry(
                "jdk.types.Module",
                *id,
                map(vec![
                    ("name", Val::Ref(sym(&m.name))),
                    ("version", Val::Ref(sym(&m.version))),
                    ("location", Val::Ref(sym(&m.location))),
                    ("classLoader", Val::Ref(m.loader)),
                ]),
            );
        }
    }
    for id in &methods {
        if !written.methods.insert(*id) {
            continue;
        }
        if let Some(m) = tables.methods.get(id) {
            let sym = |t: &str| tables.symbol_ids.get(t).copied().unwrap_or(0);
            out.entry(
                "jdk.types.Method",
                *id,
                map(vec![
                    ("type", Val::Ref(m.class)),
                    ("name", Val::Ref(sym(&m.name))),
                    ("descriptor", Val::Ref(sym(&m.descriptor))),
                    ("modifiers", Val::Int(m.modifiers)),
                    ("hidden", Val::Bool(m.hidden)),
                ]),
            );
        }
    }
    for id in &stacks {
        if !written.stacks.insert(*id) {
            continue;
        }
        if let Some(st) = tables.stacks.get(id) {
            let frames: Vec<Vec<(String, Val)>> = st
                .frames
                .iter()
                .map(|f| {
                    map(vec![
                        ("method", Val::Ref(f.method)),
                        ("lineNumber", Val::Int(f.line)),
                        ("bytecodeIndex", Val::Int(f.bci)),
                        ("type", Val::Ref(f.frame_type)),
                    ])
                })
                .collect();
            out.entry(
                "jdk.types.StackTrace",
                *id,
                map(vec![
                    ("truncated", Val::Bool(st.truncated)),
                    ("frames", Val::Structs(frames)),
                ]),
            );
        }
    }
    for id in &symbols {
        if !written.symbols.insert(*id) {
            continue;
        }
        if let Some(text) = tables.symbols.get(id) {
            out.entry("jdk.types.Symbol", *id, map(vec![("string", s(text))]));
        }
    }
    for id in &needs.strings {
        if !written.strings.insert(*id) {
            continue;
        }
        if let Some(text) = tables.strings.get(id) {
            out.entry("java.lang.String", *id, map(vec![("string", s(text))]));
        }
    }
    if !written.frame_types {
        written.frame_types = true;
        for (key, text) in [
            (0i64, "Interpreted"),
            (1, "JIT compiled"),
            (2, "Inlined"),
            (3, "Native"),
            (4, "C++"),
        ] {
            out.entry(
                "jdk.types.FrameType",
                key,
                map(vec![("description", s(text))]),
            );
        }
    }
    for (type_name, entries) in &needs.extra {
        for (key, values) in entries {
            if !written.extra.insert((type_name.clone(), *key)) {
                continue;
            }
            out.entry(type_name, *key, values.clone());
        }
    }
    out.pools.into_iter().collect()
}

// ---------------------------------------------------------------------------
// The chunk
// ---------------------------------------------------------------------------

/// Inputs of one flush segment that do not live in the chunk.
pub struct SegmentInput<'a> {
    pub meta: Option<&'a Metadata>,
    pub metadata_blob: &'a [u8],
    pub metadata_id: i64,
    pub tables: &'a PoolTables,
    pub needs: &'a Needs,
    pub now_nanos: i64,
    pub now_ticks: i64,
    pub ticks_per_second: i64,
}

pub struct ChunkWriter {
    pub path: Option<String>,
    file: Option<File>,
    pub start_nanos: i64,
    pub start_ticks: i64,
    /// Event bytes not yet in the file (all of them for a memory chunk).
    data: Vec<u8>,
    file_len: u64,
    last_cp_pos: u64,
    last_meta_pos: u64,
    generation: u8,
    written_metadata_id: i64,
    written: Written,
    pub final_chunk: bool,
    pub event_bytes_total: u64,
}

impl ChunkWriter {
    pub fn new(start_nanos: i64, start_ticks: i64) -> ChunkWriter {
        ChunkWriter {
            path: None,
            file: None,
            start_nanos,
            start_ticks,
            data: Vec::new(),
            file_len: 0,
            last_cp_pos: 0,
            last_meta_pos: 0,
            generation: 1,
            written_metadata_id: -1,
            written: Written::default(),
            final_chunk: false,
            event_bytes_total: 0,
        }
    }

    pub fn is_file(&self) -> bool {
        self.file.is_some()
    }

    /// Bytes this chunk holds: file plus pending.
    pub fn size_hint(&self) -> u64 {
        if self.file.is_some() {
            self.file_len + self.data.len() as u64
        } else {
            self.data.len() as u64
        }
    }

    pub fn pending_len(&self) -> usize {
        self.data.len()
    }

    /// The event bytes not yet in the file, for the unit tests' mock
    /// recordings (`jfr_backend::test_recording`).
    #[cfg(test)]
    pub(crate) fn pending_events(&self) -> &[u8] {
        &self.data
    }

    pub fn append_events(&mut self, bytes: &[u8]) {
        self.event_bytes_total += bytes.len() as u64;
        self.data.extend_from_slice(bytes);
    }

    /// Drop the oldest pending events down to `cap` bytes (memory chunks), on
    /// an event boundary.
    pub fn trim_pending(&mut self, cap: usize) {
        if self.file.is_some() || self.data.len() <= cap {
            return;
        }
        let mut cut = 0usize;
        let target = self.data.len() - cap;
        while cut < target {
            // read the event size (LEB128) at `cut`
            let mut size: u64 = 0;
            let mut shift = 0u32;
            let mut i = cut;
            let mut ok = false;
            for _ in 0..8 {
                let Some(&b) = self.data.get(i) else { break };
                size |= u64::from(b & 0x7F) << shift;
                i += 1;
                if b & 0x80 == 0 {
                    ok = true;
                    break;
                }
                shift += 7;
            }
            if !ok || size == 0 {
                break;
            }
            cut += size as usize;
        }
        if cut > 0 && cut <= self.data.len() {
            self.data.drain(..cut);
        }
    }

    /// Turn this chunk into a file chunk at `path`: create the file and write
    /// the provisional header. Pending events stay pending until the next
    /// [`Self::flush_segment`].
    pub fn open_file(&mut self, path: &str, ticks_per_second: i64) -> io::Result<()> {
        let mut file = File::create(path)?;
        let header = self.header_bytes(HEADER_SIZE, 0, 0, 0, STATE_UPDATING, ticks_per_second);
        file.write_all(&header)?;
        self.file = Some(file);
        self.path = Some(path.to_owned());
        self.file_len = HEADER_SIZE;
        self.last_cp_pos = 0;
        self.last_meta_pos = 0;
        self.written_metadata_id = -1;
        self.written = Written::default();
        Ok(())
    }

    fn header_bytes(
        &self,
        chunk_size: u64,
        cp_pos: u64,
        meta_pos: u64,
        duration_nanos: i64,
        state: u8,
        ticks_per_second: i64,
    ) -> Vec<u8> {
        let mut h = Vec::with_capacity(HEADER_SIZE as usize);
        h.extend_from_slice(&MAGIC);
        h.extend_from_slice(&MAJOR.to_be_bytes());
        h.extend_from_slice(&MINOR.to_be_bytes());
        h.extend_from_slice(&chunk_size.to_be_bytes());
        h.extend_from_slice(&cp_pos.to_be_bytes());
        h.extend_from_slice(&meta_pos.to_be_bytes());
        h.extend_from_slice(&self.start_nanos.to_be_bytes());
        h.extend_from_slice(&duration_nanos.to_be_bytes());
        h.extend_from_slice(&self.start_ticks.to_be_bytes());
        h.extend_from_slice(&ticks_per_second.to_be_bytes());
        h.push(state);
        h.push(0);
        h.push(0);
        let mut flags = FLAG_COMPRESSED_INTS;
        if self.final_chunk {
            flags |= FLAG_FINAL_CHUNK;
        }
        h.push(flags);
        h
    }

    /// Write the pending events, a checkpoint for what they referenced, the
    /// metadata event when it changed, and rewrite the header; with `finish`
    /// the chunk is closed (file state 0). A memory chunk does nothing.
    pub fn flush_segment(&mut self, input: &SegmentInput<'_>, finish: bool) -> io::Result<()> {
        let Some(mut file) = self.file.take() else {
            return Ok(());
        };
        let result = self.flush_segment_to(&mut file, input, finish);
        if finish {
            drop(file);
        } else {
            self.file = Some(file);
        }
        result
    }

    fn flush_segment_to(
        &mut self,
        file: &mut File,
        input: &SegmentInput<'_>,
        finish: bool,
    ) -> io::Result<()> {
        // Header: updating.
        file.seek(SeekFrom::Start(64))?;
        file.write_all(&[STATE_UPDATING])?;
        file.seek(SeekFrom::Start(self.file_len))?;
        let mut tail: Vec<u8> = std::mem::take(&mut self.data);
        let mut pos = self.file_len + tail.len() as u64;
        // Metadata first when it changed: it must precede the checkpoint a
        // parser reads only if positions say so; order is free, positions rule.
        if let Some(meta) = input.meta {
            if input.metadata_id != self.written_metadata_id {
                let mut body = Vec::with_capacity(input.metadata_blob.len() + 32);
                put_varint(&mut body, EVENT_TYPE_METADATA);
                put_varint(&mut body, input.now_ticks as u64);
                put_varint(&mut body, 0);
                put_varint(&mut body, input.metadata_id as u64);
                body.extend_from_slice(input.metadata_blob);
                let event = frame_event(&body);
                self.last_meta_pos = pos;
                pos += event.len() as u64;
                tail.extend_from_slice(&event);
                self.written_metadata_id = input.metadata_id;
            }
            let pools = collect_pools(meta, input.tables, input.needs, &mut self.written);
            // A checkpoint is always written: it carries the flush marker the
            // streaming parser ends a segment on.
            let delta: i64 = if self.last_cp_pos == 0 {
                0
            } else {
                self.last_cp_pos as i64 - pos as i64
            };
            let mut body = Vec::with_capacity(256);
            put_varint(&mut body, EVENT_TYPE_CHECKPOINT);
            put_varint(&mut body, input.now_ticks as u64);
            put_varint(&mut body, 0);
            put_varint(&mut body, delta as u64);
            body.push(CP_FLUSH);
            put_varint(&mut body, pools.len() as u64);
            for (type_id, entries) in &pools {
                put_varint(&mut body, *type_id as u64);
                put_varint(&mut body, entries.len() as u64);
                for e in entries {
                    body.extend_from_slice(e);
                }
            }
            let event = frame_event(&body);
            self.last_cp_pos = pos;
            pos += event.len() as u64;
            tail.extend_from_slice(&event);
        }
        file.write_all(&tail)?;
        self.file_len = pos;
        let duration = (input.now_nanos - self.start_nanos).max(0);
        let state = if finish {
            STATE_FINISHED
        } else {
            self.generation = if self.generation >= 2 {
                1
            } else {
                self.generation + 1
            };
            self.generation
        };
        // Everything but the state byte first, then the state byte last.
        let header = self.header_bytes(
            self.file_len,
            self.last_cp_pos,
            self.last_meta_pos,
            duration,
            STATE_UPDATING,
            input.ticks_per_second,
        );
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header)?;
        file.seek(SeekFrom::Start(64))?;
        file.write_all(&[state])?;
        file.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_matches_the_readers_cap() {
        let mut b = Vec::new();
        put_varint(&mut b, 0);
        assert_eq!(b, [0]);
        let mut b = Vec::new();
        put_varint(&mut b, 128);
        assert_eq!(b, [0x80, 0x01]);
        let mut b = Vec::new();
        put_varint(&mut b, u64::MAX);
        assert_eq!(b.len(), 9);
        assert_eq!(b[8], 0xFF);
        assert_eq!(varint_len(u64::MAX), 9);
        assert_eq!(varint_len(127), 1);
        assert_eq!(varint_len(128), 2);
    }

    #[test]
    fn framing_is_self_inclusive() {
        let body = vec![7u8; 126];
        let framed = frame_event(&body);
        // 126 + 1 = 127 fits one byte
        assert_eq!(framed.len(), 127);
        assert_eq!(framed[0], 127);
        let body = vec![7u8; 127];
        let framed = frame_event(&body);
        // 127 + 1 = 128 needs two bytes, so the size is 129
        assert_eq!(framed.len(), 129);
        assert_eq!(framed[0], 0x81);
        assert_eq!(framed[1], 0x01);
    }

    #[test]
    fn trim_cuts_on_event_boundaries() {
        let mut c = ChunkWriter::new(0, 0);
        for i in 0..10u8 {
            c.append_events(&frame_event(&[i; 9]));
        }
        assert_eq!(c.pending_len(), 100);
        c.trim_pending(35);
        assert_eq!(c.pending_len() % 10, 0);
        assert!(c.pending_len() <= 40);
    }

    #[test]
    fn symbols_are_interned() {
        let mut t = PoolTables::default();
        let a = t.symbol("java/lang/String");
        let b = t.symbol("x");
        assert_ne!(a, b);
        assert_eq!(a, t.symbol("java/lang/String"));
    }
}
