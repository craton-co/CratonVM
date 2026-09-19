// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JDWP ID management.
//!
//! Maps between JVM-internal references and the 64-bit wire IDs that JDWP
//! uses on the protocol.  Every ID type is a simple newtype around `u64`;
//! the [`IdManager`] hands out monotonically-increasing values and keeps a
//! bidirectional mapping so we can go from wire ID back to the internal
//! object.

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Wire-level ID newtypes
// ---------------------------------------------------------------------------

/// Opaque object reference sent over JDWP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectId(pub u64);

/// Reference-type (class / interface / array) ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReferenceTypeId(pub u64);

/// Thread ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadId(pub u64);

/// Stack-frame ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameId(pub u64);

/// Method ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MethodId(pub u64);

/// Field ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FieldId(pub u64);

// ---------------------------------------------------------------------------
// IdManager
// ---------------------------------------------------------------------------

/// Manages the mapping between internal JVM handles and JDWP wire IDs.
///
/// All counters start at 1 (0 is the JDWP "null" sentinel).
pub struct IdManager {
    next_object_id: u64,
    next_ref_type_id: u64,
    next_thread_id: u64,
    next_frame_id: u64,
    next_method_id: u64,
    next_field_id: u64,

    // Forward map: wire-id → internal handle (stored as u64).
    objects: HashMap<u64, u64>,
    ref_types: HashMap<u64, u64>,
    threads: HashMap<u64, u64>,
    frames: HashMap<u64, u64>,
    methods: HashMap<u64, u64>,
    fields: HashMap<u64, u64>,

    // Reverse map: internal handle → wire-id.
    objects_rev: HashMap<u64, u64>,
    ref_types_rev: HashMap<u64, u64>,
    threads_rev: HashMap<u64, u64>,
    methods_rev: HashMap<u64, u64>,
    fields_rev: HashMap<u64, u64>,
}

impl IdManager {
    pub fn new() -> Self {
        Self {
            next_object_id: 1,
            next_ref_type_id: 1,
            next_thread_id: 1,
            next_frame_id: 1,
            next_method_id: 1,
            next_field_id: 1,
            objects: HashMap::new(),
            ref_types: HashMap::new(),
            threads: HashMap::new(),
            frames: HashMap::new(),
            methods: HashMap::new(),
            fields: HashMap::new(),
            objects_rev: HashMap::new(),
            ref_types_rev: HashMap::new(),
            threads_rev: HashMap::new(),
            methods_rev: HashMap::new(),
            fields_rev: HashMap::new(),
        }
    }

    // -- objects -----------------------------------------------------------

    /// Register an internal object handle and return a fresh [`ObjectId`].
    /// If the same handle was already registered the existing ID is returned.
    pub fn register_object(&mut self, internal: u64) -> ObjectId {
        if let Some(&id) = self.objects_rev.get(&internal) {
            return ObjectId(id);
        }
        let id = self.next_object_id;
        self.next_object_id += 1;
        self.objects.insert(id, internal);
        self.objects_rev.insert(internal, id);
        ObjectId(id)
    }

    pub fn lookup_object(&self, id: ObjectId) -> Option<u64> {
        self.objects.get(&id.0).copied()
    }

    // -- reference types ---------------------------------------------------

    pub fn register_ref_type(&mut self, internal: u64) -> ReferenceTypeId {
        if let Some(&id) = self.ref_types_rev.get(&internal) {
            return ReferenceTypeId(id);
        }
        let id = self.next_ref_type_id;
        self.next_ref_type_id += 1;
        self.ref_types.insert(id, internal);
        self.ref_types_rev.insert(internal, id);
        ReferenceTypeId(id)
    }

    pub fn lookup_ref_type(&self, id: ReferenceTypeId) -> Option<u64> {
        self.ref_types.get(&id.0).copied()
    }

    // -- threads -----------------------------------------------------------

    pub fn register_thread(&mut self, internal: u64) -> ThreadId {
        if let Some(&id) = self.threads_rev.get(&internal) {
            return ThreadId(id);
        }
        let id = self.next_thread_id;
        self.next_thread_id += 1;
        self.threads.insert(id, internal);
        self.threads_rev.insert(internal, id);
        ThreadId(id)
    }

    pub fn lookup_thread(&self, id: ThreadId) -> Option<u64> {
        self.threads.get(&id.0).copied()
    }

    /// Return all currently-registered thread wire IDs.
    pub fn all_thread_ids(&self) -> Vec<ThreadId> {
        self.threads.keys().copied().map(ThreadId).collect()
    }

    // -- frames ------------------------------------------------------------

    pub fn register_frame(&mut self, internal: u64) -> FrameId {
        let id = self.next_frame_id;
        self.next_frame_id += 1;
        self.frames.insert(id, internal);
        FrameId(id)
    }

    pub fn lookup_frame(&self, id: FrameId) -> Option<u64> {
        self.frames.get(&id.0).copied()
    }

    // -- methods -----------------------------------------------------------

    pub fn register_method(&mut self, internal: u64) -> MethodId {
        if let Some(&id) = self.methods_rev.get(&internal) {
            return MethodId(id);
        }
        let id = self.next_method_id;
        self.next_method_id += 1;
        self.methods.insert(id, internal);
        self.methods_rev.insert(internal, id);
        MethodId(id)
    }

    pub fn lookup_method(&self, id: MethodId) -> Option<u64> {
        self.methods.get(&id.0).copied()
    }

    // -- fields ------------------------------------------------------------

    pub fn register_field(&mut self, internal: u64) -> FieldId {
        if let Some(&id) = self.fields_rev.get(&internal) {
            return FieldId(id);
        }
        let id = self.next_field_id;
        self.next_field_id += 1;
        self.fields.insert(id, internal);
        self.fields_rev.insert(internal, id);
        FieldId(id)
    }

    pub fn lookup_field(&self, id: FieldId) -> Option<u64> {
        self.fields.get(&id.0).copied()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_lookup_object() {
        let mut mgr = IdManager::new();
        let id = mgr.register_object(0xDEAD);
        assert_eq!(id, ObjectId(1));
        assert_eq!(mgr.lookup_object(id), Some(0xDEAD));
    }

    #[test]
    fn duplicate_object_returns_same_id() {
        let mut mgr = IdManager::new();
        let a = mgr.register_object(42);
        let b = mgr.register_object(42);
        assert_eq!(a, b);
    }

    #[test]
    fn lookup_missing_object_returns_none() {
        let mgr = IdManager::new();
        assert_eq!(mgr.lookup_object(ObjectId(999)), None);
    }

    #[test]
    fn register_and_lookup_thread() {
        let mut mgr = IdManager::new();
        let t = mgr.register_thread(7);
        assert_eq!(mgr.lookup_thread(t), Some(7));
    }

    #[test]
    fn all_thread_ids_returns_registered() {
        let mut mgr = IdManager::new();
        mgr.register_thread(1);
        mgr.register_thread(2);
        let ids = mgr.all_thread_ids();
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn register_ref_type_and_method() {
        let mut mgr = IdManager::new();
        let rt = mgr.register_ref_type(100);
        let m = mgr.register_method(200);
        assert_eq!(mgr.lookup_ref_type(rt), Some(100));
        assert_eq!(mgr.lookup_method(m), Some(200));
    }

    #[test]
    fn register_field_and_frame() {
        let mut mgr = IdManager::new();
        let f = mgr.register_field(300);
        let fr = mgr.register_frame(400);
        assert_eq!(mgr.lookup_field(f), Some(300));
        assert_eq!(mgr.lookup_frame(fr), Some(400));
    }

    #[test]
    fn ids_start_at_one() {
        let mut mgr = IdManager::new();
        assert_eq!(mgr.register_object(0), ObjectId(1));
        assert_eq!(mgr.register_thread(0), ThreadId(1));
        assert_eq!(mgr.register_ref_type(0), ReferenceTypeId(1));
        assert_eq!(mgr.register_method(0), MethodId(1));
        assert_eq!(mgr.register_field(0), FieldId(1));
        assert_eq!(mgr.register_frame(0), FrameId(1));
    }
}
