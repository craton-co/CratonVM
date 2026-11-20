use super::{Backend, BACKEND};
use crate::jfr_vm_events::RECORDING_VMS;
use std::sync::atomic::Ordering;
use crate::jfr_meta::{MetaField, MetaType, Metadata};
use std::collections::HashMap;
use std::sync::Arc;

const SCALARS: [&str; 3] = ["long", "int", "boolean"];

/// One appended event: its type and its declared fields' raw values
/// (scalars as integers, a constant-pool field as its key).
#[derive(Debug, Clone)]
pub(crate) struct Row {
    pub(crate) name: String,
    pub(crate) fields: Vec<(String, i64)>,
}

impl Row {
    pub(crate) fn get(&self, field: &str) -> Option<i64> {
        self.fields.iter().find(|(n, _)| n == field).map(|(_, v)| *v)
    }
}

/// An open mock recording on VM id `vm`; closed on drop.
pub(crate) struct MockRecording {
    pub(crate) vm: usize,
}

/// Open a recording on `vm` (an id no real VM uses: tests pick a
/// distinctive constant) with `events` -- `(event name, [(field name,
/// type name)])` -- declared and enabled, thresholds 0, no stack traces.
pub(crate) fn open(vm: usize, events: &[(&str, &[(&str, &str)])]) -> MockRecording {
    let mut types = Vec::new();
    let mut ids: HashMap<String, i64> = HashMap::new();
    for (i, name) in SCALARS.iter().enumerate() {
        ids.insert((*name).to_owned(), i as i64 + 1);
        types.push(MetaType {
            id: i as i64 + 1,
            name: (*name).to_owned(),
            super_type: None,
            simple: false,
            fields: Vec::new(),
        });
    }
    let mut next = 100i64;
    let mut type_id = |types: &mut Vec<MetaType>, name: &str| -> i64 {
        if let Some(&id) = ids.get(name) {
            return id;
        }
        next += 1;
        ids.insert(name.to_owned(), next);
        types.push(MetaType {
            id: next,
            name: name.to_owned(),
            super_type: None,
            simple: false,
            fields: Vec::new(),
        });
        next
    };
    let mut event_ids = Vec::new();
    for (name, fields) in events {
        let fields = fields
            .iter()
            .map(|(f, ty)| MetaField {
                name: (*f).to_owned(),
                type_id: type_id(&mut types, ty),
                array: false,
                constant_pool: !SCALARS.contains(ty),
            })
            .collect();
        let id = type_id(&mut types, name);
        let ty = types.iter_mut().find(|t| t.id == id).expect("just added");
        ty.super_type = Some("jdk.jfr.Event".to_owned());
        ty.fields = fields;
        event_ids.push(id);
    }
    let meta = Arc::new(Metadata::from_types(types));
    BACKEND.with(vm, |b| {
        *b = Backend::default();
        b.meta = Some(meta);
        for id in event_ids {
            b.enabled.insert(id, true);
        }
        // `set_recording` without its JIT-decision gate sync, a process
        // global another test's VM may own.
        b.recording = true;
        RECORDING_VMS.fetch_add(1, Ordering::Relaxed);
    });
    MockRecording { vm }
}

impl MockRecording {
    /// Every event appended so far, in order.
    pub(crate) fn rows(&self) -> Vec<Row> {
        BACKEND.with(self.vm, |b| {
            let (Some(meta), Some(chunk)) = (b.meta.as_ref(), b.chunk.as_ref()) else {
                return Vec::new();
            };
            decode(meta, chunk.pending_events())
        })
    }

    /// The rows of type `name`.
    pub(crate) fn rows_of(&self, name: &str) -> Vec<Row> {
        self.rows().into_iter().filter(|r| r.name == name).collect()
    }

    /// The text an enum-like pool (`jdk.types.VMOperationType`, ...)
    /// gave `key`.
    pub(crate) fn enum_text(&self, pool: &str, key: i64) -> Option<String> {
        BACKEND.with(self.vm, |b| {
            b.enum_keys
                .get(pool)?
                .iter()
                .find(|(_, k)| **k == key)
                .map(|(t, _)| t.clone())
        })
    }
}

impl Drop for MockRecording {
    fn drop(&mut self) {
        BACKEND.with(self.vm, |b| b.recording = false);
        RECORDING_VMS.fetch_sub(1, Ordering::Relaxed);
        BACKEND.forget(self.vm);
    }
}

fn varint(bytes: &[u8], pos: &mut usize) -> u64 {
    let mut v = 0u64;
    for i in 0..8 {
        let b = bytes[*pos];
        *pos += 1;
        v |= u64::from(b & 0x7F) << (7 * i);
        if b & 0x80 == 0 {
            return v;
        }
    }
    let b = bytes[*pos];
    *pos += 1;
    v | (u64::from(b) << 56)
}

fn decode(meta: &Metadata, bytes: &[u8]) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        let start = pos;
        let size = varint(bytes, &mut pos) as usize;
        let ty = meta
            .by_id(varint(bytes, &mut pos) as i64)
            .expect("an event this descriptor declares");
        let mut fields = Vec::new();
        for f in &ty.fields {
            let type_name = meta.by_id(f.type_id).map(|t| t.name.as_str());
            let v = if !f.constant_pool && type_name == Some("boolean") {
                pos += 1;
                i64::from(bytes[pos - 1])
            } else {
                varint(bytes, &mut pos) as i64
            };
            fields.push((f.name.clone(), v));
        }
        assert_eq!(pos - start, size, "{}: framed size", ty.name);
        rows.push(Row {
            name: ty.name.clone(),
            fields,
        });
    }
    rows
}
