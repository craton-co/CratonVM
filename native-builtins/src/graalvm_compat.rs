// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! GraalVM Native Image Compatibility Layer (Phase 17.2).
//!
//! Provides metadata structures for GraalVM native-image configuration
//! (reflection, resources, JNI, proxies, serialization) and compatibility
//! stubs so that code targeting GraalVM's Substrate VM APIs can run on
//! CratonVM without native-image.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ObjectRef, Value};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};

/// Escape a string for safe JSON embedding. Handles `"`, `\`, and control chars.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

// ===========================================================================
// Method / Field configuration (shared by Reflection & JNI)
// ===========================================================================

#[derive(Debug, Clone, PartialEq)]
pub struct MethodConfig {
    pub name: String,
    pub parameter_types: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldConfig {
    pub name: String,
    pub allow_write: bool,
}

// ===========================================================================
// Reflection Configuration
// ===========================================================================

#[derive(Debug, Clone, PartialEq)]
pub struct ReflectionEntry {
    pub class_name: String,
    pub all_public_methods: bool,
    pub all_public_fields: bool,
    pub all_declared_methods: bool,
    pub all_declared_fields: bool,
    pub all_public_constructors: bool,
    pub all_declared_constructors: bool,
    pub methods: Vec<MethodConfig>,
    pub fields: Vec<FieldConfig>,
}

impl ReflectionEntry {
    fn new(class_name: &str) -> Self {
        Self {
            class_name: class_name.to_string(),
            all_public_methods: false,
            all_public_fields: false,
            all_declared_methods: false,
            all_declared_fields: false,
            all_public_constructors: false,
            all_declared_constructors: false,
            methods: Vec::new(),
            fields: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ReflectionConfig {
    pub entries: Vec<ReflectionEntry>,
}

impl ReflectionConfig {
    pub fn new() -> Self {
        Self::default()
    }

    fn find_or_create_entry(&mut self, class_name: &str) -> &mut ReflectionEntry {
        let pos = self.entries.iter().position(|e| e.class_name == class_name);
        match pos {
            Some(i) => &mut self.entries[i],
            None => {
                self.entries.push(ReflectionEntry::new(class_name));
                // SAFETY: just pushed above, entries is non-empty
                self.entries
                    .last_mut()
                    .expect("entries non-empty after push")
            }
        }
    }

    pub fn add_class(&mut self, class_name: &str, all_public: bool, all_declared: bool) {
        let entry = self.find_or_create_entry(class_name);
        if all_public {
            entry.all_public_methods = true;
            entry.all_public_fields = true;
            entry.all_public_constructors = true;
        }
        if all_declared {
            entry.all_declared_methods = true;
            entry.all_declared_fields = true;
            entry.all_declared_constructors = true;
        }
    }

    pub fn add_method(&mut self, class_name: &str, method_name: &str, param_types: &[&str]) {
        let entry = self.find_or_create_entry(class_name);
        let mc = MethodConfig {
            name: method_name.to_string(),
            parameter_types: param_types.iter().map(|s| s.to_string()).collect(),
        };
        if !entry.methods.contains(&mc) {
            entry.methods.push(mc);
        }
    }

    pub fn add_field(&mut self, class_name: &str, field_name: &str, allow_write: bool) {
        let entry = self.find_or_create_entry(class_name);
        // Update existing or add new
        if let Some(existing) = entry.fields.iter_mut().find(|f| f.name == field_name) {
            existing.allow_write = existing.allow_write || allow_write;
        } else {
            entry.fields.push(FieldConfig {
                name: field_name.to_string(),
                allow_write,
            });
        }
    }

    pub fn merge(&mut self, other: &ReflectionConfig) {
        for other_entry in &other.entries {
            let entry = self.find_or_create_entry(&other_entry.class_name);
            entry.all_public_methods = entry.all_public_methods || other_entry.all_public_methods;
            entry.all_public_fields = entry.all_public_fields || other_entry.all_public_fields;
            entry.all_declared_methods =
                entry.all_declared_methods || other_entry.all_declared_methods;
            entry.all_declared_fields =
                entry.all_declared_fields || other_entry.all_declared_fields;
            entry.all_public_constructors =
                entry.all_public_constructors || other_entry.all_public_constructors;
            entry.all_declared_constructors =
                entry.all_declared_constructors || other_entry.all_declared_constructors;
            for m in &other_entry.methods {
                if !entry.methods.contains(m) {
                    entry.methods.push(m.clone());
                }
            }
            for f in &other_entry.fields {
                if let Some(existing) = entry.fields.iter_mut().find(|ef| ef.name == f.name) {
                    existing.allow_write = existing.allow_write || f.allow_write;
                } else {
                    entry.fields.push(f.clone());
                }
            }
        }
    }

    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    pub fn to_json(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for entry in &self.entries {
            let mut obj_parts: Vec<String> = Vec::new();
            obj_parts.push(format!("\"name\":\"{}\"", json_escape(&entry.class_name)));
            if entry.all_public_methods {
                obj_parts.push("\"allPublicMethods\":true".to_string());
            }
            if entry.all_public_fields {
                obj_parts.push("\"allPublicFields\":true".to_string());
            }
            if entry.all_declared_methods {
                obj_parts.push("\"allDeclaredMethods\":true".to_string());
            }
            if entry.all_declared_fields {
                obj_parts.push("\"allDeclaredFields\":true".to_string());
            }
            if entry.all_public_constructors {
                obj_parts.push("\"allPublicConstructors\":true".to_string());
            }
            if entry.all_declared_constructors {
                obj_parts.push("\"allDeclaredConstructors\":true".to_string());
            }
            if !entry.methods.is_empty() {
                let methods_json: Vec<String> = entry
                    .methods
                    .iter()
                    .map(|m| {
                        let params: Vec<String> = m
                            .parameter_types
                            .iter()
                            .map(|p| format!("\"{}\"", json_escape(p)))
                            .collect();
                        format!(
                            "{{\"name\":\"{}\",\"parameterTypes\":[{}]}}",
                            json_escape(&m.name),
                            params.join(",")
                        )
                    })
                    .collect();
                obj_parts.push(format!("\"methods\":[{}]", methods_json.join(",")));
            }
            if !entry.fields.is_empty() {
                let fields_json: Vec<String> = entry
                    .fields
                    .iter()
                    .map(|f| {
                        if f.allow_write {
                            format!(
                                "{{\"name\":\"{}\",\"allowWrite\":true}}",
                                json_escape(&f.name)
                            )
                        } else {
                            format!("{{\"name\":\"{}\"}}", json_escape(&f.name))
                        }
                    })
                    .collect();
                obj_parts.push(format!("\"fields\":[{}]", fields_json.join(",")));
            }
            parts.push(format!("{{{}}}", obj_parts.join(",")));
        }
        format!("[{}]", parts.join(","))
    }

    pub fn from_json(json: &str) -> Result<Self, String> {
        // Simplified JSON parser for reflect-config.json
        let trimmed = json.trim();
        if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
            return Err("Expected JSON array".to_string());
        }
        let inner = &trimmed[1..trimmed.len() - 1].trim();
        if inner.is_empty() {
            return Ok(Self::default());
        }

        let mut config = ReflectionConfig::new();
        // Split top-level objects by finding matching braces
        let objects = split_top_level_objects(inner)?;
        for obj_str in objects {
            let entry = parse_reflection_entry(&obj_str)?;
            config.entries.push(entry);
        }
        Ok(config)
    }
}

/// Split a string into top-level JSON objects (brace-delimited).
/// Correctly handles braces inside string literals and escaped quotes.
fn split_top_level_objects(s: &str) -> Result<Vec<String>, String> {
    let mut objects = Vec::new();
    let mut depth = 0i32;
    let mut start = None;
    let mut in_string = false;
    let mut escape_next = false;
    for (i, ch) in s.char_indices() {
        if escape_next {
            escape_next = false;
            continue;
        }
        if ch == '\\' && in_string {
            escape_next = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        match ch {
            '{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s_idx) = start {
                        objects.push(s[s_idx..=i].to_string());
                    }
                    start = None;
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err("Unbalanced braces in JSON".to_string());
    }
    Ok(objects)
}

/// Simplified parser for a single reflection entry JSON object.
fn parse_reflection_entry(obj: &str) -> Result<ReflectionEntry, String> {
    let name =
        extract_string_value(obj, "name").ok_or_else(|| "Missing 'name' field".to_string())?;
    let mut entry = ReflectionEntry::new(&name);
    entry.all_public_methods = extract_bool_value(obj, "allPublicMethods");
    entry.all_public_fields = extract_bool_value(obj, "allPublicFields");
    entry.all_declared_methods = extract_bool_value(obj, "allDeclaredMethods");
    entry.all_declared_fields = extract_bool_value(obj, "allDeclaredFields");
    entry.all_public_constructors = extract_bool_value(obj, "allPublicConstructors");
    entry.all_declared_constructors = extract_bool_value(obj, "allDeclaredConstructors");
    Ok(entry)
}

fn extract_string_value(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\":\"", key);
    let start = json.find(&pattern)? + pattern.len();
    // Find the closing quote, skipping escaped quotes.
    let bytes = json[start..].as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2; // skip escaped character
            continue;
        }
        if bytes[i] == b'"' {
            return Some(json[start..start + i].to_string());
        }
        i += 1;
    }
    None
}

fn extract_bool_value(json: &str, key: &str) -> bool {
    let pattern = format!("\"{}\":true", key);
    json.contains(&pattern)
}

// ===========================================================================
// Resource Configuration
// ===========================================================================

#[derive(Debug, Clone, PartialEq)]
pub struct ResourcePattern {
    pub pattern: String,
}

impl ResourcePattern {
    pub fn new(pattern: &str) -> Self {
        Self {
            pattern: pattern.to_string(),
        }
    }

    /// Simple glob matching supporting `*` (single segment) and `**` (any depth).
    pub fn matches(&self, path: &str) -> bool {
        glob_match(&self.pattern, path)
    }
}

/// Simple glob matcher: `*` matches anything except `/`, `**` matches everything.
/// Uses an iterative approach to avoid exponential backtracking (ReDoS).
fn glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    let (plen, tlen) = (pat.len(), txt.len());

    let mut pi = 0usize;
    let mut ti = 0usize;
    // Backtrack positions for '*' matching.
    let mut star_pi: Option<usize> = None;
    let mut star_ti = 0usize;
    // Backtrack positions for '**' matching.
    let mut dstar_pi: Option<usize> = None;
    let mut dstar_ti = 0usize;

    while ti < tlen || pi < plen {
        if pi < plen {
            // Check for **
            if pi + 1 < plen && pat[pi] == '*' && pat[pi + 1] == '*' {
                dstar_pi = Some(pi);
                dstar_ti = ti;
                // Skip ** and optional trailing /
                pi += 2;
                if pi < plen && pat[pi] == '/' {
                    pi += 1;
                }
                // Reset single-star state since ** takes precedence
                star_pi = None;
                continue;
            }
            // Check for *
            if pat[pi] == '*' {
                star_pi = Some(pi);
                star_ti = ti;
                pi += 1;
                continue;
            }
            // Literal match
            if ti < tlen && pat[pi] == txt[ti] {
                pi += 1;
                ti += 1;
                continue;
            }
        }
        // Mismatch — try to extend the most recent wildcard.
        // Try single-star backtrack first (can't cross /).
        if let Some(sp) = star_pi {
            star_ti += 1;
            if star_ti <= tlen && txt[star_ti - 1] != '/' {
                pi = sp + 1;
                ti = star_ti;
                continue;
            }
            // Single star can't help, clear it.
            #[allow(unused_assignments)]
            {
                star_pi = None;
            }
        }
        // Try double-star backtrack (can cross anything).
        if let Some(dp) = dstar_pi {
            dstar_ti += 1;
            if dstar_ti <= tlen {
                pi = dp + 2;
                if pi < plen && pat[pi] == '/' {
                    pi += 1;
                }
                ti = dstar_ti;
                star_pi = None;
                continue;
            }
        }
        return false;
    }
    true
}

#[derive(Debug, Clone, Default)]
pub struct ResourceConfig {
    pub includes: Vec<ResourcePattern>,
    pub excludes: Vec<ResourcePattern>,
    pub bundles: Vec<String>,
}

impl ResourceConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_include(&mut self, pattern: &str) {
        self.includes.push(ResourcePattern::new(pattern));
    }

    pub fn add_exclude(&mut self, pattern: &str) {
        self.excludes.push(ResourcePattern::new(pattern));
    }

    pub fn add_bundle(&mut self, bundle_name: &str) {
        if !self.bundles.iter().any(|b| b == bundle_name) {
            self.bundles.push(bundle_name.to_string());
        }
    }

    /// Check if a resource path should be included (matches an include and
    /// does not match any exclude).
    pub fn matches(&self, resource_path: &str) -> bool {
        let included = self.includes.iter().any(|p| p.matches(resource_path));
        if !included {
            return false;
        }
        let excluded = self.excludes.iter().any(|p| p.matches(resource_path));
        !excluded
    }

    pub fn to_json(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.includes.is_empty() {
            let inc: Vec<String> = self
                .includes
                .iter()
                .map(|p| format!("{{\"pattern\":\"{}\"}}", json_escape(&p.pattern)))
                .collect();
            parts.push(format!("\"includes\":[{}]", inc.join(",")));
        }
        if !self.excludes.is_empty() {
            let exc: Vec<String> = self
                .excludes
                .iter()
                .map(|p| format!("{{\"pattern\":\"{}\"}}", json_escape(&p.pattern)))
                .collect();
            parts.push(format!("\"excludes\":[{}]", exc.join(",")));
        }
        if !self.bundles.is_empty() {
            let bnd: Vec<String> = self
                .bundles
                .iter()
                .map(|b| format!("\"{}\"", json_escape(b)))
                .collect();
            parts.push(format!("\"bundles\":[{}]", bnd.join(",")));
        }
        format!("{{{}}}", parts.join(","))
    }
}

// ===========================================================================
// JNI Configuration
// ===========================================================================

#[derive(Debug, Clone, PartialEq)]
pub struct JniEntry {
    pub class_name: String,
    pub methods: Vec<MethodConfig>,
    pub fields: Vec<String>,
}

impl JniEntry {
    fn new(class_name: &str) -> Self {
        Self {
            class_name: class_name.to_string(),
            methods: Vec::new(),
            fields: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct JniConfig {
    pub entries: Vec<JniEntry>,
}

impl JniConfig {
    pub fn new() -> Self {
        Self::default()
    }

    fn find_or_create_entry(&mut self, class_name: &str) -> &mut JniEntry {
        let pos = self.entries.iter().position(|e| e.class_name == class_name);
        match pos {
            Some(i) => &mut self.entries[i],
            None => {
                self.entries.push(JniEntry::new(class_name));
                // SAFETY: just pushed above, entries is non-empty
                self.entries
                    .last_mut()
                    .expect("entries non-empty after push")
            }
        }
    }

    pub fn add_class(&mut self, class_name: &str) {
        self.find_or_create_entry(class_name);
    }

    pub fn add_method(&mut self, class_name: &str, method_name: &str, param_types: &[&str]) {
        let entry = self.find_or_create_entry(class_name);
        let mc = MethodConfig {
            name: method_name.to_string(),
            parameter_types: param_types.iter().map(|s| s.to_string()).collect(),
        };
        if !entry.methods.contains(&mc) {
            entry.methods.push(mc);
        }
    }

    pub fn add_field(&mut self, class_name: &str, field_name: &str) {
        let entry = self.find_or_create_entry(class_name);
        let name = field_name.to_string();
        if !entry.fields.contains(&name) {
            entry.fields.push(name);
        }
    }

    pub fn to_json(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for entry in &self.entries {
            let mut obj_parts: Vec<String> = Vec::new();
            obj_parts.push(format!("\"name\":\"{}\"", json_escape(&entry.class_name)));
            if !entry.methods.is_empty() {
                let methods_json: Vec<String> = entry
                    .methods
                    .iter()
                    .map(|m| {
                        let params: Vec<String> = m
                            .parameter_types
                            .iter()
                            .map(|p| format!("\"{}\"", json_escape(p)))
                            .collect();
                        format!(
                            "{{\"name\":\"{}\",\"parameterTypes\":[{}]}}",
                            json_escape(&m.name),
                            params.join(",")
                        )
                    })
                    .collect();
                obj_parts.push(format!("\"methods\":[{}]", methods_json.join(",")));
            }
            if !entry.fields.is_empty() {
                let fields_json: Vec<String> = entry
                    .fields
                    .iter()
                    .map(|f| format!("\"{}\"", json_escape(f)))
                    .collect();
                obj_parts.push(format!("\"fields\":[{}]", fields_json.join(",")));
            }
            parts.push(format!("{{{}}}", obj_parts.join(",")));
        }
        format!("[{}]", parts.join(","))
    }
}

// ===========================================================================
// Proxy Configuration
// ===========================================================================

#[derive(Debug, Clone, Default)]
pub struct ProxyConfig {
    pub proxy_classes: Vec<Vec<String>>,
}

impl ProxyConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_proxy(&mut self, interfaces: &[&str]) {
        let ifaces: Vec<String> = interfaces.iter().map(|s| s.to_string()).collect();
        if !self.proxy_classes.contains(&ifaces) {
            self.proxy_classes.push(ifaces);
        }
    }

    pub fn to_json(&self) -> String {
        let entries: Vec<String> = self
            .proxy_classes
            .iter()
            .map(|ifaces| {
                let items: Vec<String> = ifaces
                    .iter()
                    .map(|i| format!("\"{}\"", json_escape(i)))
                    .collect();
                format!("[{}]", items.join(","))
            })
            .collect();
        format!("[{}]", entries.join(","))
    }
}

// ===========================================================================
// Serialization Configuration
// ===========================================================================

#[derive(Debug, Clone, PartialEq)]
pub struct SerializationEntry {
    pub class_name: String,
    pub custom_target_constructor_class: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SerializationConfig {
    pub entries: Vec<SerializationEntry>,
}

impl SerializationConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_class(&mut self, class_name: &str, custom_target: Option<&str>) {
        let entry = SerializationEntry {
            class_name: class_name.to_string(),
            custom_target_constructor_class: custom_target.map(|s| s.to_string()),
        };
        if !self.entries.contains(&entry) {
            self.entries.push(entry);
        }
    }

    pub fn to_json(&self) -> String {
        let parts: Vec<String> = self
            .entries
            .iter()
            .map(|e| {
                if let Some(ref target) = e.custom_target_constructor_class {
                    format!(
                        "{{\"name\":\"{}\",\"customTargetConstructorClass\":\"{}\"}}",
                        json_escape(&e.class_name),
                        json_escape(target)
                    )
                } else {
                    format!("{{\"name\":\"{}\"}}", json_escape(&e.class_name))
                }
            })
            .collect();
        format!("[{}]", parts.join(","))
    }
}

// ===========================================================================
// Native Image Build Configuration (top-level)
// ===========================================================================

#[derive(Debug, Clone, Default)]
pub struct NativeImageConfig {
    pub reflection: ReflectionConfig,
    pub resources: ResourceConfig,
    pub jni: JniConfig,
    pub proxies: ProxyConfig,
    pub serialization: SerializationConfig,
    pub build_args: Vec<String>,
    pub initialize_at_build_time: Vec<String>,
    pub initialize_at_run_time: Vec<String>,
}

impl NativeImageConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_build_arg(&mut self, arg: &str) {
        self.build_args.push(arg.to_string());
    }

    pub fn add_build_time_init(&mut self, class: &str) {
        if !self.initialize_at_build_time.iter().any(|c| c == class) {
            self.initialize_at_build_time.push(class.to_string());
        }
    }

    pub fn add_run_time_init(&mut self, class: &str) {
        if !self.initialize_at_run_time.iter().any(|c| c == class) {
            self.initialize_at_run_time.push(class.to_string());
        }
    }

    /// Generate all GraalVM native-image configuration files.
    /// Returns a map of filename to JSON content.
    pub fn generate_all_configs(&self) -> HashMap<String, String> {
        let mut configs = HashMap::new();
        configs.insert("reflect-config.json".to_string(), self.reflection.to_json());
        configs.insert("resource-config.json".to_string(), self.resources.to_json());
        configs.insert("jni-config.json".to_string(), self.jni.to_json());
        configs.insert("proxy-config.json".to_string(), self.proxies.to_json());
        configs.insert(
            "serialization-config.json".to_string(),
            self.serialization.to_json(),
        );
        configs
    }
}

// ===========================================================================
// Feature Detection / Substitution Registry
// ===========================================================================

#[derive(Debug, Clone)]
pub struct SubstitutionTarget {
    pub original_class: String,
    pub replacement_class: String,
    pub reason: String,
    pub active: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SubstitutionRegistry {
    pub substitutions: HashMap<String, SubstitutionTarget>,
}

impl SubstitutionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, original: &str, replacement: &str, reason: &str) {
        self.substitutions.insert(
            original.to_string(),
            SubstitutionTarget {
                original_class: original.to_string(),
                replacement_class: replacement.to_string(),
                reason: reason.to_string(),
                active: true,
            },
        );
    }

    pub fn lookup(&self, original: &str) -> Option<&SubstitutionTarget> {
        self.substitutions.get(original)
    }

    pub fn all_substitutions(&self) -> Vec<(&str, &SubstitutionTarget)> {
        self.substitutions
            .iter()
            .map(|(k, v)| (k.as_str(), v))
            .collect()
    }
}

// ===========================================================================
// Global GraalVM Metadata State
// ===========================================================================

/// Global GraalVM native-image configuration — collects metadata at runtime.
static GRAALVM_CONFIG: RwLock<Option<NativeImageConfig>> = RwLock::new(None);

/// Global substitution registry.
static GRAALVM_SUBSTITUTIONS: RwLock<Option<SubstitutionRegistry>> = RwLock::new(None);

/// Feature callbacks registered by the application.
static GRAALVM_FEATURES: RwLock<Option<Vec<FeatureRegistration>>> = RwLock::new(None);

/// Global `ImageSingletons` registry (org.graalvm.nativeimage.ImageSingletons).
///
/// Maps a singleton *key class* (internal slash-form name) to the live
/// singleton object. We never store the bare `ObjectRef` pointer across a GC
/// move — instead we keep the VM's pin handle (from
/// [`NativeContext::pin_native_root`]) plus the raw pointer as a fallback, and
/// read the forwarded reference back with `read_native_pin` on lookup.
static GRAALVM_SINGLETONS: RwLock<Option<HashMap<String, SingletonSlot>>> = RwLock::new(None);

/// One entry in the ImageSingletons registry.
#[derive(Debug, Clone, Copy)]
struct SingletonSlot {
    /// GC pin handle keeping the object alive (0 for non-relocating contexts).
    pin_handle: usize,
    /// Raw pointer fallback, used to rebuild an `ObjectRef` when the VM does
    /// not relocate (mock contexts return the fallback unchanged).
    raw_ptr: *mut u8,
}

// SAFETY: `SingletonSlot::raw_ptr` is only ever dereferenced by the VM through
// `read_native_pin` (which validates the handle); we store it purely as the
// fallback `ObjectRef` and never read through it directly. The object it names
// is kept alive by `pin_handle`. The pointer is therefore a plain integer
// token from this module's perspective, so the slot is safe to share.
unsafe impl Send for SingletonSlot {}
unsafe impl Sync for SingletonSlot {}

/// A registered GraalVM Feature (org.graalvm.nativeimage.hosted.Feature).
#[derive(Debug, Clone)]
pub struct FeatureRegistration {
    /// Feature class name (e.g. "com/example/MyFeature").
    pub class_name: String,
    /// Whether beforeAnalysis has been called.
    pub before_analysis_called: bool,
    /// Whether afterAnalysis has been called.
    pub after_analysis_called: bool,
}

/// Initialize the global GraalVM metadata collection state.
pub fn init_graalvm_metadata() {
    *GRAALVM_CONFIG.write() = Some(NativeImageConfig::new());
    *GRAALVM_SUBSTITUTIONS.write() = Some(SubstitutionRegistry::new());
    *GRAALVM_FEATURES.write() = Some(Vec::new());
    *GRAALVM_SINGLETONS.write() = Some(HashMap::new());
}

/// Register a class for runtime reflection access.
pub fn graalvm_register_reflection(class_name: &str, all_public: bool, all_declared: bool) {
    if let Some(ref mut config) = *GRAALVM_CONFIG.write() {
        config
            .reflection
            .add_class(class_name, all_public, all_declared);
    }
}

/// Register a class for runtime JNI access.
pub fn graalvm_register_jni(class_name: &str) {
    if let Some(ref mut config) = *GRAALVM_CONFIG.write() {
        config.jni.add_class(class_name);
    }
}

/// Register a class for serialization.
pub fn graalvm_register_serialization(class_name: &str) {
    if let Some(ref mut config) = *GRAALVM_CONFIG.write() {
        config.serialization.add_class(class_name, None);
    }
}

/// Register a resource pattern for inclusion.
pub fn graalvm_register_resource(pattern: &str) {
    if let Some(ref mut config) = *GRAALVM_CONFIG.write() {
        config.resources.add_include(pattern);
    }
}

/// Register a proxy interface set.
pub fn graalvm_register_proxy(interfaces: &[&str]) {
    if let Some(ref mut config) = *GRAALVM_CONFIG.write() {
        config.proxies.add_proxy(interfaces);
    }
}

/// Register a class substitution (for @TargetClass/@Substitute support).
pub fn graalvm_register_substitution(original: &str, replacement: &str, reason: &str) {
    if let Some(ref mut subs) = *GRAALVM_SUBSTITUTIONS.write() {
        subs.register(original, replacement, reason);
    }
}

/// Register a Feature class for lifecycle callbacks.
pub fn graalvm_register_feature(class_name: &str) {
    if let Some(ref mut features) = *GRAALVM_FEATURES.write() {
        if !features.iter().any(|f| f.class_name == class_name) {
            features.push(FeatureRegistration {
                class_name: class_name.to_string(),
                before_analysis_called: false,
                after_analysis_called: false,
            });
        }
    }
}

/// Generate all GraalVM native-image configuration files.
/// Returns a map of filename to JSON content.
pub fn graalvm_generate_configs() -> HashMap<String, String> {
    let guard = GRAALVM_CONFIG.read();
    match guard.as_ref() {
        Some(config) => config.generate_all_configs(),
        None => HashMap::new(),
    }
}

/// Dump GraalVM configuration to a directory.
/// Returns the number of files written.
pub fn graalvm_dump_configs(output_dir: &str) -> usize {
    // Reject path traversal in the output directory.
    let dir = std::path::Path::new(output_dir);
    for component in dir.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return 0; // Reject paths containing `..`
        }
    }
    let configs = graalvm_generate_configs();
    let mut written = 0;
    if std::fs::create_dir_all(dir).is_err() {
        return 0;
    }
    for (filename, content) in &configs {
        // Sanitize filename — reject any path separators or traversals in filenames.
        if filename.contains('/') || filename.contains('\\') || filename.contains("..") {
            continue;
        }
        let path = dir.join(filename);
        if std::fs::write(&path, content).is_ok() {
            written += 1;
        }
    }
    written
}

/// Get the current metadata statistics.
pub fn graalvm_metadata_stats() -> (usize, usize, usize, usize, usize) {
    let guard = GRAALVM_CONFIG.read();
    match guard.as_ref() {
        Some(config) => (
            config.reflection.entry_count(),
            config.resources.includes.len(),
            config.jni.entries.len(),
            config.proxies.proxy_classes.len(),
            config.serialization.entries.len(),
        ),
        None => (0, 0, 0, 0, 0),
    }
}

/// Check if a class is registered for reflection access.
/// Returns true if: no GraalVM config is active (permissive), or the class is registered.
pub fn graalvm_is_reflection_allowed(class_name: &str) -> bool {
    let guard = GRAALVM_CONFIG.read();
    match guard.as_ref() {
        None => true, // No config → permissive (not running as native-image)
        Some(config) => config
            .reflection
            .entries
            .iter()
            .any(|e| e.class_name == class_name),
    }
}

/// Check if a resource path is allowed by the resource config.
/// Returns true if: no config active, no include patterns defined, or path matches.
pub fn graalvm_is_resource_allowed(path: &str) -> bool {
    let guard = GRAALVM_CONFIG.read();
    match guard.as_ref() {
        None => true,
        Some(config) => {
            // If no include patterns are defined, allow all resources.
            if config.resources.includes.is_empty() {
                return true;
            }
            config.resources.matches(path)
        }
    }
}

/// Check if a class is registered for JNI access.
pub fn graalvm_is_jni_allowed(class_name: &str) -> bool {
    let guard = GRAALVM_CONFIG.read();
    match guard.as_ref() {
        None => true,
        Some(config) => config
            .jni
            .entries
            .iter()
            .any(|e| e.class_name == class_name),
    }
}

/// Check if a class is registered for serialization.
pub fn graalvm_is_serialization_allowed(class_name: &str) -> bool {
    let guard = GRAALVM_CONFIG.read();
    match guard.as_ref() {
        None => true,
        Some(config) => config
            .serialization
            .entries
            .iter()
            .any(|e| e.class_name == class_name),
    }
}

/// Check if a set of interfaces is registered as a proxy class.
pub fn graalvm_is_proxy_allowed(interfaces: &[&str]) -> bool {
    let guard = GRAALVM_CONFIG.read();
    match guard.as_ref() {
        None => true,
        Some(config) => {
            config.proxies.proxy_classes.iter().any(|p| {
                p.len() == interfaces.len() && p.iter().zip(interfaces).all(|(a, b)| a == b)
            })
        }
    }
}

/// Reset global GraalVM state (for testing).
#[cfg(test)]
fn reset_graalvm_globals() {
    *GRAALVM_CONFIG.write() = None;
    *GRAALVM_SUBSTITUTIONS.write() = None;
    *GRAALVM_FEATURES.write() = None;
    *GRAALVM_SINGLETONS.write() = None;
    set_image_code_mode(ImageCodeMode::Off);
}

/// True when an `ImageSingletons` entry exists for `key_class`.
///
/// Mirrors `ImageSingletons.contains(Class)`. Returns `false` when the
/// registry is not initialized.
pub fn graalvm_singleton_contains(key_class: &str) -> bool {
    let guard = GRAALVM_SINGLETONS.read();
    match guard.as_ref() {
        Some(map) => map.contains_key(key_class),
        None => false,
    }
}

/// Number of registered `ImageSingletons` entries (for diagnostics/tests).
pub fn graalvm_singleton_count() -> usize {
    let guard = GRAALVM_SINGLETONS.read();
    guard.as_ref().map(|m| m.len()).unwrap_or(0)
}

// ===========================================================================
// ImageInfo runtime mode (org.graalvm.nativeimage.ImageInfo)
// ===========================================================================
//
// Real GraalVM derives ImageInfo's booleans from the system property
// `org.graalvm.nativeimage.imagecode`, whose value is one of:
//   - unset      → not running inside a native image (HotSpot / CratonVM)
//   - "buildtime"→ executing in the image *generator* (build time)
//   - "runtime"  → executing inside the generated image at run time
//
// CratonVM is a regular JVM, so the honest default is *not in image*
// (`inImageCode() == false`), which matches real GraalVM running on HotSpot.
// We still model the three states explicitly so (a) test harnesses and
// build-tooling probing the surface get internally-consistent answers, and
// (b) a host embedding CratonVM as an image-generation sandbox can flip the
// mode via [`set_image_code_mode`].

/// The three ImageInfo execution states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageCodeMode {
    /// Not running inside a native image (default — CratonVM as a normal JVM).
    Off,
    /// Executing inside the native-image *generator* (build time).
    Buildtime,
    /// Executing inside a generated native image at run time.
    Runtime,
}

impl ImageCodeMode {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => ImageCodeMode::Buildtime,
            2 => ImageCodeMode::Runtime,
            _ => ImageCodeMode::Off,
        }
    }

    fn as_u8(self) -> u8 {
        match self {
            ImageCodeMode::Off => 0,
            ImageCodeMode::Buildtime => 1,
            ImageCodeMode::Runtime => 2,
        }
    }

    /// The GraalVM `org.graalvm.nativeimage.imagecode` property value, if any.
    pub fn property_value(self) -> Option<&'static str> {
        match self {
            ImageCodeMode::Off => None,
            ImageCodeMode::Buildtime => Some("buildtime"),
            ImageCodeMode::Runtime => Some("runtime"),
        }
    }

    fn from_property(value: &str) -> Self {
        match value {
            "buildtime" => ImageCodeMode::Buildtime,
            "runtime" => ImageCodeMode::Runtime,
            _ => ImageCodeMode::Off,
        }
    }
}

/// Global ImageInfo mode. Defaults to `Off` (CratonVM is not a native image).
static IMAGE_CODE_MODE: AtomicU8 = AtomicU8::new(0);

/// Override the ImageInfo execution mode. Hosts embedding CratonVM as an
/// image-generation sandbox call this; otherwise the default (`Off`) is the
/// correct answer for a normal JVM.
pub fn set_image_code_mode(mode: ImageCodeMode) {
    IMAGE_CODE_MODE.store(mode.as_u8(), Ordering::Release);
}

/// Current ImageInfo execution mode.
pub fn image_code_mode() -> ImageCodeMode {
    ImageCodeMode::from_u8(IMAGE_CODE_MODE.load(Ordering::Acquire))
}

/// Resolve the effective mode for a native call: the system property
/// (if the running program set one) takes precedence over the global flag,
/// mirroring GraalVM's property-driven contract.
fn effective_image_mode(ctx: &dyn NativeContext) -> ImageCodeMode {
    if let Some(v) = ctx.get_system_property("org.graalvm.nativeimage.imagecode") {
        return ImageCodeMode::from_property(&v);
    }
    image_code_mode()
}

// ===========================================================================
// Native method implementations for GraalVM Substrate VM APIs
// ===========================================================================

fn graalvm_in_image_code(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // True in either buildtime or runtime image modes.
    let in_image = effective_image_mode(ctx) != ImageCodeMode::Off;
    Ok(Some(Value::Int(if in_image { 1 } else { 0 })))
}

fn graalvm_in_image_buildtime_code(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let v = effective_image_mode(ctx) == ImageCodeMode::Buildtime;
    Ok(Some(Value::Int(if v { 1 } else { 0 })))
}

fn graalvm_in_image_runtime_code(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let v = effective_image_mode(ctx) == ImageCodeMode::Runtime;
    Ok(Some(Value::Int(if v { 1 } else { 0 })))
}

fn graalvm_is_executable(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // A generated image is an executable only while it is running (runtime).
    let v = effective_image_mode(ctx) == ImageCodeMode::Runtime;
    Ok(Some(Value::Int(if v { 1 } else { 0 })))
}

fn graalvm_is_shared_library(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // CratonVM never produces a shared-library image form.
    Ok(Some(Value::Int(0)))
}

fn graalvm_platform_included_in(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = Platform class (static), args[1] = Class to check
    // Detect the current platform and match against known GraalVM platform classes
    if let Some(Value::Object(Some(class_obj))) = args.get(1) {
        let class_id = ctx.class_id_of_object(*class_obj);
        if let Some(name) = ctx.class_name_of_id(class_id) {
            let is_match = match name.as_str() {
                "org/graalvm/nativeimage/Platform$LINUX" => cfg!(target_os = "linux"),
                "org/graalvm/nativeimage/Platform$DARWIN" => cfg!(target_os = "macos"),
                "org/graalvm/nativeimage/Platform$WINDOWS" => cfg!(target_os = "windows"),
                "org/graalvm/nativeimage/Platform$AMD64" => cfg!(target_arch = "x86_64"),
                "org/graalvm/nativeimage/Platform$AARCH64" => cfg!(target_arch = "aarch64"),
                _ => false,
            };
            return Ok(Some(Value::Int(if is_match { 1 } else { 0 })));
        }
    }
    Ok(Some(Value::Int(0)))
}

fn graalvm_runtime_reflection_register(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = Class object to register for reflection
    if let Some(Value::Object(Some(class_obj))) = args.first() {
        let class_id = ctx.class_id_of_object(*class_obj);
        if let Some(name) = ctx.class_name_of_id(class_id) {
            graalvm_register_reflection(&name, true, false);
        }
    }
    Ok(None)
}

fn graalvm_runtime_reflection_register_instantiation(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Register for both reflection and instantiation
    if let Some(Value::Object(Some(class_obj))) = args.first() {
        let class_id = ctx.class_id_of_object(*class_obj);
        if let Some(name) = ctx.class_name_of_id(class_id) {
            graalvm_register_reflection(&name, true, true);
        }
    }
    Ok(None)
}

fn graalvm_runtime_serialization_register(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(class_obj))) = args.first() {
        let class_id = ctx.class_id_of_object(*class_obj);
        if let Some(name) = ctx.class_name_of_id(class_id) {
            graalvm_register_serialization(&name);
        }
    }
    Ok(None)
}

fn graalvm_runtime_jni_register(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(class_obj))) = args.first() {
        let class_id = ctx.class_id_of_object(*class_obj);
        if let Some(name) = ctx.class_name_of_id(class_id) {
            graalvm_register_jni(&name);
        }
    }
    Ok(None)
}

fn graalvm_feature_register(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = Feature class object
    if let Some(Value::Object(Some(class_obj))) = args.first() {
        let class_id = ctx.class_id_of_object(*class_obj);
        if let Some(name) = ctx.class_name_of_id(class_id) {
            graalvm_register_feature(&name);
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// org.graalvm.nativeimage.ImageSingletons
// ---------------------------------------------------------------------------
//
// GraalVM's ImageSingletons is a build-time/run-time keyed registry of
// per-image singleton objects, keyed by Class. On a normal JVM there is no
// image, so frameworks that probe ImageSingletons (Micronaut, Quarkus, the
// GraalVM SDK itself) need a working in-heap map: `add(key, obj)` stores,
// `contains(key)` queries, `lookup(key)` returns the stored object (throwing
// in real GraalVM if absent — we return null, which the bytecode wrapper turns
// into the same outcome for the common `contains`-guarded call pattern).

/// Resolve the internal name of the Class object passed as `args[idx]`.
fn class_name_arg(ctx: &dyn NativeContext, args: &[Value], idx: usize) -> Option<String> {
    if let Some(Value::Object(Some(class_obj))) = args.get(idx) {
        let class_id = ctx.class_id_of_object(*class_obj);
        ctx.class_name_of_id(class_id)
    } else {
        None
    }
}

fn graalvm_singletons_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // static contains(Class) — args[0] = key Class
    let present = class_name_arg(ctx, args, 0)
        .map(|name| graalvm_singleton_contains(&name))
        .unwrap_or(false);
    Ok(Some(Value::Int(if present { 1 } else { 0 })))
}

fn graalvm_singletons_lookup(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // static <T> lookup(Class<T>) — args[0] = key Class; returns the singleton.
    let Some(name) = class_name_arg(ctx, args, 0) else {
        return Ok(Some(Value::Object(None)));
    };
    let slot = {
        let guard = GRAALVM_SINGLETONS.read();
        guard.as_ref().and_then(|m| m.get(&name).copied())
    };
    match slot {
        Some(slot) => {
            // Rebuild the fallback ObjectRef from the stored pointer, then ask
            // the VM for the current (possibly forwarded) reference.
            // SAFETY: raw_ptr was a live, non-null, 8-byte-aligned heap object
            // when stored via `add`; it is kept alive by the pin handle. In a
            // relocating VM `read_native_pin` returns the forwarded reference
            // and ignores the stale fallback.
            if slot.raw_ptr.is_null() {
                return Ok(Some(Value::Object(None)));
            }
            let fallback = unsafe { ObjectRef::from_raw(slot.raw_ptr) };
            let live = ctx.read_native_pin(slot.pin_handle, fallback);
            Ok(Some(Value::Object(Some(live))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

fn graalvm_singletons_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // static <T> add(Class<T> key, T value) — args[0] = key Class, args[1] = value.
    let Some(name) = class_name_arg(ctx, args, 0) else {
        return Ok(None);
    };
    let Some(Value::Object(Some(value))) = args.get(1).copied() else {
        return Ok(None);
    };
    // Pin the object so a moving GC keeps it alive and so we can read the
    // forwarded reference back on lookup.
    let pin_handle = ctx.pin_native_root(value);
    let raw_ptr = ctx.read_native_pin(pin_handle, value).as_ptr();
    if let Some(ref mut map) = *GRAALVM_SINGLETONS.write() {
        map.insert(
            name,
            SingletonSlot {
                pin_handle,
                raw_ptr,
            },
        );
    }
    Ok(None)
}

fn graalvm_dump_metadata(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = output directory path string
    // Returns: number of files written as Int
    let dir = if let Some(Value::Object(Some(path_ref))) = args.first() {
        ctx.read_string(*path_ref)
    } else {
        None
    };

    let written = if let Some(dir) = dir {
        graalvm_dump_configs(&dir)
    } else {
        0
    };
    Ok(Some(Value::Int(written as i32)))
}

pub(crate) fn register_graalvm_compat_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    // Initialize global metadata on first registration
    init_graalvm_metadata();

    // --- org/graalvm/nativeimage/ImageInfo ---
    r.register(
        "org/graalvm/nativeimage/ImageInfo",
        "inImageCode",
        "()Z",
        graalvm_in_image_code,
    );
    r.register(
        "org/graalvm/nativeimage/ImageInfo",
        "inImageBuildtimeCode",
        "()Z",
        graalvm_in_image_buildtime_code,
    );
    r.register(
        "org/graalvm/nativeimage/ImageInfo",
        "inImageRuntimeCode",
        "()Z",
        graalvm_in_image_runtime_code,
    );
    r.register(
        "org/graalvm/nativeimage/ImageInfo",
        "isExecutable",
        "()Z",
        graalvm_is_executable,
    );
    r.register(
        "org/graalvm/nativeimage/ImageInfo",
        "isSharedLibrary",
        "()Z",
        graalvm_is_shared_library,
    );

    // --- org/graalvm/nativeimage/RuntimeReflection ---
    r.register(
        "org/graalvm/nativeimage/RuntimeReflection",
        "register",
        "(Ljava/lang/Class;)V",
        graalvm_runtime_reflection_register,
    );
    r.register(
        "org/graalvm/nativeimage/RuntimeReflection",
        "registerForReflectiveInstantiation",
        "(Ljava/lang/Class;)V",
        graalvm_runtime_reflection_register_instantiation,
    );

    // --- org/graalvm/nativeimage/RuntimeSerialization ---
    r.register(
        "org/graalvm/nativeimage/RuntimeSerialization",
        "register",
        "(Ljava/lang/Class;)V",
        graalvm_runtime_serialization_register,
    );

    // --- org/graalvm/nativeimage/RuntimeJNIAccess ---
    r.register(
        "org/graalvm/nativeimage/RuntimeJNIAccess",
        "register",
        "(Ljava/lang/Class;)V",
        graalvm_runtime_jni_register,
    );

    // --- org/graalvm/nativeimage/Platform ---
    r.register(
        "org/graalvm/nativeimage/Platform",
        "includedIn",
        "(Ljava/lang/Class;)Z",
        graalvm_platform_included_in,
    );

    // --- org/graalvm/nativeimage/hosted/Feature ---
    r.register(
        "org/graalvm/nativeimage/hosted/Feature",
        "register",
        "(Ljava/lang/Class;)V",
        graalvm_feature_register,
    );

    // --- org/graalvm/nativeimage/ImageSingletons ---
    r.register(
        "org/graalvm/nativeimage/ImageSingletons",
        "contains",
        "(Ljava/lang/Class;)Z",
        graalvm_singletons_contains,
    );
    r.register(
        "org/graalvm/nativeimage/ImageSingletons",
        "lookup",
        "(Ljava/lang/Class;)Ljava/lang/Object;",
        graalvm_singletons_lookup,
    );
    r.register(
        "org/graalvm/nativeimage/ImageSingletons",
        "add",
        "(Ljava/lang/Class;Ljava/lang/Object;)V",
        graalvm_singletons_add,
    );

    // --- CratonVM extension: metadata dump ---
    r.register(
        "cratonvm/graalvm/MetadataAgent",
        "dumpConfigs",
        "(Ljava/lang/String;)I",
        graalvm_dump_metadata,
    );
    r.set_category(__prev_cat);
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    // Serialize tests that mutate GraalVM global state
    // (GRAALVM_CONFIG, GRAALVM_SUBSTITUTIONS, GRAALVM_FEATURES).
    // cargo test runs tests in parallel by default, so tests calling
    // `init_graalvm_metadata` / `reset_graalvm_globals` must hold this lock
    // for the duration of each test body to avoid cross-contamination.
    fn graalvm_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    // -----------------------------------------------------------------------
    // ReflectionConfig tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_reflection_config_new_is_empty() {
        let cfg = ReflectionConfig::new();
        assert_eq!(cfg.entry_count(), 0);
        assert_eq!(cfg.to_json(), "[]");
    }

    #[test]
    fn test_reflection_add_class_public() {
        let mut cfg = ReflectionConfig::new();
        cfg.add_class("com.example.Foo", true, false);
        assert_eq!(cfg.entry_count(), 1);
        let entry = &cfg.entries[0];
        assert!(entry.all_public_methods);
        assert!(entry.all_public_fields);
        assert!(entry.all_public_constructors);
        assert!(!entry.all_declared_methods);
    }

    #[test]
    fn test_reflection_add_class_declared() {
        let mut cfg = ReflectionConfig::new();
        cfg.add_class("com.example.Bar", false, true);
        let entry = &cfg.entries[0];
        assert!(entry.all_declared_methods);
        assert!(entry.all_declared_fields);
        assert!(entry.all_declared_constructors);
        assert!(!entry.all_public_methods);
    }

    #[test]
    fn test_reflection_add_class_both() {
        let mut cfg = ReflectionConfig::new();
        cfg.add_class("com.example.Baz", true, true);
        let entry = &cfg.entries[0];
        assert!(entry.all_public_methods);
        assert!(entry.all_declared_methods);
    }

    #[test]
    fn test_reflection_add_class_dedup() {
        let mut cfg = ReflectionConfig::new();
        cfg.add_class("com.example.Foo", true, false);
        cfg.add_class("com.example.Foo", false, true);
        assert_eq!(cfg.entry_count(), 1);
        let entry = &cfg.entries[0];
        assert!(entry.all_public_methods);
        assert!(entry.all_declared_methods);
    }

    #[test]
    fn test_reflection_add_method() {
        let mut cfg = ReflectionConfig::new();
        cfg.add_method("com.example.Foo", "doStuff", &["int", "java.lang.String"]);
        assert_eq!(cfg.entry_count(), 1);
        assert_eq!(cfg.entries[0].methods.len(), 1);
        assert_eq!(cfg.entries[0].methods[0].name, "doStuff");
        assert_eq!(cfg.entries[0].methods[0].parameter_types.len(), 2);
    }

    #[test]
    fn test_reflection_add_method_dedup() {
        let mut cfg = ReflectionConfig::new();
        cfg.add_method("com.example.Foo", "doStuff", &["int"]);
        cfg.add_method("com.example.Foo", "doStuff", &["int"]);
        assert_eq!(cfg.entries[0].methods.len(), 1);
    }

    #[test]
    fn test_reflection_add_field() {
        let mut cfg = ReflectionConfig::new();
        cfg.add_field("com.example.Foo", "myField", false);
        assert_eq!(cfg.entries[0].fields.len(), 1);
        assert!(!cfg.entries[0].fields[0].allow_write);
    }

    #[test]
    fn test_reflection_add_field_write_upgrade() {
        let mut cfg = ReflectionConfig::new();
        cfg.add_field("com.example.Foo", "myField", false);
        cfg.add_field("com.example.Foo", "myField", true);
        assert_eq!(cfg.entries[0].fields.len(), 1);
        assert!(cfg.entries[0].fields[0].allow_write);
    }

    #[test]
    fn test_reflection_merge_empty() {
        let mut cfg = ReflectionConfig::new();
        let other = ReflectionConfig::new();
        cfg.merge(&other);
        assert_eq!(cfg.entry_count(), 0);
    }

    #[test]
    fn test_reflection_merge_disjoint() {
        let mut cfg = ReflectionConfig::new();
        cfg.add_class("A", true, false);
        let mut other = ReflectionConfig::new();
        other.add_class("B", false, true);
        cfg.merge(&other);
        assert_eq!(cfg.entry_count(), 2);
    }

    #[test]
    fn test_reflection_merge_overlapping() {
        let mut cfg = ReflectionConfig::new();
        cfg.add_class("A", true, false);
        cfg.add_method("A", "foo", &[]);
        let mut other = ReflectionConfig::new();
        other.add_class("A", false, true);
        other.add_method("A", "bar", &[]);
        cfg.merge(&other);
        assert_eq!(cfg.entry_count(), 1);
        let entry = &cfg.entries[0];
        assert!(entry.all_public_methods);
        assert!(entry.all_declared_methods);
        assert_eq!(entry.methods.len(), 2);
    }

    #[test]
    fn test_reflection_to_json_basic() {
        let mut cfg = ReflectionConfig::new();
        cfg.add_class("com.example.Foo", true, false);
        let json = cfg.to_json();
        assert!(json.contains("\"name\":\"com.example.Foo\""));
        assert!(json.contains("\"allPublicMethods\":true"));
    }

    #[test]
    fn test_reflection_to_json_methods() {
        let mut cfg = ReflectionConfig::new();
        cfg.add_method("Foo", "bar", &["int"]);
        let json = cfg.to_json();
        assert!(json.contains("\"methods\""));
        assert!(json.contains("\"name\":\"bar\""));
        assert!(json.contains("\"parameterTypes\":[\"int\"]"));
    }

    #[test]
    fn test_reflection_to_json_fields() {
        let mut cfg = ReflectionConfig::new();
        cfg.add_field("Foo", "x", true);
        let json = cfg.to_json();
        assert!(json.contains("\"fields\""));
        assert!(json.contains("\"name\":\"x\""));
        assert!(json.contains("\"allowWrite\":true"));
    }

    #[test]
    fn test_reflection_from_json_empty() {
        let cfg = ReflectionConfig::from_json("[]").unwrap();
        assert_eq!(cfg.entry_count(), 0);
    }

    #[test]
    fn test_reflection_from_json_single() {
        let json = r#"[{"name":"com.example.Foo","allPublicMethods":true}]"#;
        let cfg = ReflectionConfig::from_json(json).unwrap();
        assert_eq!(cfg.entry_count(), 1);
        assert_eq!(cfg.entries[0].class_name, "com.example.Foo");
        assert!(cfg.entries[0].all_public_methods);
    }

    #[test]
    fn test_reflection_from_json_multiple() {
        let json = r#"[{"name":"A","allDeclaredMethods":true},{"name":"B"}]"#;
        let cfg = ReflectionConfig::from_json(json).unwrap();
        assert_eq!(cfg.entry_count(), 2);
        assert!(cfg.entries[0].all_declared_methods);
        assert!(!cfg.entries[1].all_declared_methods);
    }

    #[test]
    fn test_reflection_from_json_invalid() {
        assert!(ReflectionConfig::from_json("not json").is_err());
    }

    #[test]
    fn test_reflection_roundtrip() {
        let mut cfg = ReflectionConfig::new();
        cfg.add_class("com.example.Foo", true, true);
        let json = cfg.to_json();
        let cfg2 = ReflectionConfig::from_json(&json).unwrap();
        assert_eq!(cfg2.entry_count(), 1);
        assert_eq!(cfg2.entries[0].class_name, "com.example.Foo");
        assert!(cfg2.entries[0].all_public_methods);
        assert!(cfg2.entries[0].all_declared_methods);
    }

    // -----------------------------------------------------------------------
    // ResourceConfig / ResourcePattern tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_resource_pattern_exact() {
        let p = ResourcePattern::new("META-INF/MANIFEST.MF");
        assert!(p.matches("META-INF/MANIFEST.MF"));
        assert!(!p.matches("META-INF/other.txt"));
    }

    #[test]
    fn test_resource_pattern_star() {
        let p = ResourcePattern::new("META-INF/services/*");
        assert!(p.matches("META-INF/services/com.example.Spi"));
        assert!(!p.matches("META-INF/services/sub/deep"));
    }

    #[test]
    fn test_resource_pattern_star_extension() {
        let p = ResourcePattern::new("*.properties");
        assert!(p.matches("messages.properties"));
        assert!(!p.matches("dir/messages.properties"));
    }

    #[test]
    fn test_resource_pattern_double_star() {
        let p = ResourcePattern::new("**/*.xml");
        assert!(p.matches("a/b/c.xml"));
        assert!(p.matches("c.xml"));
        assert!(!p.matches("a/b/c.json"));
    }

    #[test]
    fn test_resource_config_matches_include() {
        let mut cfg = ResourceConfig::new();
        cfg.add_include("*.properties");
        assert!(cfg.matches("app.properties"));
        assert!(!cfg.matches("app.xml"));
    }

    #[test]
    fn test_resource_config_matches_exclude() {
        let mut cfg = ResourceConfig::new();
        cfg.add_include("**/*");
        cfg.add_exclude("*.class");
        assert!(cfg.matches("foo/bar.xml"));
        assert!(!cfg.matches("Foo.class"));
    }

    #[test]
    fn test_resource_config_no_include_no_match() {
        let cfg = ResourceConfig::new();
        assert!(!cfg.matches("anything.txt"));
    }

    #[test]
    fn test_resource_config_bundles() {
        let mut cfg = ResourceConfig::new();
        cfg.add_bundle("messages");
        cfg.add_bundle("messages"); // dedup
        assert_eq!(cfg.bundles.len(), 1);
        assert_eq!(cfg.bundles[0], "messages");
    }

    #[test]
    fn test_resource_config_to_json() {
        let mut cfg = ResourceConfig::new();
        cfg.add_include("*.xml");
        cfg.add_exclude("*.class");
        cfg.add_bundle("messages");
        let json = cfg.to_json();
        assert!(json.contains("\"includes\""));
        assert!(json.contains("\"excludes\""));
        assert!(json.contains("\"bundles\""));
        assert!(json.contains("\"messages\""));
    }

    // -----------------------------------------------------------------------
    // JniConfig tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_jni_config_empty() {
        let cfg = JniConfig::new();
        assert_eq!(cfg.to_json(), "[]");
    }

    #[test]
    fn test_jni_add_class() {
        let mut cfg = JniConfig::new();
        cfg.add_class("java.lang.String");
        assert_eq!(cfg.entries.len(), 1);
        assert_eq!(cfg.entries[0].class_name, "java.lang.String");
    }

    #[test]
    fn test_jni_add_method() {
        let mut cfg = JniConfig::new();
        cfg.add_method("java.lang.String", "length", &[]);
        assert_eq!(cfg.entries[0].methods.len(), 1);
    }

    #[test]
    fn test_jni_add_field() {
        let mut cfg = JniConfig::new();
        cfg.add_field("java.lang.Integer", "value");
        assert_eq!(cfg.entries[0].fields.len(), 1);
        assert_eq!(cfg.entries[0].fields[0], "value");
    }

    #[test]
    fn test_jni_add_field_dedup() {
        let mut cfg = JniConfig::new();
        cfg.add_field("java.lang.Integer", "value");
        cfg.add_field("java.lang.Integer", "value");
        assert_eq!(cfg.entries[0].fields.len(), 1);
    }

    #[test]
    fn test_jni_to_json() {
        let mut cfg = JniConfig::new();
        cfg.add_method("Cls", "m", &["int"]);
        cfg.add_field("Cls", "f");
        let json = cfg.to_json();
        assert!(json.contains("\"name\":\"Cls\""));
        assert!(json.contains("\"methods\""));
        assert!(json.contains("\"fields\""));
    }

    // -----------------------------------------------------------------------
    // ProxyConfig tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_proxy_config_empty() {
        let cfg = ProxyConfig::new();
        assert_eq!(cfg.to_json(), "[]");
    }

    #[test]
    fn test_proxy_add_proxy() {
        let mut cfg = ProxyConfig::new();
        cfg.add_proxy(&["java.io.Serializable", "java.lang.Comparable"]);
        assert_eq!(cfg.proxy_classes.len(), 1);
        assert_eq!(cfg.proxy_classes[0].len(), 2);
    }

    #[test]
    fn test_proxy_add_proxy_dedup() {
        let mut cfg = ProxyConfig::new();
        cfg.add_proxy(&["A", "B"]);
        cfg.add_proxy(&["A", "B"]);
        assert_eq!(cfg.proxy_classes.len(), 1);
    }

    #[test]
    fn test_proxy_to_json() {
        let mut cfg = ProxyConfig::new();
        cfg.add_proxy(&["java.io.Serializable"]);
        let json = cfg.to_json();
        assert!(json.contains("\"java.io.Serializable\""));
    }

    #[test]
    fn test_proxy_multiple_entries() {
        let mut cfg = ProxyConfig::new();
        cfg.add_proxy(&["A"]);
        cfg.add_proxy(&["B", "C"]);
        assert_eq!(cfg.proxy_classes.len(), 2);
        let json = cfg.to_json();
        assert!(json.starts_with("[["));
        assert!(json.ends_with("]]"));
    }

    // -----------------------------------------------------------------------
    // SerializationConfig tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_serialization_config_empty() {
        let cfg = SerializationConfig::new();
        assert_eq!(cfg.to_json(), "[]");
    }

    #[test]
    fn test_serialization_add_class() {
        let mut cfg = SerializationConfig::new();
        cfg.add_class("java.util.ArrayList", None);
        assert_eq!(cfg.entries.len(), 1);
        assert!(cfg.entries[0].custom_target_constructor_class.is_none());
    }

    #[test]
    fn test_serialization_add_class_with_target() {
        let mut cfg = SerializationConfig::new();
        cfg.add_class("com.example.Foo", Some("com.example.FooParent"));
        assert_eq!(
            cfg.entries[0].custom_target_constructor_class.as_deref(),
            Some("com.example.FooParent")
        );
    }

    #[test]
    fn test_serialization_dedup() {
        let mut cfg = SerializationConfig::new();
        cfg.add_class("A", None);
        cfg.add_class("A", None);
        assert_eq!(cfg.entries.len(), 1);
    }

    #[test]
    fn test_serialization_to_json() {
        let mut cfg = SerializationConfig::new();
        cfg.add_class("java.util.ArrayList", None);
        cfg.add_class("com.example.Foo", Some("com.example.Bar"));
        let json = cfg.to_json();
        assert!(json.contains("\"name\":\"java.util.ArrayList\""));
        assert!(json.contains("\"customTargetConstructorClass\":\"com.example.Bar\""));
    }

    // -----------------------------------------------------------------------
    // NativeImageConfig tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_native_image_config_new() {
        let cfg = NativeImageConfig::new();
        assert!(cfg.build_args.is_empty());
        assert!(cfg.initialize_at_build_time.is_empty());
        assert!(cfg.initialize_at_run_time.is_empty());
    }

    #[test]
    fn test_native_image_add_build_arg() {
        let mut cfg = NativeImageConfig::new();
        cfg.add_build_arg("--no-fallback");
        assert_eq!(cfg.build_args.len(), 1);
        assert_eq!(cfg.build_args[0], "--no-fallback");
    }

    #[test]
    fn test_native_image_add_build_time_init() {
        let mut cfg = NativeImageConfig::new();
        cfg.add_build_time_init("org.example.Foo");
        cfg.add_build_time_init("org.example.Foo"); // dedup
        assert_eq!(cfg.initialize_at_build_time.len(), 1);
    }

    #[test]
    fn test_native_image_add_run_time_init() {
        let mut cfg = NativeImageConfig::new();
        cfg.add_run_time_init("org.example.Bar");
        cfg.add_run_time_init("org.example.Bar"); // dedup
        assert_eq!(cfg.initialize_at_run_time.len(), 1);
    }

    #[test]
    fn test_native_image_generate_all_configs() {
        let mut cfg = NativeImageConfig::new();
        cfg.reflection.add_class("Foo", true, false);
        cfg.resources.add_include("*.xml");
        cfg.jni.add_class("Bar");
        cfg.proxies.add_proxy(&["I1"]);
        cfg.serialization.add_class("Baz", None);
        let configs = cfg.generate_all_configs();
        assert_eq!(configs.len(), 5);
        assert!(configs.contains_key("reflect-config.json"));
        assert!(configs.contains_key("resource-config.json"));
        assert!(configs.contains_key("jni-config.json"));
        assert!(configs.contains_key("proxy-config.json"));
        assert!(configs.contains_key("serialization-config.json"));
    }

    #[test]
    fn test_native_image_configs_have_content() {
        let mut cfg = NativeImageConfig::new();
        cfg.reflection.add_class("Foo", true, false);
        let configs = cfg.generate_all_configs();
        let reflect_json = &configs["reflect-config.json"];
        assert!(reflect_json.contains("Foo"));
    }

    // -----------------------------------------------------------------------
    // SubstitutionRegistry tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_substitution_registry_empty() {
        let reg = SubstitutionRegistry::new();
        assert!(reg.lookup("anything").is_none());
        assert!(reg.all_substitutions().is_empty());
    }

    #[test]
    fn test_substitution_register_and_lookup() {
        let mut reg = SubstitutionRegistry::new();
        reg.register(
            "sun.misc.Unsafe",
            "com.oracle.svm.core.UnsafeSubstitution",
            "Unsafe access",
        );
        let target = reg.lookup("sun.misc.Unsafe").unwrap();
        assert_eq!(
            target.replacement_class,
            "com.oracle.svm.core.UnsafeSubstitution"
        );
        assert_eq!(target.reason, "Unsafe access");
        assert!(target.active);
    }

    #[test]
    fn test_substitution_all_substitutions() {
        let mut reg = SubstitutionRegistry::new();
        reg.register("A", "A2", "reason1");
        reg.register("B", "B2", "reason2");
        let all = reg.all_substitutions();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_substitution_overwrite() {
        let mut reg = SubstitutionRegistry::new();
        reg.register("A", "A2", "old");
        reg.register("A", "A3", "new");
        let target = reg.lookup("A").unwrap();
        assert_eq!(target.replacement_class, "A3");
        assert_eq!(target.reason, "new");
    }

    // -----------------------------------------------------------------------
    // Native method registration tests
    // -----------------------------------------------------------------------

    fn make_registry() -> NativeMethodRegistry {
        let mut r = NativeMethodRegistry::new();
        register_graalvm_compat_natives(&mut r);
        r
    }

    const IMAGE_INFO: &str = "org/graalvm/nativeimage/ImageInfo";
    const RUNTIME_REFLECTION: &str = "org/graalvm/nativeimage/RuntimeReflection";
    const RUNTIME_SERIALIZATION: &str = "org/graalvm/nativeimage/RuntimeSerialization";
    const RUNTIME_JNI: &str = "org/graalvm/nativeimage/RuntimeJNIAccess";
    const PLATFORM: &str = "org/graalvm/nativeimage/Platform";

    #[test]
    fn test_register_image_info_in_image_code() {
        let r = make_registry();
        assert!(r.find(IMAGE_INFO, "inImageCode", "()Z").is_some());
    }

    #[test]
    fn test_register_image_info_in_image_buildtime_code() {
        let r = make_registry();
        assert!(r.find(IMAGE_INFO, "inImageBuildtimeCode", "()Z").is_some());
    }

    #[test]
    fn test_register_image_info_in_image_runtime_code() {
        let r = make_registry();
        assert!(r.find(IMAGE_INFO, "inImageRuntimeCode", "()Z").is_some());
    }

    #[test]
    fn test_register_image_info_is_executable() {
        let r = make_registry();
        assert!(r.find(IMAGE_INFO, "isExecutable", "()Z").is_some());
    }

    #[test]
    fn test_register_image_info_is_shared_library() {
        let r = make_registry();
        assert!(r.find(IMAGE_INFO, "isSharedLibrary", "()Z").is_some());
    }

    #[test]
    fn test_register_runtime_reflection_register() {
        let r = make_registry();
        assert!(r
            .find(RUNTIME_REFLECTION, "register", "(Ljava/lang/Class;)V")
            .is_some());
    }

    #[test]
    fn test_register_runtime_reflection_register_for_instantiation() {
        let r = make_registry();
        assert!(r
            .find(
                RUNTIME_REFLECTION,
                "registerForReflectiveInstantiation",
                "(Ljava/lang/Class;)V"
            )
            .is_some());
    }

    #[test]
    fn test_register_runtime_serialization() {
        let r = make_registry();
        assert!(r
            .find(RUNTIME_SERIALIZATION, "register", "(Ljava/lang/Class;)V")
            .is_some());
    }

    #[test]
    fn test_register_runtime_jni_access() {
        let r = make_registry();
        assert!(r
            .find(RUNTIME_JNI, "register", "(Ljava/lang/Class;)V")
            .is_some());
    }

    #[test]
    fn test_register_platform_included_in() {
        let r = make_registry();
        assert!(r
            .find(PLATFORM, "includedIn", "(Ljava/lang/Class;)Z")
            .is_some());
    }

    /// What the real GraalVM SDK has where CratonVM registered something.
    ///
    /// Measured, not recalled — see [`GRAALVM_COMPAT_REGISTRATIONS`] for the
    /// exact `javap` provenance.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Sdk {
        /// `javap` shows this exact class, method name and descriptor.
        Exact,
        /// The SDK has no such `(class, method, descriptor)`. The payload is
        /// what the SDK really declares, or `None` when the SDK has no method
        /// of that name on any related class at all.
        ///
        /// A `Divergent` row is an EXEMPTION, and it expires: if the
        /// registration is ever corrected to match the payload, the row stops
        /// being divergent and
        /// `the_graalvm_sdk_divergences_are_the_five_that_were_measured` fails
        /// telling you to reclassify it.
        Divergent(Option<(&'static str, &'static str, &'static str)>),
        /// A CratonVM extension with no GraalVM counterpart, on purpose.
        CratonExtension,
    }

    /// Every triple `register_graalvm_compat_natives` installs, frozen, with
    /// each row's verdict against the real GraalVM SDK.
    ///
    /// Two-sided on purpose: a row missing from the registry is a LOST
    /// registration, and a registration missing from this table is an
    /// UNDECLARED one. A changed descriptor trips both halves at once, which is
    /// why the descriptor is part of the row rather than a separate assertion.
    ///
    /// Transcribed from `register_graalvm_compat_natives` on 2026-08-13 and
    /// verified by counting `r.register(` in that function: 5 `ImageInfo` +
    /// 2 `RuntimeReflection` + 1 `RuntimeSerialization` + 1 `RuntimeJNIAccess`
    /// + 1 `Platform` + 1 `hosted/Feature` + 3 `ImageSingletons` + 1 CratonVM
    /// `MetadataAgent` extension = **15**, not the 10 the old comment claimed.
    ///
    /// EXTERNAL ORACLE. The `Sdk` column was measured on this host on
    /// 2026-08-13 with
    ///
    /// ```text
    /// javap -public -s -cp nativeimage-25.0.2.jar org.graalvm.nativeimage.<Class>
    /// ```
    ///
    /// against the GraalVM SDK **25.0.2** shipped in
    /// `tornadovm-4.0.1-jdk25-ptx/share/java/tornado/nativeimage-25.0.2.jar`,
    /// using `javap` from `openjdk 25.0.3 2026-04-21 LTS, Microsoft-13877124,
    /// build 25.0.3+9-LTS`. Nine rows matched the SDK exactly. Five did not,
    /// and they are recorded below rather than quietly asserted away:
    /// `RuntimeReflection`, `RuntimeSerialization` and `RuntimeJNIAccess` live
    /// in `org.graalvm.nativeimage.**hosted**`, not in
    /// `org.graalvm.nativeimage`, and every one of their `register` methods is
    /// **varargs** (`([Ljava/lang/Class;)V`), while
    /// `org.graalvm.nativeimage.hosted.Feature` declares no `register` method
    /// of any shape. See NOM E33-1 —
    /// `docs/known-issues/jdk-only/E33-R11-FOUR-UNFALSIFIABLE-GUARDS-20260813.md`.
    ///
    /// RESIDUAL: the jar is a host path, not a checked-in fixture, so nothing
    /// re-measures this column at test time. NOM E33-1 asks for the frozen
    /// `javap` output to be committed next to this table.
    #[rustfmt::skip]
    const GRAALVM_COMPAT_REGISTRATIONS: &[(&str, &str, &str, Sdk)] = &[
        ("org/graalvm/nativeimage/ImageInfo",             "inImageCode",                        "()Z",                                   Sdk::Exact),
        ("org/graalvm/nativeimage/ImageInfo",             "inImageBuildtimeCode",               "()Z",                                   Sdk::Exact),
        ("org/graalvm/nativeimage/ImageInfo",             "inImageRuntimeCode",                 "()Z",                                   Sdk::Exact),
        ("org/graalvm/nativeimage/ImageInfo",             "isExecutable",                       "()Z",                                   Sdk::Exact),
        ("org/graalvm/nativeimage/ImageInfo",             "isSharedLibrary",                    "()Z",                                   Sdk::Exact),
        ("org/graalvm/nativeimage/RuntimeReflection",     "register",                           "(Ljava/lang/Class;)V",                  Sdk::Divergent(Some(("org/graalvm/nativeimage/hosted/RuntimeReflection",    "register",                           "([Ljava/lang/Class;)V")))),
        ("org/graalvm/nativeimage/RuntimeReflection",     "registerForReflectiveInstantiation", "(Ljava/lang/Class;)V",                  Sdk::Divergent(Some(("org/graalvm/nativeimage/hosted/RuntimeReflection",    "registerForReflectiveInstantiation", "([Ljava/lang/Class;)V")))),
        ("org/graalvm/nativeimage/RuntimeSerialization",  "register",                           "(Ljava/lang/Class;)V",                  Sdk::Divergent(Some(("org/graalvm/nativeimage/hosted/RuntimeSerialization", "register",                           "([Ljava/lang/Class;)V")))),
        ("org/graalvm/nativeimage/RuntimeJNIAccess",      "register",                           "(Ljava/lang/Class;)V",                  Sdk::Divergent(Some(("org/graalvm/nativeimage/hosted/RuntimeJNIAccess",     "register",                           "([Ljava/lang/Class;)V")))),
        ("org/graalvm/nativeimage/Platform",              "includedIn",                         "(Ljava/lang/Class;)Z",                  Sdk::Exact),
        ("org/graalvm/nativeimage/hosted/Feature",        "register",                           "(Ljava/lang/Class;)V",                  Sdk::Divergent(None)),
        ("org/graalvm/nativeimage/ImageSingletons",       "contains",                           "(Ljava/lang/Class;)Z",                  Sdk::Exact),
        ("org/graalvm/nativeimage/ImageSingletons",       "lookup",                             "(Ljava/lang/Class;)Ljava/lang/Object;", Sdk::Exact),
        ("org/graalvm/nativeimage/ImageSingletons",       "add",                                "(Ljava/lang/Class;Ljava/lang/Object;)V", Sdk::Exact),
        ("cratonvm/graalvm/MetadataAgent",                "dumpConfigs",                        "(Ljava/lang/String;)I",                 Sdk::CratonExtension),
    ];

    /// The five GraalVM-SDK divergences measured on 2026-08-13, named.
    ///
    /// This is the ratcheted, EXPIRING half of the exemption. It fails three
    /// ways, and every one of them is a thing somebody would otherwise do
    /// silently:
    ///
    ///  * a sixth divergence appears — a new registration that does not match
    ///    the SDK, or an `Exact` row edited into a wrong shape;
    ///  * a divergence disappears — good news, but the row must be reclassified
    ///    `Sdk::Exact` in the same edit or the exemption outlives the exception
    ///    (E20's decay mode, and E25's `already_triaged` rows, again);
    ///  * a `Divergent(Some(..))` row whose registration now equals what the
    ///    SDK declares. That row is fixed and is lying about itself.
    #[rustfmt::skip]
    const MEASURED_SDK_DIVERGENCES: &[&str] = &[
        "org/graalvm/nativeimage/RuntimeReflection::register(Ljava/lang/Class;)V",
        "org/graalvm/nativeimage/RuntimeReflection::registerForReflectiveInstantiation(Ljava/lang/Class;)V",
        "org/graalvm/nativeimage/RuntimeSerialization::register(Ljava/lang/Class;)V",
        "org/graalvm/nativeimage/RuntimeJNIAccess::register(Ljava/lang/Class;)V",
        "org/graalvm/nativeimage/hosted/Feature::register(Ljava/lang/Class;)V",
    ];

    #[test]
    fn the_graalvm_sdk_divergences_are_the_five_that_were_measured() {
        let mut divergent: Vec<String> = Vec::new();
        let mut stale: Vec<String> = Vec::new();
        for (class, method, descriptor, sdk) in GRAALVM_COMPAT_REGISTRATIONS.iter().copied() {
            if let Sdk::Divergent(what_the_sdk_has) = sdk {
                divergent.push(format!("{class}::{method}{descriptor}"));
                if what_the_sdk_has == Some((class, method, descriptor)) {
                    stale.push(format!("{class}::{method}{descriptor}"));
                }
            }
        }
        divergent.sort();
        let mut measured: Vec<String> = MEASURED_SDK_DIVERGENCES
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        measured.sort();

        assert!(
            stale.is_empty(),
            "these rows are marked Sdk::Divergent but now register exactly what \
             the GraalVM SDK declares:\n    {}\n\n\
             The registration was fixed and the row was not. Change it to \
             Sdk::Exact and drop it from MEASURED_SDK_DIVERGENCES.",
            stale.join("\n    ")
        );
        assert_eq!(
            divergent, measured,
            "the set of registrations that disagree with GraalVM SDK 25.0.2 \
             changed. If a divergence was FIXED, delete its row from \
             MEASURED_SDK_DIVERGENCES and mark it Sdk::Exact. If a NEW one \
             appeared, re-measure with `javap -public -s -cp \
             nativeimage-25.0.2.jar <class>` before adding it — an exemption \
             added without a measurement is how this whole class of guard rots."
        );
        assert_eq!(
            GRAALVM_COMPAT_REGISTRATIONS
                .iter()
                .filter(|(_, _, _, s)| *s == Sdk::CratonExtension)
                .count(),
            1,
            "exactly one row is a CratonVM extension with no GraalVM \
             counterpart (`cratonvm/graalvm/MetadataAgent`). A second one is \
             either a real extension that needs saying so, or an \
             `org.graalvm.*` row misfiled to dodge the SDK check."
        );
    }

    /// WHY THIS TEST WAS REWRITTEN ON 2026-08-13.
    ///
    /// Its name says "total method count" and the comment inside it did the
    /// arithmetic `= 10 methods`, but the entire body was one line:
    /// `assert!(r.find(IMAGE_INFO, "nonExistent", "()V").is_none());`. That
    /// asserts the absence of a method **nothing has ever registered**, so no
    /// registration change of any kind could turn this test red — deleting
    /// every single `r.register(...)` call in `register_graalvm_compat_natives`
    /// left it green. It was counted as coverage for a claim it never made.
    /// (E25 sweep, `docs/known-issues/jdk-only/E25-R11-GUARD-POPULATION-SWEEP-20260813.md`
    /// §3 row 1.) The arithmetic was wrong too: the registrar makes 15
    /// registrations, and the tally omitted `ImageSingletons` entirely.
    ///
    /// The replacement is the shape this tree already blesses for exactly this
    /// failure — `vm/tests/wp8_10_9_string_contains_native.rs:266`
    /// `the_surviving_string_registration_set_is_exactly_this`, written because
    /// "both passed while the drop was silently deleting four registrations
    /// nobody had thought to name". A two-sided frozen set, plus the
    /// over-registration probe the old body did carry.
    ///
    /// The GraalVM-SDK conformance of each row is a separate, measured column —
    /// see [`GRAALVM_COMPAT_REGISTRATIONS`] and
    /// `the_graalvm_sdk_divergences_are_the_five_that_were_measured`. This test
    /// asserts only that the registrar registers what it says it registers.
    #[test]
    fn test_register_total_method_count() {
        let r = make_registry();

        let mut expected: Vec<(&str, &str, &str)> = GRAALVM_COMPAT_REGISTRATIONS
            .iter()
            .map(|(c, m, d, _sdk)| (*c, *m, *d))
            .collect();
        expected.sort_unstable();
        let declared = expected.len();
        expected.dedup();
        assert_eq!(
            expected.len(),
            declared,
            "GRAALVM_COMPAT_REGISTRATIONS lists the same triple twice; a duplicated row \
             would hide a lost registration behind its own copy"
        );

        let mut actual: Vec<(&str, &str, &str)> = r
            .dump_registrations()
            .into_iter()
            .map(|(c, m, d, _kind)| (c, m, d))
            .collect();
        actual.sort_unstable();
        actual.dedup();

        let lost: Vec<String> = expected
            .iter()
            .copied()
            .filter(|row| !actual.contains(row))
            .map(|(c, m, d)| format!("{c}::{m}{d}"))
            .collect();
        let undeclared: Vec<String> = actual
            .iter()
            .copied()
            .filter(|row| !expected.contains(row))
            .map(|(c, m, d)| format!("{c}::{m}{d}"))
            .collect();

        assert!(
            lost.is_empty() && undeclared.is_empty(),
            "register_graalvm_compat_natives no longer matches its frozen set.\n  \
             REGISTRATION LOST (declared here, not registered):\n    {}\n  \
             UNDECLARED REGISTRATION (registered, not declared here):\n    {}\n\n\
             If you ADDED a native, add its row above in the same edit. If a row \
             disappeared, a registration was dropped — that is the failure this \
             test exists for, and until 2026-08-13 it could not report it.",
            if lost.is_empty() {
                "(none)".to_string()
            } else {
                lost.join("\n    ")
            },
            if undeclared.is_empty() {
                "(none)".to_string()
            } else {
                undeclared.join("\n    ")
            },
        );

        // The count the name promises, now backed by the set above rather than
        // by a comment.
        assert_eq!(
            r.len(),
            GRAALVM_COMPAT_REGISTRATIONS.len(),
            "registry slot count disagrees with the frozen set even though every \
             triple matched — two rows must have collapsed onto one slot"
        );

        // Retained from the old body: a name nothing registers must stay
        // unregistered. This is the only half the old test had.
        assert!(r.find(IMAGE_INFO, "nonExistent", "()V").is_none());
    }

    /// The anchor OUTSIDE this module: a registry this module does not build.
    ///
    /// `register_graalvm_compat_natives` is reached from exactly one call site
    /// (`lib.rs`, inside `register_synthetic_overrides`), so every triple above
    /// is a Substrate-VM stand-in that must never be visible on the real-JDK
    /// path. `ImageInfo.inImageCode()` answering on a real JDK would tell an
    /// application it is running inside a native image when it is not; the
    /// `register_p68_xml` precedent (a synthetic surface pulled onto the
    /// real-JDK path, which pre-empted Tomcat's real SAX parser and broke
    /// `server.xml`) is the same mistake with a different class name.
    ///
    /// Unlike the frozen set above, the expectation here is not transcribed
    /// from the code under test: it is read off a registry built by
    /// `register_essential_natives`.
    #[test]
    fn no_graalvm_substrate_stub_reaches_the_essential_path() {
        let mut essential = NativeMethodRegistry::new();
        crate::register_essential_natives(&mut essential);

        // Anti-vacuity: an empty registry would make the check below pass for
        // the wrong reason.
        assert!(
            essential.len() > 100,
            "register_essential_natives produced only {} registrations — this \
             check would have passed vacuously",
            essential.len()
        );

        let leaked: Vec<String> = GRAALVM_COMPAT_REGISTRATIONS
            .iter()
            .copied()
            // `kind_of` is the EXACT-triple lookup. `find` would also match
            // through the registry's descriptor-compatibility rewriting, which
            // could report a leak that is really a different registration.
            .filter(|(c, m, d, _sdk)| essential.kind_of(c, m, d).is_some())
            .map(|(c, m, d, _sdk)| format!("{c}::{m}{d}"))
            .collect();
        assert!(
            leaked.is_empty(),
            "GraalVM Substrate stubs reached the real-JDK path:\n    {}\n\n\
             These are synthetic stand-ins for `org.graalvm.nativeimage.*`. On a \
             real JDK the application is not a native image, and these would \
             answer for it anyway.",
            leaked.join("\n    ")
        );
    }

    // -----------------------------------------------------------------------
    // Glob matching edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_glob_empty_pattern_matches_empty_string() {
        assert!(glob_match("", ""));
    }

    #[test]
    fn test_glob_star_matches_empty() {
        assert!(glob_match("*", ""));
    }

    #[test]
    fn test_glob_double_star_matches_deep_path() {
        assert!(glob_match("**/file.txt", "a/b/c/file.txt"));
    }

    #[test]
    fn test_glob_no_match() {
        assert!(!glob_match("foo", "bar"));
    }

    #[test]
    fn test_glob_star_does_not_cross_slash() {
        assert!(!glob_match("*.txt", "dir/file.txt"));
    }

    // -----------------------------------------------------------------------
    // JSON parsing edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_split_top_level_objects_multiple() {
        let objects = split_top_level_objects(r#"{"a":1},{"b":2}"#).unwrap();
        assert_eq!(objects.len(), 2);
    }

    #[test]
    fn test_split_top_level_objects_unbalanced() {
        assert!(split_top_level_objects("{").is_err());
    }

    #[test]
    fn test_extract_string_value_present() {
        let v = extract_string_value(r#"{"name":"Foo"}"#, "name");
        assert_eq!(v, Some("Foo".to_string()));
    }

    #[test]
    fn test_extract_string_value_absent() {
        let v = extract_string_value(r#"{"name":"Foo"}"#, "missing");
        assert_eq!(v, None);
    }

    #[test]
    fn test_extract_bool_value_true() {
        assert!(extract_bool_value(r#"{"flag":true}"#, "flag"));
    }

    #[test]
    fn test_extract_bool_value_false() {
        assert!(!extract_bool_value(r#"{"flag":false}"#, "flag"));
    }

    // -----------------------------------------------------------------------
    // Global GraalVM metadata state tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_init_graalvm_metadata() {
        let _guard = graalvm_test_lock();
        reset_graalvm_globals();
        init_graalvm_metadata();
        assert!(GRAALVM_CONFIG.read().is_some());
        assert!(GRAALVM_SUBSTITUTIONS.read().is_some());
        assert!(GRAALVM_FEATURES.read().is_some());
        reset_graalvm_globals();
    }

    #[test]
    fn test_graalvm_register_reflection() {
        let _guard = graalvm_test_lock();
        reset_graalvm_globals();
        init_graalvm_metadata();
        graalvm_register_reflection("com.example.Foo", true, false);
        graalvm_register_reflection("com.example.Bar", false, true);
        let (refl, _, _, _, _) = graalvm_metadata_stats();
        assert_eq!(refl, 2);
        reset_graalvm_globals();
    }

    #[test]
    fn test_graalvm_register_jni() {
        let _guard = graalvm_test_lock();
        reset_graalvm_globals();
        init_graalvm_metadata();
        graalvm_register_jni("com.example.NativeLib");
        let (_, _, jni, _, _) = graalvm_metadata_stats();
        assert_eq!(jni, 1);
        reset_graalvm_globals();
    }

    #[test]
    fn test_graalvm_register_serialization() {
        let _guard = graalvm_test_lock();
        reset_graalvm_globals();
        init_graalvm_metadata();
        graalvm_register_serialization("java.util.ArrayList");
        let (_, _, _, _, ser) = graalvm_metadata_stats();
        assert_eq!(ser, 1);
        reset_graalvm_globals();
    }

    #[test]
    fn test_graalvm_register_resource() {
        let _guard = graalvm_test_lock();
        reset_graalvm_globals();
        init_graalvm_metadata();
        graalvm_register_resource("**/*.properties");
        graalvm_register_resource("META-INF/services/*");
        let (_, res, _, _, _) = graalvm_metadata_stats();
        assert_eq!(res, 2);
        reset_graalvm_globals();
    }

    #[test]
    fn test_graalvm_register_proxy() {
        let _guard = graalvm_test_lock();
        reset_graalvm_globals();
        init_graalvm_metadata();
        graalvm_register_proxy(&["java.io.Serializable", "java.lang.Comparable"]);
        let (_, _, _, proxy, _) = graalvm_metadata_stats();
        assert_eq!(proxy, 1);
        reset_graalvm_globals();
    }

    #[test]
    fn test_graalvm_register_substitution() {
        let _guard = graalvm_test_lock();
        reset_graalvm_globals();
        init_graalvm_metadata();
        graalvm_register_substitution(
            "sun.misc.Unsafe",
            "com.oracle.svm.core.UnsafeSubst",
            "Unsafe access",
        );
        {
            let guard = GRAALVM_SUBSTITUTIONS.read();
            let subs = guard.as_ref().unwrap();
            let target = subs.lookup("sun.misc.Unsafe").unwrap();
            assert_eq!(target.replacement_class, "com.oracle.svm.core.UnsafeSubst");
        }
        reset_graalvm_globals();
    }

    #[test]
    fn test_graalvm_register_feature() {
        let _guard = graalvm_test_lock();
        reset_graalvm_globals();
        init_graalvm_metadata();
        graalvm_register_feature("com/example/MyFeature");
        graalvm_register_feature("com/example/MyFeature"); // dedup
        {
            let guard = GRAALVM_FEATURES.read();
            let features = guard.as_ref().unwrap();
            assert_eq!(features.len(), 1);
            assert_eq!(features[0].class_name, "com/example/MyFeature");
            assert!(!features[0].before_analysis_called);
        }
        reset_graalvm_globals();
    }

    #[test]
    fn test_graalvm_generate_configs() {
        let _guard = graalvm_test_lock();
        reset_graalvm_globals();
        init_graalvm_metadata();
        graalvm_register_reflection("com.example.Foo", true, false);
        graalvm_register_resource("*.xml");
        graalvm_register_jni("com.example.NativeLib");
        let configs = graalvm_generate_configs();
        assert_eq!(configs.len(), 5);
        assert!(configs["reflect-config.json"].contains("com.example.Foo"));
        assert!(configs["resource-config.json"].contains("*.xml"));
        assert!(configs["jni-config.json"].contains("com.example.NativeLib"));
        reset_graalvm_globals();
    }

    #[test]
    fn test_graalvm_dump_configs_to_dir() {
        let _guard = graalvm_test_lock();
        reset_graalvm_globals();
        init_graalvm_metadata();
        graalvm_register_reflection("com.example.Test", true, false);

        // Per-test tempdir so parallel tests don't share "cratonvm_graalvm_test"
        // in $TMP and clobber each other's dumps.  TempDir cleans up on drop.
        let tmp = tempfile::TempDir::new().expect("create per-test tempdir");
        let dir = tmp.path();
        let dir_str = dir.to_string_lossy().to_string();
        let written = graalvm_dump_configs(&dir_str);
        assert_eq!(written, 5);
        assert!(dir.join("reflect-config.json").exists());
        assert!(dir.join("resource-config.json").exists());
        assert!(dir.join("jni-config.json").exists());
        assert!(dir.join("proxy-config.json").exists());
        assert!(dir.join("serialization-config.json").exists());

        // Verify content
        let reflect_content = std::fs::read_to_string(dir.join("reflect-config.json")).unwrap();
        assert!(reflect_content.contains("com.example.Test"));

        reset_graalvm_globals();
    }

    #[test]
    fn test_graalvm_metadata_stats_empty() {
        let _guard = graalvm_test_lock();
        reset_graalvm_globals();
        init_graalvm_metadata();
        let (r, res, j, p, s) = graalvm_metadata_stats();
        assert_eq!((r, res, j, p, s), (0, 0, 0, 0, 0));
        reset_graalvm_globals();
    }

    #[test]
    fn test_graalvm_metadata_stats_not_initialized() {
        let _guard = graalvm_test_lock();
        reset_graalvm_globals();
        let (r, res, j, p, s) = graalvm_metadata_stats();
        assert_eq!((r, res, j, p, s), (0, 0, 0, 0, 0));
    }

    #[test]
    fn test_graalvm_register_multiple_features() {
        let _guard = graalvm_test_lock();
        reset_graalvm_globals();
        init_graalvm_metadata();
        graalvm_register_feature("com/example/Feature1");
        graalvm_register_feature("com/example/Feature2");
        {
            let guard = GRAALVM_FEATURES.read();
            let features = guard.as_ref().unwrap();
            assert_eq!(features.len(), 2);
        }
        reset_graalvm_globals();
    }

    #[test]
    fn test_register_graalvm_compat_total_count() {
        let r = make_registry();
        // 5 ImageInfo + 2 RuntimeReflection + 1 RuntimeSerialization
        // + 1 RuntimeJNIAccess + 1 Platform + 1 Feature + 1 MetadataAgent = 12
        assert!(r.len() >= 12);
    }

    // -----------------------------------------------------------------------
    // JSON escape and to_json injection tests (M30 fixes)
    // -----------------------------------------------------------------------

    #[test]
    fn test_json_escape_basic() {
        assert_eq!(json_escape("hello"), "hello");
        assert_eq!(json_escape(""), "");
    }

    #[test]
    fn test_json_escape_special_chars() {
        assert_eq!(json_escape("a\"b"), "a\\\"b");
        assert_eq!(json_escape("a\\b"), "a\\\\b");
        assert_eq!(json_escape("a\nb"), "a\\nb");
        assert_eq!(json_escape("a\tb"), "a\\tb");
        assert_eq!(json_escape("a\rb"), "a\\rb");
    }

    #[test]
    fn test_jni_config_to_json_escapes_names() {
        let mut cfg = JniConfig::new();
        cfg.add_class("com/evil/Class\"injection");
        let json = cfg.to_json();
        assert!(json.contains("com/evil/Class\\\"injection"));
        assert!(!json.contains("com/evil/Class\"injection\""));
    }

    #[test]
    fn test_proxy_config_to_json_escapes() {
        let mut cfg = ProxyConfig::new();
        cfg.add_proxy(&["java/io/Serial\"izable"]);
        let json = cfg.to_json();
        assert!(json.contains("Serial\\\"izable"));
    }

    #[test]
    fn test_serialization_config_to_json_escapes() {
        let mut cfg = SerializationConfig::new();
        cfg.add_class("com/test/Evil\"Class", Some("com/target\"Ctor"));
        let json = cfg.to_json();
        assert!(json.contains("Evil\\\"Class"));
        assert!(json.contains("target\\\"Ctor"));
    }

    #[test]
    fn test_resource_config_to_json_escapes_patterns() {
        let mut cfg = ResourceConfig::new();
        cfg.add_include("*.\"json");
        let json = cfg.to_json();
        assert!(json.contains("*.\\\"json"));
    }

    // -----------------------------------------------------------------------
    // Glob match iterative (no stack overflow / ReDoS)
    // -----------------------------------------------------------------------

    #[test]
    fn test_glob_match_no_redos_on_long_input() {
        // This would cause stack overflow with recursive implementation
        let pattern = "**/*";
        let input = "a/".repeat(1000) + "file.txt";
        // Just verify it completes without hanging
        let _ = glob_match(pattern, &input);
    }

    #[test]
    fn test_glob_match_star_star_patterns() {
        assert!(glob_match("**/foo.txt", "a/b/c/foo.txt"));
        assert!(glob_match("src/**/*.rs", "src/lib.rs"));
        assert!(glob_match("src/**/*.rs", "src/deep/nested/file.rs"));
        assert!(!glob_match("src/**/*.rs", "test/file.rs"));
    }

    // -----------------------------------------------------------------------
    // ImageInfo execution-mode tests
    // -----------------------------------------------------------------------
    //
    // These drive the real native bodies through the in-crate MockNativeContext
    // (which resolves class names + system properties), so they exercise the
    // mode logic end-to-end rather than just registration presence.

    use crate::test_utils::MockNativeContext;

    /// Allocate an object whose runtime class resolves to `class_name`. Our
    /// `ImageSingletons` natives only need `class_id_of_object` →
    /// `class_name_of_id` to round-trip, which this satisfies.
    fn class_object(ctx: &mut MockNativeContext, class_name: &str) -> Value {
        let cid = ctx
            .ensure_class_initialized(class_name)
            .expect("class init");
        Value::Object(Some(ctx.alloc_object(cid, 0)))
    }

    #[test]
    fn test_image_info_default_off_is_not_in_image() {
        let _guard = graalvm_test_lock();
        reset_graalvm_globals();
        let mut ctx = MockNativeContext::new();
        // Default mode: not a native image (CratonVM is a normal JVM).
        assert_eq!(
            graalvm_in_image_code(&mut ctx, &[]).unwrap(),
            Some(Value::Int(0))
        );
        assert_eq!(
            graalvm_in_image_buildtime_code(&mut ctx, &[]).unwrap(),
            Some(Value::Int(0))
        );
        assert_eq!(
            graalvm_in_image_runtime_code(&mut ctx, &[]).unwrap(),
            Some(Value::Int(0))
        );
        assert_eq!(
            graalvm_is_executable(&mut ctx, &[]).unwrap(),
            Some(Value::Int(0))
        );
        reset_graalvm_globals();
    }

    #[test]
    fn test_image_info_runtime_mode_flag() {
        let _guard = graalvm_test_lock();
        reset_graalvm_globals();
        set_image_code_mode(ImageCodeMode::Runtime);
        let mut ctx = MockNativeContext::new();
        assert_eq!(
            graalvm_in_image_code(&mut ctx, &[]).unwrap(),
            Some(Value::Int(1))
        );
        assert_eq!(
            graalvm_in_image_runtime_code(&mut ctx, &[]).unwrap(),
            Some(Value::Int(1))
        );
        assert_eq!(
            graalvm_in_image_buildtime_code(&mut ctx, &[]).unwrap(),
            Some(Value::Int(0))
        );
        // A running generated image is the executable form.
        assert_eq!(
            graalvm_is_executable(&mut ctx, &[]).unwrap(),
            Some(Value::Int(1))
        );
        assert_eq!(
            graalvm_is_shared_library(&mut ctx, &[]).unwrap(),
            Some(Value::Int(0))
        );
        reset_graalvm_globals();
    }

    #[test]
    fn test_image_info_buildtime_mode_flag() {
        let _guard = graalvm_test_lock();
        reset_graalvm_globals();
        set_image_code_mode(ImageCodeMode::Buildtime);
        let mut ctx = MockNativeContext::new();
        assert_eq!(
            graalvm_in_image_code(&mut ctx, &[]).unwrap(),
            Some(Value::Int(1))
        );
        assert_eq!(
            graalvm_in_image_buildtime_code(&mut ctx, &[]).unwrap(),
            Some(Value::Int(1))
        );
        assert_eq!(
            graalvm_in_image_runtime_code(&mut ctx, &[]).unwrap(),
            Some(Value::Int(0))
        );
        // Build-time image generation is not the executable.
        assert_eq!(
            graalvm_is_executable(&mut ctx, &[]).unwrap(),
            Some(Value::Int(0))
        );
        reset_graalvm_globals();
    }

    #[test]
    fn test_image_info_system_property_overrides_flag() {
        let _guard = graalvm_test_lock();
        reset_graalvm_globals();
        // Flag says Off, but the running program set the GraalVM property to
        // "runtime" — the property must win (matches GraalVM's contract).
        set_image_code_mode(ImageCodeMode::Off);
        let mut ctx = MockNativeContext::new();
        ctx.set_system_property("org.graalvm.nativeimage.imagecode", "runtime");
        assert_eq!(
            graalvm_in_image_code(&mut ctx, &[]).unwrap(),
            Some(Value::Int(1))
        );
        assert_eq!(
            graalvm_in_image_runtime_code(&mut ctx, &[]).unwrap(),
            Some(Value::Int(1))
        );
        reset_graalvm_globals();
    }

    #[test]
    fn test_image_code_mode_property_value() {
        assert_eq!(ImageCodeMode::Off.property_value(), None);
        assert_eq!(ImageCodeMode::Buildtime.property_value(), Some("buildtime"));
        assert_eq!(ImageCodeMode::Runtime.property_value(), Some("runtime"));
    }

    // -----------------------------------------------------------------------
    // ImageSingletons tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_image_singletons_add_contains_lookup_roundtrip() {
        let _guard = graalvm_test_lock();
        reset_graalvm_globals();
        init_graalvm_metadata();
        let mut ctx = MockNativeContext::new();

        let key = class_object(&mut ctx, "com/example/MyServiceKey");
        // Value: any heap object standing in for the singleton instance.
        let value_cid = ctx
            .ensure_class_initialized("com/example/MyServiceImpl")
            .unwrap();
        let value = Value::Object(Some(ctx.alloc_object(value_cid, 2)));

        // Not present before add.
        assert_eq!(
            graalvm_singletons_contains(&mut ctx, std::slice::from_ref(&key)).unwrap(),
            Some(Value::Int(0))
        );
        assert_eq!(
            graalvm_singletons_lookup(&mut ctx, std::slice::from_ref(&key)).unwrap(),
            Some(Value::Object(None))
        );

        // add(key, value)
        graalvm_singletons_add(&mut ctx, &[key, value]).unwrap();
        assert_eq!(graalvm_singleton_count(), 1);
        assert!(graalvm_singleton_contains("com/example/MyServiceKey"));

        // contains(key) → true
        assert_eq!(
            graalvm_singletons_contains(&mut ctx, std::slice::from_ref(&key)).unwrap(),
            Some(Value::Int(1))
        );

        // lookup(key) → the same object we stored.
        let looked_up = graalvm_singletons_lookup(&mut ctx, std::slice::from_ref(&key)).unwrap();
        assert_eq!(looked_up, Some(value));
        reset_graalvm_globals();
    }

    #[test]
    fn test_image_singletons_lookup_absent_returns_null() {
        let _guard = graalvm_test_lock();
        reset_graalvm_globals();
        init_graalvm_metadata();
        let mut ctx = MockNativeContext::new();
        let key = class_object(&mut ctx, "com/example/Absent");
        assert_eq!(
            graalvm_singletons_lookup(&mut ctx, std::slice::from_ref(&key)).unwrap(),
            Some(Value::Object(None))
        );
        reset_graalvm_globals();
    }

    #[test]
    fn test_image_singletons_distinct_keys() {
        let _guard = graalvm_test_lock();
        reset_graalvm_globals();
        init_graalvm_metadata();
        let mut ctx = MockNativeContext::new();

        let key_a = class_object(&mut ctx, "com/example/KeyA");
        let key_b = class_object(&mut ctx, "com/example/KeyB");
        let val_cid = ctx.ensure_class_initialized("com/example/ValA").unwrap();
        let val_a = Value::Object(Some(ctx.alloc_object(val_cid, 1)));

        graalvm_singletons_add(&mut ctx, &[key_a, val_a]).unwrap();
        assert!(graalvm_singleton_contains("com/example/KeyA"));
        assert!(!graalvm_singleton_contains("com/example/KeyB"));
        assert_eq!(
            graalvm_singletons_contains(&mut ctx, std::slice::from_ref(&key_b)).unwrap(),
            Some(Value::Int(0))
        );
        reset_graalvm_globals();
    }

    #[test]
    fn test_image_singletons_helpers_uninitialized() {
        let _guard = graalvm_test_lock();
        reset_graalvm_globals();
        // No init_graalvm_metadata(): registry is None → permissive defaults.
        assert!(!graalvm_singleton_contains("anything"));
        assert_eq!(graalvm_singleton_count(), 0);
        reset_graalvm_globals();
    }

    #[test]
    fn test_register_image_singletons_natives() {
        let r = make_registry();
        const SINGLETONS: &str = "org/graalvm/nativeimage/ImageSingletons";
        assert!(r
            .find(SINGLETONS, "contains", "(Ljava/lang/Class;)Z")
            .is_some());
        assert!(r
            .find(
                SINGLETONS,
                "lookup",
                "(Ljava/lang/Class;)Ljava/lang/Object;"
            )
            .is_some());
        assert!(r
            .find(SINGLETONS, "add", "(Ljava/lang/Class;Ljava/lang/Object;)V")
            .is_some());
    }
}
