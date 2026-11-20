// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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

pub fn register_graalvm_compat_natives(r: &mut NativeMethodRegistry) {
            // Initialize global metadata on first registration
    init_graalvm_metadata();

    // --- org/graalvm/nativeimage/ImageInfo ---
    r.register(
        "org/graalvm/nativeimage/ImageInfo",
        "inImageCode",
        "()Z",
        graalvm_in_image_code, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        "org/graalvm/nativeimage/ImageInfo",
        "inImageBuildtimeCode",
        "()Z",
        graalvm_in_image_buildtime_code, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        "org/graalvm/nativeimage/ImageInfo",
        "inImageRuntimeCode",
        "()Z",
        graalvm_in_image_runtime_code, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        "org/graalvm/nativeimage/ImageInfo",
        "isExecutable",
        "()Z",
        graalvm_is_executable, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        "org/graalvm/nativeimage/ImageInfo",
        "isSharedLibrary",
        "()Z",
        graalvm_is_shared_library, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // --- org/graalvm/nativeimage/RuntimeReflection ---
    r.register(
        "org/graalvm/nativeimage/RuntimeReflection",
        "register",
        "(Ljava/lang/Class;)V",
        graalvm_runtime_reflection_register, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        "org/graalvm/nativeimage/RuntimeReflection",
        "registerForReflectiveInstantiation",
        "(Ljava/lang/Class;)V",
        graalvm_runtime_reflection_register_instantiation, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // --- org/graalvm/nativeimage/RuntimeSerialization ---
    r.register(
        "org/graalvm/nativeimage/RuntimeSerialization",
        "register",
        "(Ljava/lang/Class;)V",
        graalvm_runtime_serialization_register, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // --- org/graalvm/nativeimage/RuntimeJNIAccess ---
    r.register(
        "org/graalvm/nativeimage/RuntimeJNIAccess",
        "register",
        "(Ljava/lang/Class;)V",
        graalvm_runtime_jni_register, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // --- org/graalvm/nativeimage/Platform ---
    r.register(
        "org/graalvm/nativeimage/Platform",
        "includedIn",
        "(Ljava/lang/Class;)Z",
        graalvm_platform_included_in, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // --- org/graalvm/nativeimage/hosted/Feature ---
    r.register(
        "org/graalvm/nativeimage/hosted/Feature",
        "register",
        "(Ljava/lang/Class;)V",
        graalvm_feature_register, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // --- org/graalvm/nativeimage/ImageSingletons ---
    r.register(
        "org/graalvm/nativeimage/ImageSingletons",
        "contains",
        "(Ljava/lang/Class;)Z",
        graalvm_singletons_contains, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        "org/graalvm/nativeimage/ImageSingletons",
        "lookup",
        "(Ljava/lang/Class;)Ljava/lang/Object;",
        graalvm_singletons_lookup, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        "org/graalvm/nativeimage/ImageSingletons",
        "add",
        "(Ljava/lang/Class;Ljava/lang/Object;)V",
        graalvm_singletons_add, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // --- CratonVM extension: metadata dump ---
    r.register(
        "cratonvm/graalvm/MetadataAgent",
        "dumpConfigs",
        "(Ljava/lang/String;)I",
        graalvm_dump_metadata, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    }

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests;
