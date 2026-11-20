// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use std::collections::HashMap;
use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

/// Inline-storage capacity for `EventInstance.fields`.
///
/// JFR built-in events (see `jfr/src/builtin.rs`) declare between 0 and 8
/// fields each. Round-5 Fix 1 sets the inline capacity to 8 so every emit
/// path avoids a heap allocation for the `fields` vector. Beyond 8 the
/// `SmallVec` spills to the heap with the same semantics as `Vec`.
pub const EVENT_FIELD_INLINE: usize = 8;

/// Alias for the inline-storage vector used by `EventInstance.fields`.
///
/// Constructors should use `smallvec::smallvec![...]` or
/// `EventFields::new()` to build values; consumers read it like any
/// `Vec<EventValue>` slice (auto-deref).
pub type EventFields = SmallVec<[EventValue; EVENT_FIELD_INLINE]>;

/// Unique identifier for an event type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EventTypeId(pub u32);

impl EventTypeId {
    /// Sentinel value used by per-emit-site caches when a built-in event type
    /// is not yet registered (or registration failed). `emit_*` functions
    /// treat this as "skip emission" without re-probing the registry on
    /// subsequent calls.
    pub const INVALID: Self = EventTypeId(u32::MAX);

    /// Returns true if this is the `INVALID` sentinel.
    #[inline]
    pub fn is_invalid(self) -> bool {
        self.0 == u32::MAX
    }
}

/// Describes a field in an event
#[derive(Debug, Clone)]
pub struct EventField {
    pub name: String,
    pub type_name: String, // "long", "string", "boolean", "float", "double", "int"
    pub description: String,
}

impl EventField {
    pub fn new(name: &str, type_name: &str, description: &str) -> Self {
        Self {
            name: name.to_string(),
            type_name: type_name.to_string(),
            description: description.to_string(),
        }
    }
}

/// Describes an event type (metadata)
#[derive(Debug, Clone)]
pub struct EventType {
    pub id: EventTypeId,
    pub name: String,
    pub category: Vec<String>,
    pub description: String,
    pub fields: Vec<EventField>,
    pub has_thread: bool,
    pub has_stacktrace: bool,
    pub period: EventPeriod,
    pub threshold: Option<std::time::Duration>,
}

#[derive(Debug, Clone)]
pub enum EventPeriod {
    /// Instant event
    None,
    /// Duration event
    BeginEnd,
    /// Periodic per chunk
    EveryChunk,
    /// Periodic per second
    EverySecond,
}

/// Validation failure for [`EventType`] metadata supplied to
/// [`EventTypeRegistry::register`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventTypeValidationError {
    EmptyName,
    InvalidName,
    DuplicateName(String),
    InvalidCategory(String),
    InvalidDescription,
    EmptyFieldName {
        field_index: usize,
    },
    InvalidFieldName {
        field_index: usize,
    },
    DuplicateFieldName(String),
    InvalidFieldDescription {
        field_index: usize,
    },
    UnknownFieldType {
        field_index: usize,
        type_name: String,
    },
}

impl std::fmt::Display for EventTypeValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EventTypeValidationError::EmptyName => write!(f, "event type name is empty"),
            EventTypeValidationError::InvalidName => {
                write!(f, "event type name contains control characters")
            }
            EventTypeValidationError::DuplicateName(name) => {
                write!(f, "event type name '{}' is already registered", name)
            }
            EventTypeValidationError::InvalidCategory(category) => {
                write!(f, "category '{}' contains control characters", category)
            }
            EventTypeValidationError::InvalidDescription => {
                write!(f, "description contains control characters")
            }
            EventTypeValidationError::EmptyFieldName { field_index } => {
                write!(f, "field {} name is empty", field_index)
            }
            EventTypeValidationError::InvalidFieldName { field_index } => {
                write!(f, "field {} name contains control characters", field_index)
            }
            EventTypeValidationError::DuplicateFieldName(name) => {
                write!(f, "field name '{}' is duplicated", name)
            }
            EventTypeValidationError::InvalidFieldDescription { field_index } => {
                write!(
                    f,
                    "field {} description contains control characters",
                    field_index
                )
            }
            EventTypeValidationError::UnknownFieldType {
                field_index,
                type_name,
            } => write!(
                f,
                "field {} declares unknown type '{}'",
                field_index, type_name
            ),
        }
    }
}

impl std::error::Error for EventTypeValidationError {}

fn metadata_text_is_clean(s: &str) -> bool {
    !s.chars().any(char::is_control)
}

/// A single recorded event instance.
///
/// `fields` uses `SmallVec<[EventValue; 8]>` (alias [`EventFields`]) so that
/// every emit path avoids a heap allocation when the event carries ≤ 8 fields
/// — which covers every JFR built-in event today. Beyond 8 fields the
/// container spills to the heap with the same semantics as `Vec`. See
/// round-5 JFR Fix 1.
#[derive(Debug, Clone)]
pub struct EventInstance {
    pub type_id: EventTypeId,
    pub start_time: u64, // nanos since epoch
    pub end_time: u64,   // nanos since epoch (== start_time for instant events)
    pub thread_id: u64,
    pub fields: EventFields,
}

/// Event field values.
///
/// `String` variant uses `Arc<str>` to allow cheap cloning and sharing across
/// multiple recordings without per-recording heap allocation.
///
/// J6 (round-2): `Str` holds a `&'static str` directly so emit sites that
/// pass literal cause/name strings (e.g. "G1 Young", "Allocation Failure")
/// avoid the `Arc::from(&str)` heap allocation per event. The dumper treats
/// `Str` and `String` identically — both encode as a UTF-8 byte slice.
#[derive(Debug, Clone)]
pub enum EventValue {
    Long(i64),
    Int(i32),
    Float(f32),
    Double(f64),
    Boolean(bool),
    String(Arc<str>),
    Str(&'static str),
    Null,
}

/// Canonical "shape kind" of a field value, used to compare a runtime
/// [`EventValue`] variant against a registry-declared `type_name`.
///
/// Task #30 (HIGH correctness): the JFR writer dispatches on the Rust
/// [`EventValue`] variant (see `dump::encode_event_value`), but the reader
/// dispatches on the registry-declared `type_name` (see
/// `dump::decode_event_value`). A per-emit mismatch (e.g. caller passes
/// `EventValue::Float(_)` for a field declared `"double"`) silently
/// desynchronises the entire chunk because the writer emits 4 bytes where
/// the reader expects 8. `FieldKind` is the abstraction we compare in both
/// directions to catch the mismatch at emit time.
///
/// Both `String` and `Str` collapse to `FieldKind::String` (they share
/// encoding). `Null` is the wildcard — it is accepted for any field type.
///
/// LOW fix (2026-06-17): a `Null` in a numeric/boolean field is no longer a
/// write/read tag desync. `dump::encode_event_value` now emits the declared
/// kind's fixed-width zero for a numeric/boolean Null (4 bytes for `float`,
/// 8 for `double`, 1 compressed-long `0` for `int`/`long`, 1 byte for
/// `boolean`), so the reader stays byte-aligned and the value round-trips
/// deterministically to a typed zero. A `Null` in a `string` field still
/// encodes as the canonical 1-byte JFR null-string tag (encoding-type 0) and
/// round-trips back to [`EventValue::Null`]. The wildcard acceptance here is
/// therefore now sound for every declared kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Int,
    Long,
    Float,
    Double,
    Boolean,
    String,
    /// Wildcard — `EventValue::Null` matches every declared type.
    Null,
}

impl FieldKind {
    /// Map a registry-declared `type_name` string to a [`FieldKind`].
    ///
    /// Returns `None` for unrecognised type strings; callers should treat
    /// that as a registration-time bug (and the emit-time validator does:
    /// it rejects the emit rather than silently corrupting the chunk).
    pub fn from_declared(type_name: &str) -> Option<Self> {
        match type_name {
            "int" => Some(FieldKind::Int),
            "long" => Some(FieldKind::Long),
            "float" => Some(FieldKind::Float),
            "double" => Some(FieldKind::Double),
            "boolean" => Some(FieldKind::Boolean),
            "string" => Some(FieldKind::String),
            _ => None,
        }
    }

    /// Classify a runtime [`EventValue`] variant.
    pub fn of_value(value: &EventValue) -> Self {
        match value {
            EventValue::Int(_) => FieldKind::Int,
            EventValue::Long(_) => FieldKind::Long,
            EventValue::Float(_) => FieldKind::Float,
            EventValue::Double(_) => FieldKind::Double,
            EventValue::Boolean(_) => FieldKind::Boolean,
            EventValue::String(_) | EventValue::Str(_) => FieldKind::String,
            EventValue::Null => FieldKind::Null,
        }
    }

    /// Returns `true` if a runtime value of kind `self` is compatible with
    /// the declared kind `declared`. `Null` is the wildcard on the runtime
    /// side — it matches every declared type. The writer
    /// (`dump::encode_event_value`) now makes a Null self-describing per
    /// declared kind: a string-field Null is the 1-byte null tag, while a
    /// numeric/boolean Null is the kind's fixed-width zero. Either way the
    /// reader stays byte-aligned, so accepting Null for any declared kind is
    /// safe (no chunk desync). Callers may still prefer explicit typed zeros
    /// for numeric fields for clarity, but Null is no longer a correctness
    /// hazard.
    #[inline]
    pub fn matches_declared(self, declared: FieldKind) -> bool {
        if self == FieldKind::Null {
            return true;
        }
        self == declared
    }
}

impl EventValue {
    /// Convenience constructor for string values from a `&str`.
    pub fn from_str(s: &str) -> Self {
        EventValue::String(Arc::from(s))
    }

    /// Zero-allocation constructor for static string literals.
    ///
    /// Prefer this over `from_str` when the input is a `&'static str` (e.g.
    /// a literal in source code), to avoid the per-event `Arc::from` heap
    /// allocation on emit.
    #[inline]
    pub const fn from_static(s: &'static str) -> Self {
        EventValue::Str(s)
    }

    /// Borrow the underlying bytes of a string-typed value, if any.
    ///
    /// Returns `Some(&[u8])` for `String`, `Str`; `None` otherwise (including
    /// `Null`, which encodes as a null tag, not a byte slice).
    #[inline]
    pub fn as_str_bytes(&self) -> Option<&[u8]> {
        match self {
            EventValue::String(s) => Some(s.as_bytes()),
            EventValue::Str(s) => Some(s.as_bytes()),
            _ => None,
        }
    }
}

/// Registry of all known event types.
/// T10.9.B: FxHashMap — event type IDs and names are internal JFR definitions.
pub struct EventTypeRegistry {
    types: FxHashMap<EventTypeId, EventType>,
    name_to_id: FxHashMap<String, EventTypeId>,
    next_id: u32,
}

impl EventTypeRegistry {
    pub fn new() -> Self {
        Self {
            types: FxHashMap::default(),
            name_to_id: FxHashMap::default(),
            next_id: 1,
        }
    }

    /// Register a new event type. The `id` field of the passed `EventType` is
    /// overwritten with the auto-assigned id.
    ///
    /// `EventTypeId(u32::MAX)` is reserved as the `INVALID` sentinel and will
    /// never be assigned to a real event type.
    pub fn register(&mut self, mut event_type: EventType) -> EventTypeId {
        if let Some(existing) = self.find_by_name(&event_type.name) {
            return existing;
        }
        if let Err(err) = self.validate_new_event_type(&event_type) {
            tracing::debug!(
                event_type = %event_type.name,
                error = %err,
                "rejected invalid JFR event type metadata"
            );
            return EventTypeId::INVALID;
        }
        // Guard against ever assigning the INVALID sentinel value.
        assert!(
            self.next_id < u32::MAX,
            "event type ID overflow (reached INVALID sentinel)"
        );
        let id = EventTypeId(self.next_id);
        self.next_id = self.next_id.checked_add(1).expect("event type ID overflow");
        event_type.id = id;
        self.name_to_id.insert(event_type.name.clone(), id);
        self.types.insert(id, event_type);
        id
    }

    /// Validate metadata for a not-yet-registered event type.
    ///
    /// The dump format relies on registry metadata to decode event payloads.
    /// Rejecting malformed descriptors here prevents bad custom event metadata
    /// from being written into every later recording.
    pub fn validate_new_event_type(
        &self,
        event_type: &EventType,
    ) -> Result<(), EventTypeValidationError> {
        if event_type.name.is_empty() {
            return Err(EventTypeValidationError::EmptyName);
        }
        if !metadata_text_is_clean(&event_type.name) {
            return Err(EventTypeValidationError::InvalidName);
        }
        if self.name_to_id.contains_key(&event_type.name) {
            return Err(EventTypeValidationError::DuplicateName(
                event_type.name.clone(),
            ));
        }
        if !metadata_text_is_clean(&event_type.description) {
            return Err(EventTypeValidationError::InvalidDescription);
        }
        for category in &event_type.category {
            if !metadata_text_is_clean(category) {
                return Err(EventTypeValidationError::InvalidCategory(category.clone()));
            }
        }

        let mut field_names: FxHashSet<&str> = FxHashSet::default();
        for (idx, field) in event_type.fields.iter().enumerate() {
            if field.name.is_empty() {
                return Err(EventTypeValidationError::EmptyFieldName { field_index: idx });
            }
            if !metadata_text_is_clean(&field.name) {
                return Err(EventTypeValidationError::InvalidFieldName { field_index: idx });
            }
            if !field_names.insert(field.name.as_str()) {
                return Err(EventTypeValidationError::DuplicateFieldName(
                    field.name.clone(),
                ));
            }
            if !metadata_text_is_clean(&field.description) {
                return Err(EventTypeValidationError::InvalidFieldDescription { field_index: idx });
            }
            if FieldKind::from_declared(&field.type_name).is_none() {
                return Err(EventTypeValidationError::UnknownFieldType {
                    field_index: idx,
                    type_name: field.type_name.clone(),
                });
            }
        }
        Ok(())
    }

    pub fn get(&self, id: EventTypeId) -> Option<&EventType> {
        self.types.get(&id)
    }

    pub fn find_by_name(&self, name: &str) -> Option<EventTypeId> {
        self.name_to_id.get(name).copied()
    }

    pub fn len(&self) -> usize {
        self.types.len()
    }

    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }

    /// Returns an iterator over all registered event types.
    pub fn iter(&self) -> impl Iterator<Item = (&EventTypeId, &EventType)> {
        self.types.iter()
    }
}

impl Default for EventTypeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
