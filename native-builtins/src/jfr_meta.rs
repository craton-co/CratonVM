// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The JFR metadata descriptor, as the Java half stores it with
//! `JVM.storeMetadataDescriptor(byte[])` (SBR5 lane `jfr`).
//!
//! `jdk.jfr.internal.MetadataWriter.writeBinary` serialises every visible
//! `jdk.jfr.internal.Type` into a string table followed by an element tree
//! (`root` > `metadata` > `class` > `field`...). The chunk's metadata event
//! carries that blob verbatim, and the VM-side writer needs the same
//! information for the events and constant pools it writes itself:
//! which type id a name has, and which fields (in which order, with which
//! type, constant-pool or inline, scalar or array) a type declares. Reading
//! it from the blob rather than hard-coding the layouts keeps the VM writer
//! exactly as current as the JDK it runs on.
//!
//! Integers in the blob are the 4-byte-capped LEB128 `MetadataWriter.writeInt`
//! uses; strings are `StringParser.Encoding` tagged (the writer only ever
//! emits `NULL` and `CHAR_ARRAY`, the reader accepts the others too).

use std::collections::HashMap;

/// One declared field of a [`MetaType`].
#[derive(Debug, Clone)]
pub struct MetaField {
    pub name: String,
    pub type_id: i64,
    pub array: bool,
    pub constant_pool: bool,
}

/// One `class` element: an event type, a constant-pool type or a plain value type.
#[derive(Debug, Clone)]
pub struct MetaType {
    pub id: i64,
    pub name: String,
    pub super_type: Option<String>,
    pub simple: bool,
    pub fields: Vec<MetaField>,
}

impl MetaType {
    pub fn is_event(&self) -> bool {
        self.super_type.as_deref() == Some("jdk.jfr.Event")
    }
}

/// A parsed metadata descriptor.
#[derive(Debug, Clone, Default)]
pub struct Metadata {
    pub types: Vec<MetaType>,
    by_id: HashMap<i64, usize>,
    by_name: HashMap<String, usize>,
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn byte(&mut self) -> Option<u8> {
        let b = *self.bytes.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }

    /// `MetadataWriter.writeInt`: LEB128, the fourth byte raw.
    fn int(&mut self) -> Option<u32> {
        let mut value: u32 = 0;
        for shift in [0u32, 7, 14] {
            let b = self.byte()?;
            value |= u32::from(b & 0x7F) << shift;
            if b & 0x80 == 0 {
                return Some(value);
            }
        }
        let b = self.byte()?;
        value |= u32::from(b) << 21;
        Some(value)
    }

    fn string(&mut self) -> Option<Option<String>> {
        match self.byte()? {
            0 => Some(None),
            1 => Some(Some(String::new())),
            3 => {
                let len = self.int()? as usize;
                let end = self.pos.checked_add(len)?;
                let s = std::str::from_utf8(self.bytes.get(self.pos..end)?)
                    .ok()?
                    .to_owned();
                self.pos = end;
                Some(Some(s))
            }
            4 => {
                let len = self.int()? as usize;
                let mut units = Vec::with_capacity(len.min(1 << 16));
                for _ in 0..len {
                    units.push(self.int()? as u16);
                }
                Some(Some(String::from_utf16_lossy(&units)))
            }
            5 => {
                let len = self.int()? as usize;
                let end = self.pos.checked_add(len)?;
                let s: String = self
                    .bytes
                    .get(self.pos..end)?
                    .iter()
                    .map(|&b| b as char)
                    .collect();
                self.pos = end;
                Some(Some(s))
            }
            _ => None,
        }
    }
}

#[derive(Debug)]
struct Element {
    name: u32,
    attributes: Vec<(u32, u32)>,
    children: Vec<Element>,
}

fn read_element(c: &mut Cursor<'_>, depth: u32) -> Option<Element> {
    if depth > 8 {
        return None;
    }
    let name = c.int()?;
    let attr_count = c.int()? as usize;
    let mut attributes = Vec::with_capacity(attr_count.min(64));
    for _ in 0..attr_count {
        let n = c.int()?;
        let v = c.int()?;
        attributes.push((n, v));
    }
    let child_count = c.int()? as usize;
    let mut children = Vec::with_capacity(child_count.min(4096));
    for _ in 0..child_count {
        children.push(read_element(c, depth + 1)?);
    }
    Some(Element {
        name,
        attributes,
        children,
    })
}

impl Metadata {
    /// Parse the blob of `MetadataWriter.writeBinary`. `None` when it is not
    /// that format.
    pub fn parse(bytes: &[u8]) -> Option<Metadata> {
        let mut c = Cursor { bytes, pos: 0 };
        let pool_size = c.int()? as usize;
        let mut pool: Vec<String> = Vec::with_capacity(pool_size.min(1 << 16));
        for _ in 0..pool_size {
            pool.push(c.string()?.unwrap_or_default());
        }
        let root = read_element(&mut c, 0)?;
        let str_of = |i: u32| pool.get(i as usize).map(String::as_str).unwrap_or("");
        let mut meta = Metadata::default();
        for section in &root.children {
            if str_of(section.name) != "metadata" {
                continue;
            }
            for class in &section.children {
                if str_of(class.name) != "class" {
                    continue;
                }
                let mut ty = MetaType {
                    id: -1,
                    name: String::new(),
                    super_type: None,
                    simple: false,
                    fields: Vec::new(),
                };
                for (n, v) in &class.attributes {
                    let value = str_of(*v);
                    match str_of(*n) {
                        "id" => ty.id = value.parse().ok()?,
                        "name" => ty.name = value.to_owned(),
                        "superType" => ty.super_type = Some(value.to_owned()),
                        "simpleType" => ty.simple = value == "true",
                        _ => {}
                    }
                }
                for field in &class.children {
                    if str_of(field.name) != "field" {
                        continue;
                    }
                    let mut f = MetaField {
                        name: String::new(),
                        type_id: -1,
                        array: false,
                        constant_pool: false,
                    };
                    for (n, v) in &field.attributes {
                        let value = str_of(*v);
                        match str_of(*n) {
                            "name" => f.name = value.to_owned(),
                            "class" => f.type_id = value.parse().ok()?,
                            "dimension" => f.array = value != "0",
                            "constantPool" => f.constant_pool = value == "true",
                            _ => {}
                        }
                    }
                    ty.fields.push(f);
                }
                meta.by_id.insert(ty.id, meta.types.len());
                meta.by_name.insert(ty.name.clone(), meta.types.len());
                meta.types.push(ty);
            }
        }
        Some(meta)
    }

    /// A descriptor from already-built types, for the unit tests' mock
    /// recordings (`jfr_backend::test_recording`).
    #[cfg(test)]
    pub(crate) fn from_types(types: Vec<MetaType>) -> Metadata {
        let mut meta = Metadata::default();
        for ty in types {
            meta.by_id.insert(ty.id, meta.types.len());
            meta.by_name.insert(ty.name.clone(), meta.types.len());
            meta.types.push(ty);
        }
        meta
    }

    pub fn by_name(&self, name: &str) -> Option<&MetaType> {
        self.by_name.get(name).map(|&i| &self.types[i])
    }

    pub fn by_id(&self, id: i64) -> Option<&MetaType> {
        self.by_id.get(&id).map(|&i| &self.types[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_int(out: &mut Vec<u8>, v: u32) {
        let mut v = v;
        for _ in 0..3 {
            if v < 0x80 {
                out.push(v as u8);
                return;
            }
            out.push((v as u8 & 0x7F) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    fn put_str(out: &mut Vec<u8>, s: &str) {
        out.push(4);
        put_int(out, s.len() as u32);
        for u in s.encode_utf16() {
            put_int(out, u32::from(u));
        }
    }

    #[test]
    fn parses_a_small_descriptor() {
        // strings: 0 root 1 metadata 2 class 3 name 4 id 5 field 6 class 7 x 8 long 9 231 10 T 11 99 12 superType 13 jdk.jfr.Event
        let strings = [
            "root",
            "metadata",
            "class",
            "name",
            "id",
            "field",
            "x",
            "231",
            "T",
            "99",
            "superType",
            "jdk.jfr.Event",
        ];
        let mut blob = Vec::new();
        put_int(&mut blob, strings.len() as u32);
        for s in strings {
            put_str(&mut blob, s);
        }
        // root(1 child: metadata(1 child: class T id=99 super=Event, 1 field x : 231))
        put_int(&mut blob, 0); // root
        put_int(&mut blob, 0); // attrs
        put_int(&mut blob, 1); // children
        put_int(&mut blob, 1); // metadata
        put_int(&mut blob, 0);
        put_int(&mut blob, 1);
        put_int(&mut blob, 2); // class
        put_int(&mut blob, 3); // attrs: name=T, id=99, superType
        put_int(&mut blob, 3);
        put_int(&mut blob, 8);
        put_int(&mut blob, 4);
        put_int(&mut blob, 9);
        put_int(&mut blob, 10);
        put_int(&mut blob, 11);
        put_int(&mut blob, 1); // one field
        put_int(&mut blob, 5); // field
        put_int(&mut blob, 2); // attrs name=x class=231
        put_int(&mut blob, 3);
        put_int(&mut blob, 6);
        put_int(&mut blob, 2); // "class" attribute name is string 2
        put_int(&mut blob, 7);
        put_int(&mut blob, 0);
        let meta = Metadata::parse(&blob).expect("parses");
        let t = meta.by_name("T").expect("type T");
        assert_eq!(t.id, 99);
        assert!(t.is_event());
        assert_eq!(t.fields.len(), 1);
        assert_eq!(t.fields[0].name, "x");
        assert_eq!(t.fields[0].type_id, 231);
        assert!(meta.by_id(99).is_some());
    }
}
