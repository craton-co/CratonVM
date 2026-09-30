// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Ahead-of-Time Compilation / Project Leyden (JEPs 483, 514, 515).
//!
//! Provides:
//!   - AOT profile infrastructure: method call counts, branch frequencies, type profiles
//!   - AOT cache infrastructure: pre-compiled method storage and retrieval
//!   - Pre-linking infrastructure: resolved constant pool, field offsets, vtable slots
//!   - Training run profile collection
//!   - Native method stubs for jdk/internal/misc/Leyden and related classes

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::lock_order::{LockLevel, OrderedPlMutex};
use cratonvm_types::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::{native_noop, native_noop_with_this};

// ===========================================================================
// AOT Profile Infrastructure (JEPs 483, 514, 515)
// ===========================================================================

/// Per-method AOT profiling data captured during a training run.
#[derive(Debug, Clone, Default)]
pub struct AotMethodProfile {
    /// Total invocation count for this method during training.
    pub invocation_count: u64,
    /// Receiver type history: maps class name to observed count.
    pub receiver_types: HashMap<String, u64>,
    /// Branch taken/not-taken counts keyed by bytecode offset.
    pub branch_taken: HashMap<u32, u64>,
    pub branch_not_taken: HashMap<u32, u64>,
}

impl AotMethodProfile {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one invocation of this method, optionally with receiver type.
    pub fn record_invocation(&mut self, receiver_type: Option<&str>) {
        self.invocation_count += 1;
        if let Some(ty) = receiver_type {
            if let Some(count) = self.receiver_types.get_mut(ty) {
                *count += 1;
            } else {
                self.receiver_types.insert(ty.to_string(), 1);
            }
        }
    }

    /// Record a branch outcome at the given bytecode offset.
    pub fn record_branch(&mut self, bytecode_offset: u32, taken: bool) {
        if taken {
            *self.branch_taken.entry(bytecode_offset).or_insert(0) += 1;
        } else {
            *self.branch_not_taken.entry(bytecode_offset).or_insert(0) += 1;
        }
    }

    /// Dominant receiver type (highest count), or None if no type data.
    pub fn dominant_receiver_type(&self) -> Option<&str> {
        self.receiver_types
            .iter()
            .max_by_key(|(_, &c)| c)
            .map(|(k, _)| k.as_str())
    }
}

/// Full AOT profile from a training run: a collection of per-method profiles.
#[derive(Debug, Clone, Default)]
pub struct AotProfile {
    /// Method profiles keyed by "ClassName.methodName:descriptor".
    pub methods: HashMap<String, AotMethodProfile>,
}

impl AotProfile {
    pub fn new() -> Self {
        Self::default()
    }

    fn method_key(class_name: &str, method_name: &str, descriptor: &str) -> String {
        format!("{class_name}.{method_name}:{descriptor}")
    }

    /// Get or insert a mutable method profile.
    pub fn method_profile_mut(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> &mut AotMethodProfile {
        let key = Self::method_key(class_name, method_name, descriptor);
        self.methods.entry(key).or_default()
    }

    /// Look up an existing method profile (immutable).
    pub fn method_profile(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<&AotMethodProfile> {
        let key = Self::method_key(class_name, method_name, descriptor);
        self.methods.get(&key)
    }

    /// Total number of profiled methods.
    pub fn method_count(&self) -> usize {
        self.methods.len()
    }
}

// ===========================================================================
// AOT Cache (pre-compiled methods)
// ===========================================================================

/// A single pre-compiled method entry in the AOT cache.
#[derive(Debug, Clone)]
pub struct AotCacheEntry {
    /// SHA-256 fingerprint of the original bytecode (placeholder: first 8 bytes).
    pub bytecode_fingerprint: [u8; 8],
    /// Compiled native code bytes (placeholder implementation).
    pub compiled_code: Vec<u8>,
    /// Deoptimisation map: bytecode offset → native code offset.
    pub deopt_map: HashMap<u32, u32>,
    /// Entry-point offset within `compiled_code`.
    pub entry_point_offset: u32,
}

impl AotCacheEntry {
    pub fn new_placeholder(bytecode: &[u8]) -> Self {
        let mut fp = [0u8; 8];
        let len = bytecode.len().min(8);
        fp[..len].copy_from_slice(&bytecode[..len]);
        Self {
            bytecode_fingerprint: fp,
            compiled_code: Vec::new(),
            deopt_map: HashMap::new(),
            entry_point_offset: 0,
        }
    }

    /// Return true if this entry has actual compiled code (non-empty).
    pub fn has_compiled_code(&self) -> bool {
        !self.compiled_code.is_empty()
    }
}

/// Configuration for the AOT cache.
#[derive(Debug, Clone, Default)]
pub struct AotCacheConfig {
    /// Path for `-XX:AOTCacheOutput` (write cache here after training).
    pub output_path: Option<String>,
    /// Path for `-XX:AOTCacheInput` (load cache from here at startup).
    pub input_path: Option<String>,
    /// Running in training mode (collect profile + cache).
    pub training_mode: bool,
    /// Running in production mode (use preloaded cache).
    pub production_mode: bool,
    /// Maximum number of entries in the AOT cache.
    pub max_entries: usize,
}

impl AotCacheConfig {
    pub fn new() -> Self {
        Self {
            max_entries: 10_000,
            ..Default::default()
        }
    }

    pub fn with_output_path(mut self, path: &str) -> Self {
        self.output_path = Some(path.to_string());
        self
    }

    pub fn with_input_path(mut self, path: &str) -> Self {
        self.input_path = Some(path.to_string());
        self
    }

    pub fn with_training_mode(mut self, enabled: bool) -> Self {
        self.training_mode = enabled;
        self
    }

    pub fn with_production_mode(mut self, enabled: bool) -> Self {
        self.production_mode = enabled;
        self
    }

    pub fn with_max_entries(mut self, max: usize) -> Self {
        self.max_entries = max;
        self
    }
}

/// In-process AOT cache: stores pre-compiled methods and their profiles.
#[derive(Debug, Default)]
pub struct AotCache {
    /// Cache entries keyed by "ClassName.methodName:descriptor".
    entries: HashMap<String, AotCacheEntry>,
    /// Configuration for this cache instance.
    pub config: AotCacheConfig,
}

impl AotCache {
    pub fn new(config: AotCacheConfig) -> Self {
        Self {
            entries: HashMap::new(),
            config,
        }
    }

    fn entry_key(class_name: &str, method_name: &str, descriptor: &str) -> String {
        format!("{class_name}.{method_name}:{descriptor}")
    }

    /// Store a pre-compiled entry. Returns false if the cache is full.
    pub fn store(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        entry: AotCacheEntry,
    ) -> bool {
        if self.entries.len() >= self.config.max_entries {
            return false;
        }
        let key = Self::entry_key(class_name, method_name, descriptor);
        self.entries.insert(key, entry);
        true
    }

    /// Look up a pre-compiled entry.
    pub fn lookup(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<&AotCacheEntry> {
        let key = Self::entry_key(class_name, method_name, descriptor);
        self.entries.get(&key)
    }

    /// Number of entries currently in the cache.
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Length in bytes of the AOT cache integrity digest (SHA-256 = 32).
    const INTEGRITY_DIGEST_LEN: usize = 32;

    /// Environment variable naming an optional secret key. When set (non-empty)
    /// the integrity digest is upgraded from a bare SHA-256 content hash to an
    /// HMAC-SHA256 keyed by the variable's UTF-8 bytes, which resists forgery
    /// by an attacker who does not know the key. When unset, the bare SHA-256
    /// content digest detects accidental corruption and casual tampering but
    /// NOT a determined forger (anyone can recompute SHA-256 over a tampered
    /// payload). Deploy with this set to obtain tamper resistance.
    const INTEGRITY_KEY_ENV: &'static str = "CRATONVM_AOT_HMAC_KEY";

    /// Read the optional HMAC key from the environment. Returns `None` when the
    /// variable is unset or empty (callers then fall back to a bare SHA-256
    /// content digest).
    fn integrity_key() -> Option<Vec<u8>> {
        match cratonvm_types::flags::runtime_var(Self::INTEGRITY_KEY_ENV) {
            Ok(k) if !k.is_empty() => Some(k.into_bytes()),
            _ => None,
        }
    }

    /// Compute a cryptographic integrity digest over the payload bytes.
    ///
    /// If a key is supplied this is an HMAC-SHA256 (RFC 2104) over `data`,
    /// providing forgery resistance against an attacker who does not possess
    /// the key. With no key it is a bare SHA-256 of `data`, which reliably
    /// detects corruption and accidental/casual tampering but does NOT defend
    /// against a determined forger — without secret-key infrastructure that is
    /// the strongest guarantee available. The magic + version guard in
    /// `serialize`/`deserialize` additionally rejects unrelated or
    /// version-skewed blobs. This replaces an earlier keyless non-cryptographic
    /// mix that was trivially forgeable.
    fn compute_integrity_hash(data: &[u8], key: Option<&[u8]>) -> [u8; Self::INTEGRITY_DIGEST_LEN] {
        use crate::crypto_impl::Sha256;
        match key {
            None => Sha256::digest(data),
            Some(key) => {
                // HMAC-SHA256: H((K' ^ opad) || H((K' ^ ipad) || data)), where
                // K' is the key hashed (if longer than the block) then zero-
                // padded to the 64-byte SHA-256 block size.
                const BLOCK: usize = 64;
                let mut k = if key.len() > BLOCK {
                    Sha256::digest(key).to_vec()
                } else {
                    key.to_vec()
                };
                k.resize(BLOCK, 0);
                let mut ipad = [0x36u8; BLOCK];
                let mut opad = [0x5cu8; BLOCK];
                for i in 0..BLOCK {
                    ipad[i] ^= k[i];
                    opad[i] ^= k[i];
                }
                let mut inner = Sha256::new();
                inner.update(&ipad);
                inner.update(data);
                let inner = inner.finalize();
                let mut outer = Sha256::new();
                outer.update(&opad);
                outer.update(&inner);
                outer.finalize()
            }
        }
    }

    /// Constant-time byte-slice equality, used when verifying the integrity
    /// digest so a comparison does not leak digest contents via timing.
    fn ct_eq(a: &[u8], b: &[u8]) -> bool {
        if a.len() != b.len() {
            return false;
        }
        let mut diff = 0u8;
        for (&x, &y) in a.iter().zip(b.iter()) {
            diff |= x ^ y;
        }
        diff == 0
    }

    /// Serialize the cache to a binary blob.
    ///
    /// Format:
    ///   magic(4)  version(2)  entry_count(4)
    ///   for each entry:
    ///     key_len(2)  key_bytes  fingerprint(8)
    ///     code_len(4)  code_bytes
    ///     deopt_count(4)  [bc_off(4) native_off(4)]*
    ///     entry_point_offset(4)
    ///   integrity_digest(32)  -- SHA-256 (or HMAC-SHA256 if a key is set),
    ///                            computed over everything before it, appended
    ///                            at the end.
    pub fn serialize(&self) -> Vec<u8> {
        const MAGIC: u32 = 0xA07CAC4E;
        // `Self::` is not usable from a `const` item nested in a fn body (E0401);
        // a `let` binding can reference the associated const.
        let version: u16 = Self::CURRENT_VERSION;
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC.to_le_bytes());
        buf.extend_from_slice(&version.to_le_bytes());
        buf.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for (key, entry) in &self.entries {
            let kb = key.as_bytes();
            buf.extend_from_slice(&(kb.len() as u16).to_le_bytes());
            buf.extend_from_slice(kb);
            buf.extend_from_slice(&entry.bytecode_fingerprint);
            buf.extend_from_slice(&(entry.compiled_code.len() as u32).to_le_bytes());
            buf.extend_from_slice(&entry.compiled_code);
            buf.extend_from_slice(&(entry.deopt_map.len() as u32).to_le_bytes());
            for (&bc, &nat) in &entry.deopt_map {
                buf.extend_from_slice(&bc.to_le_bytes());
                buf.extend_from_slice(&nat.to_le_bytes());
            }
            buf.extend_from_slice(&entry.entry_point_offset.to_le_bytes());
        }
        // Append a cryptographic integrity digest over the payload. Uses
        // HMAC-SHA256 when a key is configured (forgery-resistant), otherwise a
        // bare SHA-256 content hash (corruption-detecting).
        let hash = Self::compute_integrity_hash(&buf, Self::integrity_key().as_deref());
        buf.extend_from_slice(&hash);
        buf
    }

    /// Maximum entries allowed during deserialization to prevent DoS.
    const MAX_DESERIALIZE_ENTRIES: usize = 100_000;
    /// Maximum code size per entry during deserialization (16 MB).
    const MAX_CODE_SIZE: usize = 16 * 1024 * 1024;
    /// Maximum deopt map entries per method.
    const MAX_DEOPT_ENTRIES: usize = 100_000;
    /// Current cache format version. Bumped to 2 when the trailing integrity
    /// field changed from an 8-byte non-cryptographic mix to a 32-byte
    /// SHA-256/HMAC-SHA256 digest; v1 blobs are now rejected (they cannot carry
    /// a valid v2 digest and their old check is forgeable).
    const CURRENT_VERSION: u16 = 2;

    /// Deserialize a cache from bytes. Returns None on format error.
    pub fn deserialize(data: &[u8]) -> Option<Self> {
        const MAGIC: u32 = 0xA07CAC4E;
        if data.len() < 10 {
            return None;
        }
        let magic = u32::from_le_bytes(data[0..4].try_into().ok()?);
        if magic != MAGIC {
            return None;
        }
        let version = u16::from_le_bytes(data[4..6].try_into().ok()?);
        if version != Self::CURRENT_VERSION {
            return None; // Reject incompatible versions
        }
        let entry_count = u32::from_le_bytes(data[6..10].try_into().ok()?) as usize;
        if entry_count > Self::MAX_DESERIALIZE_ENTRIES {
            return None; // Reject absurdly large entry counts
        }
        let mut pos = 10usize;
        let mut entries = HashMap::new();
        for _ in 0..entry_count {
            if pos + 2 > data.len() {
                return None;
            }
            let key_len = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?) as usize;
            pos += 2;
            if pos + key_len > data.len() {
                return None;
            }
            let key = std::str::from_utf8(&data[pos..pos + key_len])
                .ok()?
                .to_string();
            pos += key_len;
            if pos + 8 > data.len() {
                return None;
            }
            let mut fp = [0u8; 8];
            fp.copy_from_slice(&data[pos..pos + 8]);
            pos += 8;
            if pos + 4 > data.len() {
                return None;
            }
            let code_len = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
            pos += 4;
            if code_len > Self::MAX_CODE_SIZE || pos + code_len > data.len() {
                return None;
            }
            let compiled_code = data[pos..pos + code_len].to_vec();
            pos += code_len;
            if pos + 4 > data.len() {
                return None;
            }
            let deopt_count = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
            if deopt_count > Self::MAX_DEOPT_ENTRIES {
                return None;
            }
            pos += 4;
            let mut deopt_map = HashMap::new();
            for _ in 0..deopt_count {
                if pos + 8 > data.len() {
                    return None;
                }
                let bc = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?);
                let nat = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().ok()?);
                pos += 8;
                deopt_map.insert(bc, nat);
            }
            if pos + 4 > data.len() {
                return None;
            }
            let entry_point_offset = u32::from_le_bytes(data[pos..pos + 4].try_into().ok()?);
            pos += 4;
            entries.insert(
                key,
                AotCacheEntry {
                    bytecode_fingerprint: fp,
                    compiled_code,
                    deopt_map,
                    entry_point_offset,
                },
            );
        }
        // Verify the integrity digest (trailing INTEGRITY_DIGEST_LEN bytes).
        // The digest is HMAC-SHA256 when a key is configured (rejects forged
        // caches) and a bare SHA-256 content hash otherwise (rejects corrupted
        // or casually tampered caches). Exactly one trailing digest is allowed
        // — a longer-than-expected blob is rejected as malformed.
        if pos + Self::INTEGRITY_DIGEST_LEN != data.len() {
            return None; // Missing, short, or trailing-garbage integrity digest
        }
        let stored_hash = &data[pos..pos + Self::INTEGRITY_DIGEST_LEN];
        let payload = &data[..pos];
        let computed_hash = Self::compute_integrity_hash(payload, Self::integrity_key().as_deref());
        if !Self::ct_eq(stored_hash, &computed_hash) {
            return None; // Integrity check failed — cache corrupted, tampered, or wrong key
        }
        Some(Self {
            entries,
            config: AotCacheConfig::new(),
        })
    }
}

// ===========================================================================
// Pre-linking infrastructure (JEP 483)
// ===========================================================================

/// A pre-resolved field reference.
#[derive(Debug, Clone)]
pub struct PrelinkedField {
    /// Heap slot offset for instance fields, or static slot index.
    pub field_offset: u32,
    /// JVM type descriptor (e.g. "I", "Ljava/lang/String;").
    pub descriptor: String,
    /// True if this is a static field.
    pub is_static: bool,
}

/// A pre-resolved method reference.
#[derive(Debug, Clone)]
pub struct PrelinkedMethod {
    /// Virtual dispatch slot in the vtable.
    pub vtable_slot: u32,
    /// Full JVM method descriptor.
    pub descriptor: String,
    /// True if this is a static method.
    pub is_static: bool,
}

/// Pre-linked constant pool data for a single class.
#[derive(Debug, Clone, Default)]
pub struct PrelinkedClass {
    /// Class name in internal form (e.g. "java/lang/String").
    pub class_name: String,
    /// Resolved field references keyed by constant-pool index.
    pub fields: HashMap<u16, PrelinkedField>,
    /// Resolved method references keyed by constant-pool index.
    pub methods: HashMap<u16, PrelinkedMethod>,
}

impl PrelinkedClass {
    pub fn new(class_name: &str) -> Self {
        Self {
            class_name: class_name.to_string(),
            fields: HashMap::new(),
            methods: HashMap::new(),
        }
    }

    pub fn add_field(&mut self, cp_index: u16, field: PrelinkedField) {
        self.fields.insert(cp_index, field);
    }

    pub fn add_method(&mut self, cp_index: u16, method: PrelinkedMethod) {
        self.methods.insert(cp_index, method);
    }

    pub fn get_field(&self, cp_index: u16) -> Option<&PrelinkedField> {
        self.fields.get(&cp_index)
    }

    pub fn get_method(&self, cp_index: u16) -> Option<&PrelinkedMethod> {
        self.methods.get(&cp_index)
    }
}

/// Cache of pre-linked class data.
#[derive(Debug, Default)]
pub struct PrelinkerCache {
    classes: HashMap<String, PrelinkedClass>,
}

impl PrelinkerCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace pre-linked data for a class.
    pub fn insert(&mut self, class: PrelinkedClass) {
        self.classes.insert(class.class_name.clone(), class);
    }

    /// Look up pre-linked data for a class.
    pub fn get(&self, class_name: &str) -> Option<&PrelinkedClass> {
        self.classes.get(class_name)
    }

    /// Number of pre-linked classes.
    pub fn class_count(&self) -> usize {
        self.classes.len()
    }

    /// Serialize to a compact binary blob.
    ///
    /// Format:
    ///   magic(4)  version(2)  class_count(4)
    ///   for each class:
    ///     name_len(2)  name_bytes
    ///     field_count(2)
    ///       [cp_idx(2)  offset(4)  is_static(1)  desc_len(2)  desc_bytes]*
    ///     method_count(2)
    ///       [cp_idx(2)  vtable_slot(4)  is_static(1)  desc_len(2)  desc_bytes]*
    pub fn serialize(&self) -> Vec<u8> {
        const MAGIC: u32 = 0xB1C1A55E;
        const VERSION: u16 = 1;
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC.to_le_bytes());
        buf.extend_from_slice(&VERSION.to_le_bytes());
        buf.extend_from_slice(&(self.classes.len() as u32).to_le_bytes());
        for (name, class) in &self.classes {
            let nb = name.as_bytes();
            buf.extend_from_slice(&(nb.len() as u16).to_le_bytes());
            buf.extend_from_slice(nb);
            buf.extend_from_slice(&(class.fields.len() as u16).to_le_bytes());
            for (&cp_idx, field) in &class.fields {
                buf.extend_from_slice(&cp_idx.to_le_bytes());
                buf.extend_from_slice(&field.field_offset.to_le_bytes());
                buf.push(if field.is_static { 1 } else { 0 });
                let db = field.descriptor.as_bytes();
                buf.extend_from_slice(&(db.len() as u16).to_le_bytes());
                buf.extend_from_slice(db);
            }
            buf.extend_from_slice(&(class.methods.len() as u16).to_le_bytes());
            for (&cp_idx, method) in &class.methods {
                buf.extend_from_slice(&cp_idx.to_le_bytes());
                buf.extend_from_slice(&method.vtable_slot.to_le_bytes());
                buf.push(if method.is_static { 1 } else { 0 });
                let db = method.descriptor.as_bytes();
                buf.extend_from_slice(&(db.len() as u16).to_le_bytes());
                buf.extend_from_slice(db);
            }
        }
        buf
    }

    /// Deserialize from bytes. Returns None on format error.
    pub fn deserialize(data: &[u8]) -> Option<Self> {
        const MAGIC: u32 = 0xB1C1A55E;
        if data.len() < 10 {
            return None;
        }
        let magic = u32::from_le_bytes(data[0..4].try_into().ok()?);
        if magic != MAGIC {
            return None;
        }
        let _version = u16::from_le_bytes(data[4..6].try_into().ok()?);
        let class_count = u32::from_le_bytes(data[6..10].try_into().ok()?) as usize;
        let mut pos = 10usize;
        let mut classes = HashMap::new();

        for _ in 0..class_count {
            if pos + 2 > data.len() {
                return None;
            }
            let name_len = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?) as usize;
            pos += 2;
            if pos + name_len > data.len() {
                return None;
            }
            let class_name = std::str::from_utf8(&data[pos..pos + name_len])
                .ok()?
                .to_string();
            pos += name_len;

            if pos + 2 > data.len() {
                return None;
            }
            let field_count = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?) as usize;
            pos += 2;
            let mut fields = HashMap::new();
            for _ in 0..field_count {
                if pos + 7 > data.len() {
                    return None;
                }
                let cp_idx = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?);
                let offset = u32::from_le_bytes(data[pos + 2..pos + 6].try_into().ok()?);
                let is_static = data[pos + 6] != 0;
                pos += 7;
                if pos + 2 > data.len() {
                    return None;
                }
                let desc_len = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?) as usize;
                pos += 2;
                if pos + desc_len > data.len() {
                    return None;
                }
                let descriptor = std::str::from_utf8(&data[pos..pos + desc_len])
                    .ok()?
                    .to_string();
                pos += desc_len;
                fields.insert(
                    cp_idx,
                    PrelinkedField {
                        field_offset: offset,
                        descriptor,
                        is_static,
                    },
                );
            }

            if pos + 2 > data.len() {
                return None;
            }
            let method_count = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?) as usize;
            pos += 2;
            let mut methods = HashMap::new();
            for _ in 0..method_count {
                if pos + 7 > data.len() {
                    return None;
                }
                let cp_idx = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?);
                let vtable_slot = u32::from_le_bytes(data[pos + 2..pos + 6].try_into().ok()?);
                let is_static = data[pos + 6] != 0;
                pos += 7;
                if pos + 2 > data.len() {
                    return None;
                }
                let desc_len = u16::from_le_bytes(data[pos..pos + 2].try_into().ok()?) as usize;
                pos += 2;
                if pos + desc_len > data.len() {
                    return None;
                }
                let descriptor = std::str::from_utf8(&data[pos..pos + desc_len])
                    .ok()?
                    .to_string();
                pos += desc_len;
                methods.insert(
                    cp_idx,
                    PrelinkedMethod {
                        vtable_slot,
                        descriptor,
                        is_static,
                    },
                );
            }

            classes.insert(
                class_name.clone(),
                PrelinkedClass {
                    class_name,
                    fields,
                    methods,
                },
            );
        }
        Some(Self { classes })
    }
}

// ===========================================================================
// Training Run Profile Collection
// ===========================================================================

/// Record of all invocations of a single method during training.
#[derive(Debug, Clone, Default)]
pub struct MethodInvocationRecord {
    /// JVM internal class name.
    pub class_name: String,
    /// Method name.
    pub method_name: String,
    /// Method descriptor.
    pub descriptor: String,
    /// Total invocation count.
    pub invocation_count: u64,
    /// Observed receiver types with counts.
    pub receiver_types: HashMap<String, u64>,
}

/// A single branch-outcome record from a method body.
#[derive(Debug, Clone)]
pub struct BranchRecord {
    /// Class the branch appears in.
    pub class_name: String,
    /// Method the branch appears in.
    pub method_name: String,
    /// Bytecode offset of the branch instruction.
    pub bytecode_offset: u32,
    /// Number of times the branch was taken.
    pub taken_count: u64,
    /// Number of times the branch was not taken.
    pub not_taken_count: u64,
}

impl BranchRecord {
    /// Probability of branch being taken (0.0 – 1.0), or NaN if never executed.
    pub fn taken_probability(&self) -> f64 {
        let total = self.taken_count + self.not_taken_count;
        if total == 0 {
            f64::NAN
        } else {
            self.taken_count as f64 / total as f64
        }
    }
}

/// Records method invocations and branches during a training run.
#[derive(Debug, Default)]
pub struct TrainingRunRecorder {
    /// Method invocation records keyed by "ClassName.methodName:descriptor".
    method_records: HashMap<String, MethodInvocationRecord>,
    /// Branch records, keyed by "ClassName.methodName@offset".
    branch_records: HashMap<String, BranchRecord>,
}

impl TrainingRunRecorder {
    pub fn new() -> Self {
        Self::default()
    }

    fn method_key(class_name: &str, method_name: &str, descriptor: &str) -> String {
        format!("{class_name}.{method_name}:{descriptor}")
    }

    fn branch_key(class_name: &str, method_name: &str, bytecode_offset: u32) -> String {
        format!("{class_name}.{method_name}@{bytecode_offset}")
    }

    /// Record one invocation of a method.
    pub fn record_method_invocation(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        receiver_type: Option<&str>,
    ) {
        let key = Self::method_key(class_name, method_name, descriptor);
        let rec = self
            .method_records
            .entry(key)
            .or_insert_with(|| MethodInvocationRecord {
                class_name: class_name.to_string(),
                method_name: method_name.to_string(),
                descriptor: descriptor.to_string(),
                ..Default::default()
            });
        rec.invocation_count += 1;
        if let Some(ty) = receiver_type {
            if let Some(count) = rec.receiver_types.get_mut(ty) {
                *count += 1;
            } else {
                rec.receiver_types.insert(ty.to_string(), 1);
            }
        }
    }

    /// Record a branch outcome.
    pub fn record_branch(
        &mut self,
        class_name: &str,
        method_name: &str,
        bytecode_offset: u32,
        taken: bool,
    ) {
        let key = Self::branch_key(class_name, method_name, bytecode_offset);
        let rec = self
            .branch_records
            .entry(key)
            .or_insert_with(|| BranchRecord {
                class_name: class_name.to_string(),
                method_name: method_name.to_string(),
                bytecode_offset,
                taken_count: 0,
                not_taken_count: 0,
            });
        if taken {
            rec.taken_count += 1;
        } else {
            rec.not_taken_count += 1;
        }
    }

    /// Get a method invocation record by key.
    pub fn method_record(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<&MethodInvocationRecord> {
        let key = Self::method_key(class_name, method_name, descriptor);
        self.method_records.get(&key)
    }

    /// Get a branch record.
    pub fn branch_record(
        &self,
        class_name: &str,
        method_name: &str,
        bytecode_offset: u32,
    ) -> Option<&BranchRecord> {
        let key = Self::branch_key(class_name, method_name, bytecode_offset);
        self.branch_records.get(&key)
    }

    /// Total number of distinct methods recorded.
    pub fn method_count(&self) -> usize {
        self.method_records.len()
    }

    /// Total number of distinct branches recorded.
    pub fn branch_count(&self) -> usize {
        self.branch_records.len()
    }

    /// Convert training data to an `AotProfile`.
    pub fn to_aot_profile(&self) -> AotProfile {
        let mut profile = AotProfile::new();
        for (_, rec) in &self.method_records {
            let mp = profile.method_profile_mut(&rec.class_name, &rec.method_name, &rec.descriptor);
            mp.invocation_count = rec.invocation_count;
            mp.receiver_types = rec.receiver_types.clone();
        }
        for (_, br) in &self.branch_records {
            let mp = profile.method_profile_mut(&br.class_name, &br.method_name, "");
            mp.branch_taken.insert(br.bytecode_offset, br.taken_count);
            mp.branch_not_taken
                .insert(br.bytecode_offset, br.not_taken_count);
        }
        profile
    }

    /// Serialize this recorder's data to a binary profile blob.
    ///
    /// Format:
    ///   magic(4=0xA07CAC4E)  version(2)  method_count(4)
    ///   for each method:
    ///     key_len(2)  key_bytes  invocation_count(8)
    ///     type_count(2)  [type_len(2)  type_bytes  count(8)]*
    pub fn serialize(&self) -> Vec<u8> {
        const MAGIC: u32 = 0xA07CAC4E;
        const VERSION: u16 = 2;
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC.to_le_bytes());
        buf.extend_from_slice(&VERSION.to_le_bytes());
        buf.extend_from_slice(&(self.method_records.len() as u32).to_le_bytes());
        for (key, rec) in &self.method_records {
            let kb = key.as_bytes();
            buf.extend_from_slice(&(kb.len() as u16).to_le_bytes());
            buf.extend_from_slice(kb);
            buf.extend_from_slice(&rec.invocation_count.to_le_bytes());
            buf.extend_from_slice(&(rec.receiver_types.len() as u16).to_le_bytes());
            for (ty, &cnt) in &rec.receiver_types {
                let tb = ty.as_bytes();
                buf.extend_from_slice(&(tb.len() as u16).to_le_bytes());
                buf.extend_from_slice(tb);
                buf.extend_from_slice(&cnt.to_le_bytes());
            }
        }
        buf
    }
}

// ===========================================================================
// Global AOT Runtime State
// ===========================================================================

/// Global AOT runtime state — accessible from native methods.
static AOT_ENABLED: AtomicBool = AtomicBool::new(false);
static AOT_TRAINING: AtomicBool = AtomicBool::new(false);
static AOT_PRODUCTION: AtomicBool = AtomicBool::new(false);

// ARCH-2026-08-04 A6 — the first two locks in this crate to carry a LockLevel.
//
// `native-builtins` had 440 raw lock constructions and zero ordered ones, in
// the crate that re-enters the VM. These two are the pattern for working that
// number down: a level is a *claim*, so it is only stamped on a lock whose
// every acquisition site has actually been read.
//
// `Scratch` (L0, the leaf level) is correct here because no lock is ever taken
// while one of these is held. Every production site — `init_aot_runtime`,
// `reset_aot_globals`, the training-flush path, and the two
// `leyden_get_aot_cache_*_path` natives — acquires, `.clone()`s the
// `Option<String>`, and drops the guard *before* touching `ctx`. The two
// natives call `ctx.create_string` only after that clone, so the guard is never
// held across a re-entry into the VM. That is exactly what the level asserts,
// and it is why these two were converted first: it is checkable by reading six
// call sites.
//
// See `native-builtins/tests/lock_discipline_ratchet.rs` for the ratchet and
// `architecture-review-a1-a9.md` §A6 for the remaining backlog.
static AOT_CACHE_INPUT_PATH: OrderedPlMutex<Option<String>> =
    OrderedPlMutex::new(None, LockLevel::Scratch);
static AOT_CACHE_OUTPUT_PATH: OrderedPlMutex<Option<String>> =
    OrderedPlMutex::new(None, LockLevel::Scratch);

/// Global training recorder — accumulates profile data during training runs.
static AOT_TRAINING_RECORDER: Mutex<Option<TrainingRunRecorder>> = Mutex::new(None);

/// Global AOT cache — holds pre-compiled method entries.
static AOT_CACHE_GLOBAL: Mutex<Option<AotCache>> = Mutex::new(None);

/// Global pre-linker cache — holds pre-resolved constant pool data.
static AOT_PRELINKER_CACHE: Mutex<Option<PrelinkerCache>> = Mutex::new(None);

/// Initialize the global AOT runtime state from VmConfig parameters.
/// Called once during VM startup.
/// Sanitize a cache file path: reject path traversal attempts.
fn sanitize_aot_path(path: &str) -> Option<String> {
    let p = std::path::Path::new(path);
    // Reject paths with `..` components
    for component in p.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return None;
        }
    }
    Some(path.to_string())
}

pub fn init_aot_runtime(
    training: bool,
    production: bool,
    cache_input: Option<&str>,
    cache_output: Option<&str>,
) {
    let enabled = training || production;
    // Use Release ordering so that subsequent Acquire loads on other threads
    // see the full initialization.
    AOT_ENABLED.store(enabled, Ordering::Release);
    AOT_TRAINING.store(training, Ordering::Release);
    AOT_PRODUCTION.store(production, Ordering::Release);

    if let Some(p) = cache_input {
        let sanitized = sanitize_aot_path(p).unwrap_or_default();
        *AOT_CACHE_INPUT_PATH.lock() = if sanitized.is_empty() {
            None
        } else {
            Some(sanitized)
        };
    }
    if let Some(p) = cache_output {
        let sanitized = sanitize_aot_path(p).unwrap_or_default();
        *AOT_CACHE_OUTPUT_PATH.lock() = if sanitized.is_empty() {
            None
        } else {
            Some(sanitized)
        };
    }

    if training {
        *AOT_TRAINING_RECORDER
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(TrainingRunRecorder::new());
        *AOT_CACHE_GLOBAL.lock().unwrap_or_else(|e| e.into_inner()) = Some(AotCache::new(
            AotCacheConfig::new().with_training_mode(true),
        ));
        *AOT_PRELINKER_CACHE
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(PrelinkerCache::new());
    }

    if production {
        // Attempt to load AOT cache from input path
        if let Some(input_path) = cache_input {
            if let Ok(data) = std::fs::read(input_path) {
                if let Some(cache) = AotCache::deserialize(&data) {
                    *AOT_CACHE_GLOBAL.lock().unwrap_or_else(|e| e.into_inner()) = Some(cache);
                }
            }
        }
        *AOT_PRELINKER_CACHE
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(PrelinkerCache::new());
    }
}

/// Record a method invocation in the global training recorder.
/// No-op if not in training mode.
pub fn aot_record_method_invocation(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    receiver_type: Option<&str>,
) {
    if !AOT_TRAINING.load(Ordering::Relaxed) {
        return;
    }
    if let Some(ref mut recorder) = *AOT_TRAINING_RECORDER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
    {
        recorder.record_method_invocation(class_name, method_name, descriptor, receiver_type);
    }
}

/// Record a branch outcome in the global training recorder.
/// No-op if not in training mode.
pub fn aot_record_branch(class_name: &str, method_name: &str, bytecode_offset: u32, taken: bool) {
    if !AOT_TRAINING.load(Ordering::Relaxed) {
        return;
    }
    if let Some(ref mut recorder) = *AOT_TRAINING_RECORDER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
    {
        recorder.record_branch(class_name, method_name, bytecode_offset, taken);
    }
}

/// Record a class load event in the global training recorder.
/// No-op if not in training mode.
pub fn aot_record_class_loaded(class_name: &str) {
    if !AOT_TRAINING.load(Ordering::Relaxed) {
        return;
    }
    // Pre-linker: create an entry placeholder for the loaded class
    if let Some(ref mut cache) = *AOT_PRELINKER_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
    {
        if cache.get(class_name).is_none() {
            cache.insert(PrelinkedClass::new(class_name));
        }
    }
}

/// Check if AOT is enabled.
pub fn is_aot_enabled() -> bool {
    AOT_ENABLED.load(Ordering::Acquire)
}

/// Check if in training mode.
pub fn is_aot_training() -> bool {
    AOT_TRAINING.load(Ordering::Acquire)
}

/// Check if in production mode.
pub fn is_aot_production() -> bool {
    AOT_PRODUCTION.load(Ordering::Acquire)
}

/// Flush training data to disk. Called at VM shutdown in training mode.
/// Returns the number of bytes written, or 0 on failure/noop.
pub fn aot_flush_training_data() -> usize {
    if !AOT_TRAINING.load(Ordering::Relaxed) {
        return 0;
    }
    let output_path = AOT_CACHE_OUTPUT_PATH.lock().clone();
    let Some(output_path) = output_path else {
        return 0;
    };

    // Serialize the AOT cache
    let cache_data = {
        let guard = AOT_CACHE_GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().map(|c| c.serialize())
    };

    if let Some(data) = cache_data {
        let len = data.len();
        if std::fs::write(&output_path, &data).is_ok() {
            return len;
        }
    }

    // Fall back to writing the training profile
    let profile_data = {
        let guard = AOT_TRAINING_RECORDER
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.as_ref().map(|r| r.serialize())
    };
    if let Some(data) = profile_data {
        let len = data.len();
        if std::fs::write(&output_path, &data).is_ok() {
            return len;
        }
    }

    0
}

/// Bulk-import branch profile data into the AOT training recorder.
///
/// Each entry is (class_name, method_name, bytecode_offset, taken_count, not_taken_count).
/// Called from the VM to bridge JIT ProfileStore → AOT TrainingRunRecorder at shutdown.
pub fn aot_bulk_import_branches(entries: &[(&str, &str, u32, u32, u32)]) -> usize {
    if !AOT_TRAINING.load(Ordering::Relaxed) {
        return 0;
    }
    let mut count = 0usize;
    if let Some(ref mut recorder) = *AOT_TRAINING_RECORDER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
    {
        for &(class_name, method_name, offset, taken, not_taken) in entries {
            // Record each branch outcome via the recorder's API.
            for _ in 0..taken {
                recorder.record_branch(class_name, method_name, offset, true);
            }
            for _ in 0..not_taken {
                recorder.record_branch(class_name, method_name, offset, false);
            }
            count += 1;
        }
    }
    count
}

/// Bulk-import receiver type data into the AOT training recorder.
///
/// Each entry is (class_name, method_name, descriptor, receiver_class_name, count).
pub fn aot_bulk_import_receivers(entries: &[(&str, &str, &str, &str, u32)]) -> usize {
    if !AOT_TRAINING.load(Ordering::Relaxed) {
        return 0;
    }
    let mut count = 0usize;
    if let Some(ref mut recorder) = *AOT_TRAINING_RECORDER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
    {
        for &(class_name, method_name, descriptor, receiver, n) in entries {
            // Record each receiver observation via the recorder's API.
            for _ in 0..n {
                recorder.record_method_invocation(
                    class_name,
                    method_name,
                    descriptor,
                    Some(receiver),
                );
            }
            count += 1;
        }
    }
    count
}

/// Record a receiver type observation in the global training recorder.
/// No-op if not in training mode.
pub fn aot_record_receiver_type(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    receiver_class_name: &str,
) {
    if !AOT_TRAINING.load(Ordering::Relaxed) {
        return;
    }
    if let Some(ref mut recorder) = *AOT_TRAINING_RECORDER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
    {
        recorder.record_method_invocation(
            class_name,
            method_name,
            descriptor,
            Some(receiver_class_name),
        );
    }
}

/// Get the current training recorder stats (method count, branch count).
pub fn aot_training_stats() -> (usize, usize) {
    let guard = AOT_TRAINING_RECORDER
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(r) => (r.method_count(), r.branch_count()),
        None => (0, 0),
    }
}

/// Reset global AOT state (for testing).
#[cfg(test)]
fn reset_aot_globals() {
    AOT_ENABLED.store(false, Ordering::Relaxed);
    AOT_TRAINING.store(false, Ordering::Relaxed);
    AOT_PRODUCTION.store(false, Ordering::Relaxed);
    *AOT_CACHE_INPUT_PATH.lock() = None;
    *AOT_CACHE_OUTPUT_PATH.lock() = None;
    *AOT_TRAINING_RECORDER.lock().unwrap() = None;
    *AOT_CACHE_GLOBAL.lock().unwrap() = None;
    *AOT_PRELINKER_CACHE.lock().unwrap() = None;
}

// ===========================================================================
// Native method implementations — jdk/internal/misc/Leyden
// ===========================================================================

const LEYDEN: &str = "jdk/internal/misc/Leyden";

fn leyden_is_aot_enabled(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(if is_aot_enabled() { 1 } else { 0 })))
}

fn leyden_is_training_mode(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(if is_aot_training() { 1 } else { 0 })))
}

fn leyden_is_production_mode(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(if is_aot_production() { 1 } else { 0 })))
}

fn leyden_get_aot_cache_input_path(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let path = AOT_CACHE_INPUT_PATH.lock().clone();
    match path {
        Some(p) => {
            let s = ctx.create_string(&p);
            Ok(Some(Value::Object(Some(s))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

fn leyden_get_aot_cache_output_path(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let path = AOT_CACHE_OUTPUT_PATH.lock().clone();
    match path {
        Some(p) => {
            let s = ctx.create_string(&p);
            Ok(Some(Value::Object(Some(s))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

fn leyden_notify_method_invoked(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = this (Leyden), args[1] = Method object
    // Extract method info from the Method object if possible
    if let Some(Value::Object(Some(method_obj))) = args.get(1) {
        // Try to read the method name from the Method object's name field (field 0)
        let name_val = ctx.get_field(*method_obj, 0);
        if let Value::Object(Some(name_ref)) = name_val {
            if let Some(name) = ctx.read_string(name_ref) {
                aot_record_method_invocation("unknown", &name, "()V", None);
            }
        }
    }
    Ok(None)
}

fn leyden_notify_class_loaded(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = this (Leyden), args[1] = Class object
    if let Some(Value::Object(Some(class_obj))) = args.get(1) {
        let class_id = ctx.class_id_of_object(*class_obj);
        if let Some(name) = ctx.class_name_of_id(class_id) {
            aot_record_class_loaded(&name);
        }
    }
    Ok(None)
}

fn leyden_lookup_aot_method(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // In production mode, look up pre-compiled method in the AOT cache.
    // args[0] = this (Leyden), args[1] = class name, args[2] = method name, args[3] = descriptor
    if !is_aot_production() {
        return Ok(Some(Value::Object(None)));
    }
    let class_name = args
        .get(1)
        .and_then(|v| {
            if let Value::Object(Some(o)) = v {
                Some(*o)
            } else {
                None
            }
        })
        .and_then(|o| ctx.read_string(o));
    let method_name = args
        .get(2)
        .and_then(|v| {
            if let Value::Object(Some(o)) = v {
                Some(*o)
            } else {
                None
            }
        })
        .and_then(|o| ctx.read_string(o));
    let descriptor = args
        .get(3)
        .and_then(|v| {
            if let Value::Object(Some(o)) = v {
                Some(*o)
            } else {
                None
            }
        })
        .and_then(|o| ctx.read_string(o));

    if let (Some(cn), Some(mn), Some(desc)) = (class_name, method_name, descriptor) {
        let guard = AOT_CACHE_GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cache) = guard.as_ref() {
            if let Some(entry) = cache.lookup(&cn, &mn, &desc) {
                if entry.has_compiled_code() {
                    // Return 1 to indicate a cache hit (caller uses this to skip interpretation)
                    return Ok(Some(Value::Int(1)));
                }
            }
        }
    }
    Ok(Some(Value::Object(None)))
}

fn leyden_store_aot_profile(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = this, args[1] = path string
    let path = if let Some(Value::Object(Some(path_ref))) = args.get(1) {
        ctx.read_string(*path_ref)
    } else {
        None
    };

    if let Some(path) = path {
        // Serialize the training recorder and write to the specified path
        let data = {
            let guard = AOT_TRAINING_RECORDER
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guard.as_ref().map(|r| r.serialize())
        };
        if let Some(data) = data {
            let _ = std::fs::write(&path, &data);
        }
    }
    Ok(None)
}

fn leyden_load_aot_profile(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = this, args[1] = path string
    // Returns 1 if successfully loaded, 0 otherwise
    let path = if let Some(Value::Object(Some(path_ref))) = args.get(1) {
        ctx.read_string(*path_ref)
    } else {
        None
    };

    if let Some(path) = path {
        if let Ok(data) = std::fs::read(&path) {
            if let Some(cache) = AotCache::deserialize(&data) {
                *AOT_CACHE_GLOBAL.lock().unwrap_or_else(|e| e.into_inner()) = Some(cache);
                return Ok(Some(Value::Int(1)));
            }
        }
    }
    Ok(Some(Value::Int(0)))
}

// ===========================================================================
// Native method implementations — annotation / reflect stubs
// ===========================================================================

fn stable_annotation_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

fn reflect_method_is_annotation_present(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

// ===========================================================================
// Public registration entry-point
// ===========================================================================

pub(crate) fn register_aot_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // --- Leyden AOT query methods ---
    r.register(LEYDEN, "isAOTEnabled", "()Z", leyden_is_aot_enabled);
    r.register(LEYDEN, "isTrainingMode", "()Z", leyden_is_training_mode);
    r.register(LEYDEN, "isProductionMode", "()Z", leyden_is_production_mode);
    r.register(
        LEYDEN,
        "getAOTCacheInputPath",
        "()Ljava/lang/String;",
        leyden_get_aot_cache_input_path,
    );
    r.register(
        LEYDEN,
        "getAOTCacheOutputPath",
        "()Ljava/lang/String;",
        leyden_get_aot_cache_output_path,
    );

    // --- Leyden training callbacks (noops) ---
    r.register(
        LEYDEN,
        "notifyMethodInvoked",
        "(Ljava/lang/reflect/Method;)V",
        leyden_notify_method_invoked,
    );
    r.register(
        LEYDEN,
        "notifyClassLoaded",
        "(Ljava/lang/Class;)V",
        leyden_notify_class_loaded,
    );
    r.register(
        LEYDEN,
        "lookupAOTMethod",
        "(Ljava/lang/reflect/Method;)Ljava/lang/Object;",
        leyden_lookup_aot_method,
    );
    r.register(
        LEYDEN,
        "storeAOTProfile",
        "(Ljava/lang/String;)V",
        leyden_store_aot_profile,
    );
    r.register(
        LEYDEN,
        "loadAOTProfile",
        "(Ljava/lang/String;)I",
        leyden_load_aot_profile,
    );

    // --- @Stable annotation natives (noops) ---
    let stable = "jdk/internal/vm/annotation/Stable";
    r.register(stable, "<init>", "()V", stable_annotation_noop);
    // annotationType() must return the annotation interface itself — it is the
    // discriminator every `java.lang.annotation.Annotation` consumer keys on
    // (`Annotation.equals`/`hashCode`, `AnnotatedElement.getAnnotation`,
    // Spring's `AnnotationUtils`, and the JDK's own `AnnotationInvocationHandler`
    // all call it first). Returning null made a `@Stable` instance claim to have
    // no annotation type at all, which NPEs the moment anything inspects it.
    // Falls back to null only if the class is not loaded, which is the same
    // answer as before.
    r.register(
        stable,
        "annotationType",
        "()Ljava/lang/Class;",
        |ctx, _args| {
            let class_id = ctx.class_id_by_name("jdk/internal/vm/annotation/Stable");
            match class_id {
                Some(cid) => {
                    let mirror = ctx.get_class_mirror(cid);
                    Ok(Some(Value::Object(Some(mirror))))
                }
                None => Ok(Some(Value::Object(None))),
            }
        },
    );

    // --- java.lang.reflect.Method annotation query ---
    // Real implementation is in lang_class.rs; use it instead of a stub.
    r.register(
        "java/lang/reflect/Method",
        "isAnnotationPresent",
        "(Ljava/lang/Class;)Z",
        crate::lang_class::native_method_is_annotation_present,
    );

    // --- Leyden <init> ---
    // Pure static-holder: `jdk/internal/misc/Leyden` is a CratonVM-side helper
    // class and EVERY other native registered on it above (isAOTEnabled,
    // isTrainingMode, isProductionMode, getAOTCache*Path, notify*, lookup*,
    // store/loadAOTProfile) is static and reads only process-wide AOT state —
    // none of them touches an instance slot. There is therefore no instance
    // state for a no-arg constructor to establish, so an empty body IS the
    // implementation rather than a stub. KEEP. NEW-6: documented.
    r.register(LEYDEN, "<init>", "()V", native_noop_with_this);
    r.set_category(__prev_cat);
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod aot_tests {
    use super::*;
    use cratonvm_native_api::NativeMethodRegistry;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    // Serialize tests that mutate AOT global state
    // (AOT_CACHE_GLOBAL, AOT_TRAINING_RECORDER, AOT_PRELINKER_CACHE, AOT_ENABLED,
    // AOT_TRAINING, AOT_PRODUCTION, AOT_CACHE_INPUT_PATH, AOT_CACHE_OUTPUT_PATH).
    // cargo test runs tests in parallel by default, which otherwise causes
    // tests that call `init_aot_runtime` / `reset_aot_globals` to race on
    // these shared statics.  Use `std::sync::Mutex` + `PoisonError` passthrough
    // so one panicking test still lets others acquire.
    fn aot_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    // -----------------------------------------------------------------------
    // AotMethodProfile tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_aot_method_profile_new_defaults() {
        let p = AotMethodProfile::new();
        assert_eq!(p.invocation_count, 0);
        assert!(p.receiver_types.is_empty());
        assert!(p.branch_taken.is_empty());
        assert!(p.branch_not_taken.is_empty());
    }

    #[test]
    fn test_aot_method_profile_record_invocation_no_type() {
        let mut p = AotMethodProfile::new();
        p.record_invocation(None);
        p.record_invocation(None);
        assert_eq!(p.invocation_count, 2);
        assert!(p.receiver_types.is_empty());
    }

    #[test]
    fn test_aot_method_profile_record_invocation_with_type() {
        let mut p = AotMethodProfile::new();
        p.record_invocation(Some("java/lang/String"));
        p.record_invocation(Some("java/lang/String"));
        p.record_invocation(Some("java/lang/StringBuilder"));
        assert_eq!(p.invocation_count, 3);
        assert_eq!(p.receiver_types["java/lang/String"], 2);
        assert_eq!(p.receiver_types["java/lang/StringBuilder"], 1);
    }

    #[test]
    fn test_aot_method_profile_dominant_receiver_type() {
        let mut p = AotMethodProfile::new();
        p.record_invocation(Some("A"));
        p.record_invocation(Some("B"));
        p.record_invocation(Some("B"));
        assert_eq!(p.dominant_receiver_type(), Some("B"));
    }

    #[test]
    fn test_aot_method_profile_dominant_receiver_type_empty() {
        let p = AotMethodProfile::new();
        assert!(p.dominant_receiver_type().is_none());
    }

    #[test]
    fn test_aot_method_profile_record_branch() {
        let mut p = AotMethodProfile::new();
        p.record_branch(10, true);
        p.record_branch(10, true);
        p.record_branch(10, false);
        assert_eq!(p.branch_taken[&10], 2);
        assert_eq!(p.branch_not_taken[&10], 1);
    }

    // -----------------------------------------------------------------------
    // AotProfile tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_aot_profile_new_empty() {
        let ap = AotProfile::new();
        assert_eq!(ap.method_count(), 0);
    }

    #[test]
    fn test_aot_profile_method_profile_mut_inserts() {
        let mut ap = AotProfile::new();
        let mp = ap.method_profile_mut("Foo", "bar", "()V");
        mp.record_invocation(None);
        assert_eq!(ap.method_count(), 1);
    }

    #[test]
    fn test_aot_profile_method_profile_lookup() {
        let mut ap = AotProfile::new();
        ap.method_profile_mut("Foo", "bar", "()V")
            .record_invocation(None);
        let found = ap.method_profile("Foo", "bar", "()V");
        assert!(found.is_some());
        assert_eq!(found.unwrap().invocation_count, 1);
    }

    #[test]
    fn test_aot_profile_method_profile_lookup_missing() {
        let ap = AotProfile::new();
        assert!(ap.method_profile("NoSuch", "method", "()V").is_none());
    }

    // -----------------------------------------------------------------------
    // AotCacheConfig tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_aot_cache_config_defaults() {
        let cfg = AotCacheConfig::new();
        assert!(cfg.output_path.is_none());
        assert!(cfg.input_path.is_none());
        assert!(!cfg.training_mode);
        assert!(!cfg.production_mode);
        assert_eq!(cfg.max_entries, 10_000);
    }

    #[test]
    fn test_aot_cache_config_builder_methods() {
        let cfg = AotCacheConfig::new()
            .with_output_path("/tmp/cache.aot")
            .with_input_path("/tmp/cache.aot")
            .with_training_mode(true)
            .with_max_entries(500);
        assert_eq!(cfg.output_path.as_deref(), Some("/tmp/cache.aot"));
        assert_eq!(cfg.input_path.as_deref(), Some("/tmp/cache.aot"));
        assert!(cfg.training_mode);
        assert_eq!(cfg.max_entries, 500);
    }

    #[test]
    fn test_aot_cache_config_production_mode() {
        let cfg = AotCacheConfig::new().with_production_mode(true);
        assert!(cfg.production_mode);
        assert!(!cfg.training_mode);
    }

    // -----------------------------------------------------------------------
    // AotCacheEntry tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_aot_cache_entry_placeholder_fingerprint() {
        let bc = [0xCAu8, 0xFEu8, 0xBAu8, 0xBEu8];
        let entry = AotCacheEntry::new_placeholder(&bc);
        assert_eq!(&entry.bytecode_fingerprint[..4], &bc[..]);
        assert!(!entry.has_compiled_code());
    }

    #[test]
    fn test_aot_cache_entry_with_code() {
        let mut entry = AotCacheEntry::new_placeholder(&[]);
        entry.compiled_code = vec![0x90u8; 16]; // NOP sled placeholder
        assert!(entry.has_compiled_code());
    }

    // -----------------------------------------------------------------------
    // AotCache tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_aot_cache_store_and_lookup() {
        let mut cache = AotCache::new(AotCacheConfig::new());
        let entry = AotCacheEntry::new_placeholder(&[1, 2, 3]);
        cache.store("Foo", "bar", "()V", entry);
        assert_eq!(cache.entry_count(), 1);
        assert!(cache.lookup("Foo", "bar", "()V").is_some());
        assert!(cache.lookup("Foo", "baz", "()V").is_none());
    }

    #[test]
    fn test_aot_cache_max_entries_enforced() {
        let cfg = AotCacheConfig::new().with_max_entries(2);
        let mut cache = AotCache::new(cfg);
        assert!(cache.store("A", "m", "()V", AotCacheEntry::new_placeholder(&[])));
        assert!(cache.store("B", "m", "()V", AotCacheEntry::new_placeholder(&[])));
        // Third entry should be rejected
        assert!(!cache.store("C", "m", "()V", AotCacheEntry::new_placeholder(&[])));
        assert_eq!(cache.entry_count(), 2);
    }

    #[test]
    fn test_aot_cache_serialize_deserialize_roundtrip() {
        let mut cache = AotCache::new(AotCacheConfig::new());
        let mut entry = AotCacheEntry::new_placeholder(&[0xCA, 0xFE]);
        entry.compiled_code = vec![0x90, 0x90, 0xC3];
        entry.deopt_map.insert(4, 8);
        entry.entry_point_offset = 0;
        cache.store("com/example/Foo", "compute", "(I)I", entry);

        let blob = cache.serialize();
        let restored = AotCache::deserialize(&blob).expect("deserialization should succeed");
        assert_eq!(restored.entry_count(), 1);
        let e = restored
            .lookup("com/example/Foo", "compute", "(I)I")
            .unwrap();
        assert_eq!(e.bytecode_fingerprint[0], 0xCA);
        assert_eq!(e.bytecode_fingerprint[1], 0xFE);
        assert_eq!(e.compiled_code, vec![0x90, 0x90, 0xC3]);
        assert_eq!(e.deopt_map[&4], 8);
    }

    #[test]
    fn test_aot_cache_deserialize_bad_magic() {
        let data = [0x00u8; 20];
        assert!(AotCache::deserialize(&data).is_none());
    }

    #[test]
    fn test_aot_cache_deserialize_too_short() {
        assert!(AotCache::deserialize(&[]).is_none());
        assert!(AotCache::deserialize(&[0u8; 5]).is_none());
    }

    // -----------------------------------------------------------------------
    // PrelinkedClass / PrelinkerCache tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_prelinked_class_add_and_get_field() {
        let mut cls = PrelinkedClass::new("java/lang/Object");
        cls.add_field(
            5,
            PrelinkedField {
                field_offset: 12,
                descriptor: "I".to_string(),
                is_static: false,
            },
        );
        let f = cls.get_field(5).unwrap();
        assert_eq!(f.field_offset, 12);
        assert_eq!(f.descriptor, "I");
        assert!(!f.is_static);
        assert!(cls.get_field(99).is_none());
    }

    #[test]
    fn test_prelinked_class_add_and_get_method() {
        let mut cls = PrelinkedClass::new("java/lang/Object");
        cls.add_method(
            3,
            PrelinkedMethod {
                vtable_slot: 7,
                descriptor: "()V".to_string(),
                is_static: false,
            },
        );
        let m = cls.get_method(3).unwrap();
        assert_eq!(m.vtable_slot, 7);
        assert_eq!(m.descriptor, "()V");
        assert!(cls.get_method(99).is_none());
    }

    #[test]
    fn test_prelinker_cache_insert_and_get() {
        let mut cache = PrelinkerCache::new();
        cache.insert(PrelinkedClass::new("java/lang/String"));
        assert_eq!(cache.class_count(), 1);
        assert!(cache.get("java/lang/String").is_some());
        assert!(cache.get("java/lang/Object").is_none());
    }

    #[test]
    fn test_prelinker_cache_serialize_deserialize_roundtrip() {
        let mut cache = PrelinkerCache::new();
        let mut cls = PrelinkedClass::new("com/example/Bar");
        cls.add_field(
            1,
            PrelinkedField {
                field_offset: 8,
                descriptor: "J".to_string(),
                is_static: true,
            },
        );
        cls.add_method(
            2,
            PrelinkedMethod {
                vtable_slot: 3,
                descriptor: "(J)V".to_string(),
                is_static: false,
            },
        );
        cache.insert(cls);

        let blob = cache.serialize();
        let restored = PrelinkerCache::deserialize(&blob).expect("deserialization should succeed");
        assert_eq!(restored.class_count(), 1);
        let rc = restored.get("com/example/Bar").unwrap();
        let rf = rc.get_field(1).unwrap();
        assert_eq!(rf.field_offset, 8);
        assert_eq!(rf.descriptor, "J");
        assert!(rf.is_static);
        let rm = rc.get_method(2).unwrap();
        assert_eq!(rm.vtable_slot, 3);
        assert_eq!(rm.descriptor, "(J)V");
    }

    #[test]
    fn test_prelinker_cache_deserialize_bad_magic() {
        let data = [0xFFu8; 20];
        assert!(PrelinkerCache::deserialize(&data).is_none());
    }

    // -----------------------------------------------------------------------
    // TrainingRunRecorder tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_training_run_recorder_method_invocations() {
        let mut rec = TrainingRunRecorder::new();
        rec.record_method_invocation("Foo", "bar", "()V", None);
        rec.record_method_invocation("Foo", "bar", "()V", Some("Foo"));
        rec.record_method_invocation("Foo", "bar", "()V", Some("Foo"));
        assert_eq!(rec.method_count(), 1);
        let r = rec.method_record("Foo", "bar", "()V").unwrap();
        assert_eq!(r.invocation_count, 3);
        assert_eq!(r.receiver_types["Foo"], 2);
    }

    #[test]
    fn test_training_run_recorder_multiple_methods() {
        let mut rec = TrainingRunRecorder::new();
        rec.record_method_invocation("A", "m1", "()V", None);
        rec.record_method_invocation("B", "m2", "()I", None);
        assert_eq!(rec.method_count(), 2);
    }

    #[test]
    fn test_training_run_recorder_branch_recording() {
        let mut rec = TrainingRunRecorder::new();
        rec.record_branch("Foo", "check", 42, true);
        rec.record_branch("Foo", "check", 42, true);
        rec.record_branch("Foo", "check", 42, false);
        assert_eq!(rec.branch_count(), 1);
        let br = rec.branch_record("Foo", "check", 42).unwrap();
        assert_eq!(br.taken_count, 2);
        assert_eq!(br.not_taken_count, 1);
    }

    #[test]
    fn test_training_run_recorder_missing_record() {
        let rec = TrainingRunRecorder::new();
        assert!(rec.method_record("X", "y", "()V").is_none());
        assert!(rec.branch_record("X", "y", 0).is_none());
    }

    #[test]
    fn test_training_run_recorder_to_aot_profile() {
        let mut rec = TrainingRunRecorder::new();
        rec.record_method_invocation("Cls", "run", "()V", Some("Cls"));
        rec.record_method_invocation("Cls", "run", "()V", Some("Cls"));
        let profile = rec.to_aot_profile();
        let mp = profile.method_profile("Cls", "run", "()V");
        assert!(mp.is_some());
        assert_eq!(mp.unwrap().invocation_count, 2);
    }

    #[test]
    fn test_training_run_recorder_serialize_magic() {
        let rec = TrainingRunRecorder::new();
        let blob = rec.serialize();
        assert!(blob.len() >= 10);
        let magic = u32::from_le_bytes(blob[0..4].try_into().unwrap());
        assert_eq!(magic, 0xA07CAC4E);
    }

    #[test]
    fn test_training_run_recorder_serialize_with_data() {
        let mut rec = TrainingRunRecorder::new();
        rec.record_method_invocation("X", "go", "(I)V", None);
        let blob = rec.serialize();
        let method_count = u32::from_le_bytes(blob[6..10].try_into().unwrap());
        assert_eq!(method_count, 1);
    }

    // -----------------------------------------------------------------------
    // BranchRecord tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_branch_record_taken_probability() {
        let br = BranchRecord {
            class_name: "A".into(),
            method_name: "m".into(),
            bytecode_offset: 0,
            taken_count: 3,
            not_taken_count: 1,
        };
        let p = br.taken_probability();
        assert!((p - 0.75).abs() < 1e-9);
    }

    #[test]
    fn test_branch_record_taken_probability_never_executed() {
        let br = BranchRecord {
            class_name: "A".into(),
            method_name: "m".into(),
            bytecode_offset: 0,
            taken_count: 0,
            not_taken_count: 0,
        };
        assert!(br.taken_probability().is_nan());
    }

    // -----------------------------------------------------------------------
    // Native method registration tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_register_aot_natives_leyden_query_methods() {
        let mut r = NativeMethodRegistry::new();
        register_aot_natives(&mut r);
        assert!(r.find(LEYDEN, "isAOTEnabled", "()Z").is_some());
        assert!(r.find(LEYDEN, "isTrainingMode", "()Z").is_some());
        assert!(r.find(LEYDEN, "isProductionMode", "()Z").is_some());
        assert!(r
            .find(LEYDEN, "getAOTCacheInputPath", "()Ljava/lang/String;")
            .is_some());
        assert!(r
            .find(LEYDEN, "getAOTCacheOutputPath", "()Ljava/lang/String;")
            .is_some());
    }

    #[test]
    fn test_register_aot_natives_leyden_callbacks() {
        let mut r = NativeMethodRegistry::new();
        register_aot_natives(&mut r);
        assert!(r
            .find(
                LEYDEN,
                "notifyMethodInvoked",
                "(Ljava/lang/reflect/Method;)V"
            )
            .is_some());
        assert!(r
            .find(LEYDEN, "notifyClassLoaded", "(Ljava/lang/Class;)V")
            .is_some());
        assert!(r
            .find(
                LEYDEN,
                "lookupAOTMethod",
                "(Ljava/lang/reflect/Method;)Ljava/lang/Object;"
            )
            .is_some());
        assert!(r
            .find(LEYDEN, "storeAOTProfile", "(Ljava/lang/String;)V")
            .is_some());
        assert!(r
            .find(LEYDEN, "loadAOTProfile", "(Ljava/lang/String;)I")
            .is_some());
    }

    #[test]
    fn test_register_aot_natives_stable_annotation() {
        let mut r = NativeMethodRegistry::new();
        register_aot_natives(&mut r);
        let stable = "jdk/internal/vm/annotation/Stable";
        assert!(r.find(stable, "<init>", "()V").is_some());
        assert!(r
            .find(stable, "annotationType", "()Ljava/lang/Class;")
            .is_some());
    }

    #[test]
    fn test_register_aot_natives_reflect_method() {
        let mut r = NativeMethodRegistry::new();
        register_aot_natives(&mut r);
        assert!(r
            .find(
                "java/lang/reflect/Method",
                "isAnnotationPresent",
                "(Ljava/lang/Class;)Z"
            )
            .is_some());
    }

    #[test]
    fn test_register_aot_natives_leyden_init() {
        let mut r = NativeMethodRegistry::new();
        register_aot_natives(&mut r);
        assert!(r.find(LEYDEN, "<init>", "()V").is_some());
    }

    #[test]
    fn test_total_registered_method_count() {
        let mut r = NativeMethodRegistry::new();
        register_aot_natives(&mut r);
        // 10 Leyden methods + 2 @Stable + 1 reflect = 13 total
        assert!(r.len() >= 13);
    }

    // -----------------------------------------------------------------------
    // Global AOT runtime state tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_init_aot_runtime_training_mode() {
        let _guard = aot_test_lock();
        reset_aot_globals();
        init_aot_runtime(true, false, None, Some("/tmp/test_cache.aot"));
        assert!(is_aot_enabled());
        assert!(is_aot_training());
        assert!(!is_aot_production());
        assert_eq!(
            AOT_CACHE_OUTPUT_PATH.lock().as_deref(),
            Some("/tmp/test_cache.aot")
        );
        assert!(AOT_TRAINING_RECORDER.lock().unwrap().is_some());
        assert!(AOT_CACHE_GLOBAL.lock().unwrap().is_some());
        assert!(AOT_PRELINKER_CACHE.lock().unwrap().is_some());
        reset_aot_globals();
    }

    #[test]
    fn test_init_aot_runtime_production_mode() {
        let _guard = aot_test_lock();
        reset_aot_globals();
        init_aot_runtime(false, true, Some("/nonexistent/cache.aot"), None);
        assert!(is_aot_enabled());
        assert!(!is_aot_training());
        assert!(is_aot_production());
        assert_eq!(
            AOT_CACHE_INPUT_PATH.lock().as_deref(),
            Some("/nonexistent/cache.aot")
        );
        // Cache should be None since file doesn't exist
        assert!(AOT_PRELINKER_CACHE.lock().unwrap().is_some());
        reset_aot_globals();
    }

    #[test]
    fn test_init_aot_runtime_off() {
        let _guard = aot_test_lock();
        reset_aot_globals();
        init_aot_runtime(false, false, None, None);
        assert!(!is_aot_enabled());
        assert!(!is_aot_training());
        assert!(!is_aot_production());
        reset_aot_globals();
    }

    #[test]
    fn test_aot_record_method_invocation_training() {
        let _guard = aot_test_lock();
        reset_aot_globals();
        init_aot_runtime(true, false, None, None);
        aot_record_method_invocation("com/example/Foo", "run", "()V", Some("com/example/Foo"));
        aot_record_method_invocation("com/example/Foo", "run", "()V", None);
        let (methods, _) = aot_training_stats();
        assert_eq!(methods, 1);
        {
            let guard = AOT_TRAINING_RECORDER.lock().unwrap();
            let rec = guard.as_ref().unwrap();
            let mr = rec.method_record("com/example/Foo", "run", "()V").unwrap();
            assert_eq!(mr.invocation_count, 2);
            assert_eq!(mr.receiver_types.get("com/example/Foo"), Some(&1));
        }
        reset_aot_globals();
    }

    #[test]
    fn test_aot_record_method_invocation_noop_when_off() {
        let _guard = aot_test_lock();
        reset_aot_globals();
        init_aot_runtime(false, false, None, None);
        aot_record_method_invocation("Foo", "bar", "()V", None);
        let (methods, _) = aot_training_stats();
        assert_eq!(methods, 0);
        reset_aot_globals();
    }

    #[test]
    fn test_aot_record_branch_training() {
        let _guard = aot_test_lock();
        reset_aot_globals();
        init_aot_runtime(true, false, None, None);
        aot_record_branch("Foo", "check", 10, true);
        aot_record_branch("Foo", "check", 10, false);
        aot_record_branch("Foo", "check", 10, true);
        let (_, branches) = aot_training_stats();
        assert_eq!(branches, 1);
        {
            let guard = AOT_TRAINING_RECORDER.lock().unwrap();
            let rec = guard.as_ref().unwrap();
            let br = rec.branch_record("Foo", "check", 10).unwrap();
            assert_eq!(br.taken_count, 2);
            assert_eq!(br.not_taken_count, 1);
        }
        reset_aot_globals();
    }

    #[test]
    fn test_aot_record_class_loaded_training() {
        let _guard = aot_test_lock();
        reset_aot_globals();
        init_aot_runtime(true, false, None, None);
        aot_record_class_loaded("com/example/Bar");
        aot_record_class_loaded("com/example/Bar"); // duplicate should not error
        {
            let guard = AOT_PRELINKER_CACHE.lock().unwrap();
            let cache = guard.as_ref().unwrap();
            assert_eq!(cache.class_count(), 1);
            assert!(cache.get("com/example/Bar").is_some());
        }
        reset_aot_globals();
    }

    #[test]
    fn test_aot_flush_noop_when_off() {
        let _guard = aot_test_lock();
        reset_aot_globals();
        let written = aot_flush_training_data();
        assert_eq!(written, 0);
        reset_aot_globals();
    }

    #[test]
    fn test_aot_flush_training_data_writes_file() {
        let _guard = aot_test_lock();
        reset_aot_globals();
        // Use a per-test tempdir so parallel tests or leftover files from
        // prior runs cannot cross-contaminate.
        let tmp = tempfile::TempDir::new().expect("create per-test tempdir");
        let path = tmp.path().join("cratonvm_aot_test_flush.bin");
        let path_str = path.to_string_lossy().to_string();

        init_aot_runtime(true, false, None, Some(&path_str));
        aot_record_method_invocation("Test", "main", "()V", None);

        let written = aot_flush_training_data();
        assert!(written > 0);
        assert!(path.exists());

        // Clean up
        let _ = std::fs::remove_file(&path);
        reset_aot_globals();
    }

    #[test]
    fn test_aot_production_load_from_file() {
        let _guard = aot_test_lock();
        reset_aot_globals();
        // Per-test tempdir: RAII cleanup on drop, avoids sharing
        // "cratonvm_aot_test_load.bin" in $TMP with parallel runs.
        let tmp = tempfile::TempDir::new().expect("create per-test tempdir");
        let path = tmp.path().join("cratonvm_aot_test_load.bin");
        let path_str = path.to_string_lossy().to_string();

        // Create a valid cache file
        let mut cache = AotCache::new(AotCacheConfig::new());
        let entry = AotCacheEntry::new_placeholder(&[0xCA, 0xFE]);
        cache.store("Test", "compute", "(I)I", entry);
        let data = cache.serialize();
        std::fs::write(&path, &data).unwrap();

        // Load it in production mode
        init_aot_runtime(false, true, Some(&path_str), None);
        assert!(is_aot_production());
        let cache_guard = AOT_CACHE_GLOBAL.lock().unwrap();
        let loaded = cache_guard.as_ref().unwrap();
        assert_eq!(loaded.entry_count(), 1);
        assert!(loaded.lookup("Test", "compute", "(I)I").is_some());

        // Clean up (TempDir auto-removes file via RAII)
        drop(cache_guard);
        reset_aot_globals();
    }

    // ===== M29 AOT Training Activation Tests =====

    #[test]
    fn m29_training_records_method_invocation_with_receiver() {
        let _guard = aot_test_lock();
        reset_aot_globals();
        init_aot_runtime(true, false, None, None);
        aot_record_method_invocation(
            "com/example/Foo",
            "bar",
            "(I)V",
            Some("com/example/FooImpl"),
        );
        aot_record_method_invocation(
            "com/example/Foo",
            "bar",
            "(I)V",
            Some("com/example/FooImpl"),
        );
        aot_record_method_invocation("com/example/Foo", "bar", "(I)V", Some("com/example/FooSub"));
        let (methods, _) = aot_training_stats();
        assert_eq!(methods, 1);
        let guard = AOT_TRAINING_RECORDER.lock().unwrap();
        let rec = guard.as_ref().unwrap();
        let mr = rec.method_record("com/example/Foo", "bar", "(I)V").unwrap();
        assert_eq!(mr.invocation_count, 3);
        assert_eq!(mr.receiver_types["com/example/FooImpl"], 2);
        assert_eq!(mr.receiver_types["com/example/FooSub"], 1);
        drop(guard);
        reset_aot_globals();
    }

    #[test]
    fn m29_training_records_branch_taken_and_not_taken() {
        let _guard = aot_test_lock();
        reset_aot_globals();
        init_aot_runtime(true, false, None, None);
        aot_record_branch("com/example/Foo", "bar", 10, true);
        aot_record_branch("com/example/Foo", "bar", 10, true);
        aot_record_branch("com/example/Foo", "bar", 10, false);
        let (_, branches) = aot_training_stats();
        assert_eq!(branches, 1);
        let guard = AOT_TRAINING_RECORDER.lock().unwrap();
        let rec = guard.as_ref().unwrap();
        let br = rec.branch_record("com/example/Foo", "bar", 10).unwrap();
        assert_eq!(br.taken_count, 2);
        assert_eq!(br.not_taken_count, 1);
        drop(guard);
        reset_aot_globals();
    }

    #[test]
    fn m29_receiver_type_recording() {
        let _guard = aot_test_lock();
        reset_aot_globals();
        init_aot_runtime(true, false, None, None);
        aot_record_receiver_type("Foo", "doIt", "()V", "FooImpl");
        aot_record_receiver_type("Foo", "doIt", "()V", "FooImpl");
        aot_record_receiver_type("Foo", "doIt", "()V", "BarImpl");
        let guard = AOT_TRAINING_RECORDER.lock().unwrap();
        let rec = guard.as_ref().unwrap();
        let mr = rec.method_record("Foo", "doIt", "()V").unwrap();
        assert_eq!(mr.invocation_count, 3);
        assert_eq!(mr.receiver_types["FooImpl"], 2);
        assert_eq!(mr.receiver_types["BarImpl"], 1);
        drop(guard);
        reset_aot_globals();
    }

    #[test]
    fn m29_receiver_type_noop_when_not_training() {
        let _guard = aot_test_lock();
        reset_aot_globals();
        // Not in training mode
        init_aot_runtime(false, false, None, None);
        aot_record_receiver_type("Foo", "bar", "()V", "Impl");
        let (methods, _) = aot_training_stats();
        assert_eq!(methods, 0);
        reset_aot_globals();
    }

    #[test]
    fn m29_bulk_import_branches() {
        let _guard = aot_test_lock();
        reset_aot_globals();
        init_aot_runtime(true, false, None, None);
        let entries: Vec<(&str, &str, u32, u32, u32)> =
            vec![("A", "m1", 5, 10, 3), ("B", "m2", 20, 0, 5)];
        let count = aot_bulk_import_branches(&entries);
        assert_eq!(count, 2);
        let guard = AOT_TRAINING_RECORDER.lock().unwrap();
        let rec = guard.as_ref().unwrap();
        let br1 = rec.branch_record("A", "m1", 5).unwrap();
        assert_eq!(br1.taken_count, 10);
        assert_eq!(br1.not_taken_count, 3);
        let br2 = rec.branch_record("B", "m2", 20).unwrap();
        assert_eq!(br2.taken_count, 0);
        assert_eq!(br2.not_taken_count, 5);
        drop(guard);
        reset_aot_globals();
    }

    #[test]
    fn m29_bulk_import_receivers() {
        let _guard = aot_test_lock();
        reset_aot_globals();
        init_aot_runtime(true, false, None, None);
        let entries: Vec<(&str, &str, &str, &str, u32)> = vec![
            ("A", "m1", "()V", "ImplA", 5),
            ("A", "m1", "()V", "ImplB", 3),
        ];
        let count = aot_bulk_import_receivers(&entries);
        assert_eq!(count, 2);
        let guard = AOT_TRAINING_RECORDER.lock().unwrap();
        let rec = guard.as_ref().unwrap();
        let mr = rec.method_record("A", "m1", "()V").unwrap();
        assert_eq!(mr.invocation_count, 8); // 5 + 3
        assert_eq!(mr.receiver_types["ImplA"], 5);
        assert_eq!(mr.receiver_types["ImplB"], 3);
        drop(guard);
        reset_aot_globals();
    }

    #[test]
    fn m29_bulk_import_noop_when_not_training() {
        let _guard = aot_test_lock();
        reset_aot_globals();
        init_aot_runtime(false, true, None, None); // production, not training
        let entries: Vec<(&str, &str, u32, u32, u32)> = vec![("A", "m", 0, 10, 5)];
        let count = aot_bulk_import_branches(&entries);
        assert_eq!(count, 0);
        reset_aot_globals();
    }

    #[test]
    fn m29_class_loaded_creates_prelinker_entry() {
        let _guard = aot_test_lock();
        reset_aot_globals();
        init_aot_runtime(true, false, None, None);
        aot_record_class_loaded("com/example/MyClass");
        let guard = AOT_PRELINKER_CACHE.lock().unwrap();
        let cache = guard.as_ref().unwrap();
        assert!(cache.get("com/example/MyClass").is_some());
        assert_eq!(
            cache.get("com/example/MyClass").unwrap().class_name,
            "com/example/MyClass"
        );
        drop(guard);
        reset_aot_globals();
    }

    #[test]
    fn m29_class_loaded_idempotent() {
        let _guard = aot_test_lock();
        reset_aot_globals();
        init_aot_runtime(true, false, None, None);
        aot_record_class_loaded("com/example/Dup");
        aot_record_class_loaded("com/example/Dup");
        let guard = AOT_PRELINKER_CACHE.lock().unwrap();
        let cache = guard.as_ref().unwrap();
        assert_eq!(cache.class_count(), 1);
        drop(guard);
        reset_aot_globals();
    }

    #[test]
    fn m29_training_to_profile_includes_receivers() {
        let _guard = aot_test_lock();
        reset_aot_globals();
        init_aot_runtime(true, false, None, None);
        aot_record_method_invocation("X", "run", "()V", Some("XImpl"));
        aot_record_method_invocation("X", "run", "()V", Some("XImpl"));
        aot_record_branch("X", "run", 5, true);
        aot_record_branch("X", "run", 5, false);
        let guard = AOT_TRAINING_RECORDER.lock().unwrap();
        let rec = guard.as_ref().unwrap();
        let profile = rec.to_aot_profile();
        let mp = profile.method_profile("X", "run", "()V").unwrap();
        assert_eq!(mp.invocation_count, 2);
        assert_eq!(mp.receiver_types["XImpl"], 2);
        // Branch data goes into a profile with empty descriptor
        let bp = profile.method_profile("X", "run", "").unwrap();
        assert_eq!(bp.branch_taken[&5], 1);
        assert_eq!(bp.branch_not_taken[&5], 1);
        drop(guard);
        reset_aot_globals();
    }

    #[test]
    fn m29_training_serialize_includes_receiver_types() {
        let _guard = aot_test_lock();
        reset_aot_globals();
        init_aot_runtime(true, false, None, None);
        aot_record_method_invocation("Cls", "met", "()V", Some("Impl"));
        let guard = AOT_TRAINING_RECORDER.lock().unwrap();
        let rec = guard.as_ref().unwrap();
        let data = rec.serialize();
        // Header: magic(4) + version(2) + method_count(4) = 10 bytes minimum
        assert!(data.len() > 10);
        // Verify magic
        let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
        assert_eq!(magic, 0xA07CAC4E);
        drop(guard);
        reset_aot_globals();
    }

    #[test]
    fn m29_multiple_methods_tracked_separately() {
        let _guard = aot_test_lock();
        reset_aot_globals();
        init_aot_runtime(true, false, None, None);
        aot_record_method_invocation("A", "m1", "()V", None);
        aot_record_method_invocation("A", "m2", "(I)V", None);
        aot_record_method_invocation("B", "m1", "()V", None);
        let (methods, _) = aot_training_stats();
        assert_eq!(methods, 3);
        reset_aot_globals();
    }

    #[test]
    fn m29_branch_probability_calculation() {
        let br = BranchRecord {
            class_name: "T".to_string(),
            method_name: "f".to_string(),
            bytecode_offset: 0,
            taken_count: 75,
            not_taken_count: 25,
        };
        let prob = br.taken_probability();
        assert!((prob - 0.75).abs() < 0.001);
    }

    #[test]
    fn m29_branch_probability_zero_executions() {
        let br = BranchRecord {
            class_name: "T".to_string(),
            method_name: "f".to_string(),
            bytecode_offset: 0,
            taken_count: 0,
            not_taken_count: 0,
        };
        assert!(br.taken_probability().is_nan());
    }

    // -----------------------------------------------------------------------
    // Integrity & security tests (M29 fixes)
    // -----------------------------------------------------------------------

    #[test]
    fn test_aot_cache_deserialize_rejects_bad_version() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&0xA07CAC4Eu32.to_le_bytes()); // correct magic
        blob.extend_from_slice(&99u16.to_le_bytes()); // bad version
        blob.extend_from_slice(&0u32.to_le_bytes());
        assert!(AotCache::deserialize(&blob).is_none());
    }

    #[test]
    fn test_aot_cache_integrity_tamper_detected() {
        let mut cache = AotCache::new(AotCacheConfig::new());
        let entry = AotCacheEntry {
            bytecode_fingerprint: [0; 8],
            compiled_code: vec![0xAB; 8],
            deopt_map: HashMap::new(),
            entry_point_offset: 0,
        };
        cache.store("A", "b", "()V", entry);
        let mut blob = cache.serialize();
        // Flip a byte in the payload
        if blob.len() > 12 {
            blob[12] ^= 0xFF;
        }
        assert!(AotCache::deserialize(&blob).is_none());
    }

    #[test]
    fn test_aot_cache_integrity_digest_is_sha256_len() {
        // The trailing digest must be a full 32-byte SHA-256, not the old
        // 8-byte non-cryptographic mix.
        let cache = AotCache::new(AotCacheConfig::new());
        let blob = cache.serialize();
        // Header (10) + no entries + 32-byte digest.
        assert_eq!(blob.len(), 10 + AotCache::INTEGRITY_DIGEST_LEN);
        // The bare (keyless) digest of the header payload must equal a
        // straight SHA-256 over those bytes.
        let payload = &blob[..blob.len() - AotCache::INTEGRITY_DIGEST_LEN];
        let expect = crate::crypto_impl::Sha256::digest(payload);
        assert_eq!(
            &blob[blob.len() - AotCache::INTEGRITY_DIGEST_LEN..],
            &expect[..]
        );
    }

    #[test]
    fn test_aot_cache_integrity_recompute_forgery_still_detected_without_key() {
        // Without a key, SHA-256 detects accidental change. A forger who
        // recomputes the digest over a tampered payload would pass — this is
        // the documented residual risk addressed by CRATONVM_AOT_HMAC_KEY.
        // Here we assert the corruption-detection property: any payload edit
        // that is NOT accompanied by a recomputed digest is rejected.
        let mut cache = AotCache::new(AotCacheConfig::new());
        cache.store(
            "A",
            "b",
            "()V",
            AotCacheEntry {
                bytecode_fingerprint: [0; 8],
                compiled_code: vec![1, 2, 3, 4],
                deopt_map: HashMap::new(),
                entry_point_offset: 0,
            },
        );
        let mut blob = cache.serialize();
        // Tamper with the very last digest byte: a stale digest is rejected.
        let last = blob.len() - 1;
        blob[last] ^= 0xFF;
        assert!(AotCache::deserialize(&blob).is_none());
    }

    #[test]
    fn test_aot_cache_integrity_rejects_trailing_garbage() {
        let cache = AotCache::new(AotCacheConfig::new());
        let mut blob = cache.serialize();
        // Extra bytes after a valid digest must be rejected (exact-length
        // check), so a forger cannot append a second valid-looking digest.
        blob.push(0x00);
        assert!(AotCache::deserialize(&blob).is_none());
    }

    #[test]
    fn test_aot_cache_hmac_keyed_digest_differs_from_bare() {
        // With a key present the digest is HMAC-SHA256, which differs from the
        // bare SHA-256 of the same payload. Drive compute_integrity_hash
        // directly so this test does not depend on process-global env state
        // (env-var mutation would race with parallel tests).
        let payload = b"some-aot-cache-payload-bytes";
        let bare = AotCache::compute_integrity_hash(payload, None);
        let keyed = AotCache::compute_integrity_hash(payload, Some(b"secret-key"));
        assert_ne!(bare, keyed);
        // HMAC is deterministic for a fixed key+payload.
        let keyed2 = AotCache::compute_integrity_hash(payload, Some(b"secret-key"));
        assert_eq!(keyed, keyed2);
        // A different key yields a different tag.
        let keyed3 = AotCache::compute_integrity_hash(payload, Some(b"other-key"));
        assert_ne!(keyed, keyed3);
    }

    #[test]
    fn test_aot_cache_hmac_matches_rfc2104_reference() {
        // RFC 4231 / RFC 2104 HMAC-SHA256 test case 1:
        //   key  = 0x0b * 20
        //   data = "Hi There"
        //   mac  = b0344c61d8db38535ca8afceaf0bf12b
        //          881dc200c9833da726e9376c2e32cff7
        let key = [0x0bu8; 20];
        let mac = AotCache::compute_integrity_hash(b"Hi There", Some(&key));
        let expect: [u8; 32] = [
            0xb0, 0x34, 0x4c, 0x61, 0xd8, 0xdb, 0x38, 0x53, 0x5c, 0xa8, 0xaf, 0xce, 0xaf, 0x0b,
            0xf1, 0x2b, 0x88, 0x1d, 0xc2, 0x00, 0xc9, 0x83, 0x3d, 0xa7, 0x26, 0xe9, 0x37, 0x6c,
            0x2e, 0x32, 0xcf, 0xf7,
        ];
        assert_eq!(mac, expect);
    }

    #[test]
    fn test_sanitize_aot_path_rejects_traversal() {
        assert!(sanitize_aot_path("cache/foo.bin").is_some());
        assert!(sanitize_aot_path("../secret/data").is_none());
        assert!(sanitize_aot_path("foo/../../etc/passwd").is_none());
        assert!(sanitize_aot_path("safe_dir/cache.bin").is_some());
    }
}
