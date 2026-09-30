// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T19.2.b — WildFly Naming (JNDI) native glue.
//!
//! WildFly / Keycloak 16 use JNDI heavily:
//!
//! * the datasource subsystem publishes `java:jboss/datasources/KeycloakDS`
//!   so CDI beans in the WAR can inject a `DataSource` via `@Resource`,
//! * the CDI bean registry exposes each managed bean under
//!   `java:comp/env/...`,
//! * each WildFly subsystem registers its own resources (transaction
//!   manager, ORB, security domain) through the same store.
//!
//! Upstream WildFly maps every JNDI binding onto an MSC
//! [`ServiceController`](crate::jboss_msc) so the naming graph and the
//! service graph share a single source of truth — a binder-service
//! `Up` transition is what actually makes the binding visible to
//! `Context.lookup`.  Our native replicates that shape:
//!
//! * One `HashMap<JndiName, BindingEntry>` per VM for the bindings
//!   themselves; the bound values are that VM's GC roots (gc-common w10-d).
//! * Every `bind()` also calls
//!   [`container_for(vm).add_service(...)`](crate::jboss_msc::container_for)
//!   so the VM's MSC graph sees the binder, which lets dependents on a
//!   binder-service start when the binding goes live.
//! * Hierarchical names (`java:jboss/datasources/KeycloakDS`) split on
//!   `/`; missing intermediate subcontexts throw `NameNotFoundException`.
//!
//! # Security
//!
//! JNDI injection is the Log4Shell-class attack surface.  This module
//! closes it at the native boundary with a hard-coded allowlist — the
//! only accepted absolute prefixes are `java:`, `java:jboss/`, and
//! `java:comp/`.  The URL-scheme prefixes `ldap:`, `rmi:`, `dns:`,
//! `iiop:`, and `corbaname:` are rejected with
//! `InvalidNameException`; this is enforced on every `bind`,
//! `rebind`, `lookup`, `unbind`, `createSubcontext`,
//! `destroySubcontext`, and `listBindings` call, so no codepath can
//! bypass the check by round-tripping through a different verb.
//!
//! # Panic safety
//!
//! A `bind()` hook that panics while registering a binder-service will
//! land in the MSC worker pool's `catch_unwind` (see `jboss_msc`), which
//! marks the binder `Failed`; we surface that back to the caller as a
//! `NamingException` via [`record_binder_failure`] so the JDK code never
//! observes a half-registered binding.

#![allow(clippy::needless_pass_by_value)]

use std::collections::HashMap;
use std::sync::Arc;

use cratonvm_native_api::vm_scoped::VmScoped;
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult};
use cratonvm_types::{ObjectRef, Value};

use crate::jboss_msc::{alloc_java_service_name, Mode, ServiceName};
use crate::{obj_arg, try_alloc_concurrent_synthetic};

// ===========================================================================
// JNDI-name types + allowlist
// ===========================================================================

/// Canonical JNDI name — `Arc<str>` so many `BindingEntry`s can share the
/// same backing allocation and name comparisons collapse to pointer
/// equality when the same name is interned twice.
pub type JndiName = Arc<str>;

/// Security-allowlist check.  Returns `Ok(())` iff the supplied JNDI
/// name is acceptable under our JNDI-injection hardening.
///
/// Accepted prefixes:
///
/// * `java:` — the JDK default naming root.
/// * `java:jboss/` — WildFly subsystem bindings (datasources,
///   transaction managers, security domains).
/// * `java:comp/` — CDI / JEE component namespace.
/// * `java:global/`, `java:app/`, `java:module/` — JEE 7+ portable
///   namespaces, included for completeness; rarely used by Keycloak but
///   required by the spec for full Context conformance.
///
/// Explicitly rejected (CVE-class JNDI injection vectors):
///
/// * `ldap:`, `ldaps:` — LDAP factories; core Log4Shell vector.
/// * `rmi:` — JNDI/RMI bridge; a second Log4Shell vector.
/// * `dns:`, `iiop:`, `corbaname:` — less-common remote factories we
///   reject for defence-in-depth.
///
/// Any other URL-style `<scheme>:` prefix is rejected as untrusted.
pub fn validate_jndi_name(name: &str) -> Result<(), String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("InvalidNameException: empty JNDI name".to_string());
    }

    // Reject the known-malicious schemes even if someone tried to slip
    // one in through a non-absolute name (e.g. by embedding `ldap://...`
    // inside what looks like a relative name).
    let lower = trimmed.to_ascii_lowercase();
    const REJECTED_SCHEMES: &[&str] = &[
        "ldap:",
        "ldaps:",
        "rmi:",
        "dns:",
        "iiop:",
        "corbaname:",
        "http:",
        "https:",
        "file:",
        "ftp:",
        "jar:",
    ];
    for bad in REJECTED_SCHEMES {
        if lower.starts_with(bad) {
            return Err(format!(
                "InvalidNameException: rejected URL scheme {bad} — JNDI injection guard"
            ));
        }
    }

    // Absolute JNDI names must live under one of the allowlisted prefixes.
    // Relative names (no colon before the first slash) are permitted — they
    // bind under the implicit `java:` root.
    if let Some(colon_idx) = trimmed.find(':') {
        // There IS a scheme prefix. Verify it's one of the JNDI ones.
        let scheme = &trimmed[..=colon_idx]; // includes trailing ':'
        const ACCEPTED_SCHEMES: &[&str] = &["java:"];
        if !ACCEPTED_SCHEMES.contains(&scheme) {
            return Err(format!(
                "InvalidNameException: unknown JNDI scheme {scheme}"
            ));
        }
        // After `java:` we accept either nothing, or one of the four
        // canonical sub-roots.
        let rest = &trimmed[colon_idx + 1..];
        const ACCEPTED_ROOTS: &[&str] = &["", "jboss/", "comp/", "global/", "app/", "module/"];
        let root_ok = ACCEPTED_ROOTS.iter().any(|r| rest.starts_with(r));
        if !root_ok {
            return Err(format!(
                "InvalidNameException: absolute name `{trimmed}` not under \
                 any allowlisted sub-root (java:jboss/, java:comp/, \
                 java:global/, java:app/, java:module/)"
            ));
        }
    }

    Ok(())
}

/// Canonicalise a JNDI name: trim, intern.  Assumes
/// [`validate_jndi_name`] has already passed.
pub fn canonical_jndi_name(raw: &str) -> JndiName {
    Arc::<str>::from(raw.trim())
}

// ===========================================================================
// BindInfo + ContextNames
// ===========================================================================

/// The triple `(parent MSC ServiceName, binder MSC ServiceName,
/// stripped JNDI name, original absolute name)` returned by
/// [`context_names_bind_info_for`].
///
/// WildFly uses this when wiring up a new datasource so the same call
/// site has both the MSC service it must register under and the JNDI
/// name it must bind.
#[derive(Debug, Clone)]
pub struct BindInfo {
    pub parent_context_service_name: Arc<ServiceName>,
    pub binder_service_name: Arc<ServiceName>,
    pub binding_name: JndiName,
    pub absolute_name: JndiName,
}

/// Root service name `"java"` — MSC's convention for naming-subsystem
/// services is `java.<rest>` (see `org.jboss.as.naming.ContextNames` in
/// the upstream codebase).
pub fn java_context_service_name() -> Arc<ServiceName> {
    ServiceName::of(["java"])
}

fn split_context_parent(after_java_prefix: &str) -> (&'static [&'static str], &str) {
    let contexts: [(&str, &[&str]); 7] = [
        ("jboss/exported", &["java", "jboss", "exported"]),
        ("jboss", &["java", "jboss"]),
        ("app", &["java", "app"]),
        ("module", &["java", "module"]),
        ("comp", &["java", "comp"]),
        ("global", &["java", "global"]),
        ("", &["java"]),
    ];

    for (prefix, parent) in contexts {
        if prefix.is_empty() {
            return (parent, after_java_prefix);
        }
        if after_java_prefix == prefix {
            return (parent, "");
        }
        if let Some(rest) = after_java_prefix.strip_prefix(prefix) {
            if let Some(rest) = rest.strip_prefix('/') {
                return (parent, rest);
            }
        }
    }

    unreachable!("empty prefix fallback must match");
}

fn append_binding_name(parent: &Arc<ServiceName>, bind_name: &str) -> Arc<ServiceName> {
    let mut name = parent.clone();
    for segment in bind_name.split('/') {
        if !segment.is_empty() {
            name = name.append(segment);
        }
    }
    name
}

fn has_known_context_prefix(name: &str) -> bool {
    name.starts_with('/')
        || ["jboss", "global", "app", "module", "comp"]
            .iter()
            .any(|prefix| name == *prefix || name.starts_with(&format!("{prefix}/")))
}

/// Parse `java:jboss/datasources/KeycloakDS` →
///
/// * parent service name `java.jboss`,
/// * binder service name `java.jboss.datasources.KeycloakDS`,
/// * stripped binding name `datasources/KeycloakDS`.
pub fn context_names_bind_info_for(absolute: &str) -> Result<BindInfo, String> {
    validate_jndi_name(absolute)?;
    let trimmed = absolute.trim();
    let mut after = if let Some(rest) = trimmed.strip_prefix("java:") {
        rest
    } else if has_known_context_prefix(trimmed) {
        trimmed
    } else {
        // Mirrors ContextNames.bindInfoFor(String): unqualified names are
        // exported below java:jboss/exported/.
        return context_names_bind_info_for(&format!("java:jboss/exported/{trimmed}"));
    };

    if after.starts_with("/exported/") {
        after = &after[1..];
    }

    let (parent_segments, bind_name) = split_context_parent(after);
    let parent_context_service_name = ServiceName::of(parent_segments.iter().copied());
    let binder_service_name = append_binding_name(&parent_context_service_name, bind_name);
    Ok(BindInfo {
        parent_context_service_name,
        binder_service_name,
        binding_name: canonical_jndi_name(bind_name),
        absolute_name: canonical_jndi_name(trimmed),
    })
}

// ===========================================================================
// BindingEntry + ServiceBasedNamingStore
// ===========================================================================

/// A single resolved JNDI binding — what `Context.lookup` eventually
/// returns to the caller.
#[derive(Debug, Clone)]
pub struct BindingEntry {
    /// MSC binder service that owns this binding's lifecycle.
    pub service_name: Arc<ServiceName>,
    /// Java-facing class name — used by `NameClassPair.getClassName()`.
    pub class_name: Arc<str>,
    /// The object returned by `lookup`.  `None` means the binder hasn't
    /// produced a value yet (binder still `Down` / `Starting`).
    pub value: Option<ObjectRef>,
    /// Set to true if the binder service failed during start — lookups
    /// then throw `NamingException` rather than returning a stale value.
    pub failed: bool,
    /// RefAddr-style URL escape hatch.  When present this is the raw
    /// URL that a classic JDK JNDI impl would open; we REFUSE to follow
    /// the URL and instead return `NameNotFoundException`.  Present
    /// purely so tests can verify the refusal path.
    pub url_reference: Option<Arc<str>>,
}

impl BindingEntry {
    pub fn new(service_name: Arc<ServiceName>, class_name: &str, value: ObjectRef) -> Self {
        Self {
            service_name,
            class_name: Arc::<str>::from(class_name),
            value: Some(value),
            failed: false,
            url_reference: None,
        }
    }
}

/// Each VM's binding map (gc-common w10-d).
///
/// It used to be one process-wide `RwLock<HashMap>` of RAW, UNROOTED
/// `ObjectRef`s: a moving collection stranded every bound value, a young
/// collection could free one only the store held, and VM B's `lookup`
/// answered VM A's object
/// (`common-w9b-jndi-bindings-store-holds-unrooted-object-refs`). The values
/// are now the VM's `wildfly-side-tables` root row
/// ([`gc_scan_jndi_binding_roots`] / [`gc_update_jndi_binding_refs`]), and a
/// torn-down VM's row is dropped by [`forget_vm_jndi_bindings`].
///
/// The map also doubles as the listing backend for `listBindings`: we
/// iterate every key whose stripped path lives under a supplied parent
/// name. `VmScoped` rules apply: never allocate or call into Java while a
/// row is borrowed.
static BINDINGS: VmScoped<HashMap<JndiName, BindingEntry>> = VmScoped::new();

/// GC root scan for VM `vm`'s bound values. Companion remap is
/// [`gc_update_jndi_binding_refs`]. Bindings are strong: a bound object lives
/// until it is unbound, as it does on HotSpot.
pub fn gc_scan_jndi_binding_roots(vm: usize, out: &mut Vec<ObjectRef>) {
    BINDINGS.peek(vm, |store| {
        out.extend(store.values().filter_map(|e| e.value))
    });
}

/// Post-move remap of VM `vm`'s bound values (companion to
/// [`gc_scan_jndi_binding_roots`]).
pub fn gc_update_jndi_binding_refs(vm: usize, pointer_map: &cratonvm_types::PointerMap) {
    if pointer_map.is_empty() || !BINDINGS.has_row(vm) {
        return;
    }
    BINDINGS.with(vm, |store| {
        for value in store.values_mut().filter_map(|e| e.value.as_mut()) {
            if let Some(&new_addr) = pointer_map.get(&(value.as_ptr() as usize)) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                // SAFETY: the collector's own relocation of this live object.
                *value = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    });
}

/// Per-VM teardown: drop VM `vm`'s bindings. Idempotent.
pub fn forget_vm_jndi_bindings(vm: usize) {
    BINDINGS.forget(vm);
}

/// High-level `bind` — accepts a raw absolute name, validates it,
/// registers a binder service with VM `vm`'s MSC container, and stores
/// the binding in VM `vm`'s store.
///
/// `value` must be a current address: no allocation may run between the
/// caller reading it and this call (nothing here allocates).
pub fn bind_value(vm: usize, name: &str, class_name: &str, value: ObjectRef) -> Result<(), String> {
    let info = context_names_bind_info_for(name)?;
    let entry = BindingEntry::new(info.binder_service_name.clone(), class_name, value);

    // Register the binder service so T19.1's MSC graph sees it. A binder
    // has no Java `Service`, so its payload is 0: the value itself is rooted
    // by this store, never by the MSC side (whose payload is an opaque,
    // never-dereferenced tag that a moving collection would leave stale).
    let container = crate::jboss_msc::container_for(vm);
    if let Ok(id) = container.add_service(
        info.binder_service_name.clone(),
        Vec::new(),
        Mode::Active,
        0,
    ) {
        // Bring the binder to Up synchronously — the JNDI contract is that
        // `bind` returns once the binding is visible. Only the binder: the
        // `drain_tasks_locally` this replaces (gc-common w10-d) also marked
        // every other start-ready service `Up` without running its real
        // `start()`, whenever a bind happened during an MSC boot.
        container.start_without_callback(id);
    }
    // `add_service` fails on a dependency cycle, which a binder with no
    // dependencies cannot form, and on a name whose binder is still
    // installed (a second `bind` or a `rebind`, gc-common w12-d). That binder
    // is already `Up`, and the value below replaces the old one, so the error
    // is ignored on purpose.

    BINDINGS.with(vm, |store| {
        store.insert(info.absolute_name.clone(), entry);
    });
    Ok(())
}

/// `rebind` — overwrite any existing entry.  Also re-asserts the MSC
/// binder service.
pub fn rebind_value(
    vm: usize,
    name: &str,
    class_name: &str,
    value: ObjectRef,
) -> Result<(), String> {
    let info = context_names_bind_info_for(name)?;
    BINDINGS.with(vm, |store| {
        store.remove(&info.absolute_name);
    });
    bind_value(vm, name, class_name, value)
}

/// `unbind` — remove the entry (if present).  Missing entries are a
/// no-op per the JNDI spec.
pub fn unbind_value(vm: usize, name: &str) -> Result<(), String> {
    let info = context_names_bind_info_for(name)?;
    BINDINGS.with(vm, |store| {
        store.remove(&info.absolute_name);
    });
    Ok(())
}

/// `lookup` — resolve a name.  Walks the hierarchical name by walking
/// the key space: an absolute lookup is a single HashMap hit; lookups
/// with missing intermediate subcontexts would hit the error branch.
///
/// Rejects any entry with a `url_reference` (see [`BindingEntry`]).
pub fn lookup_value(vm: usize, name: &str) -> Result<ObjectRef, String> {
    let info = context_names_bind_info_for(name)?;
    let not_bound = || format!("NameNotFoundException: `{}` not bound", info.absolute_name);
    let entry = BINDINGS
        .peek(vm, |store| store.get(&info.absolute_name).cloned())
        .flatten()
        .ok_or_else(not_bound)?;
    if entry.failed {
        return Err(format!(
            "NamingException: binder service for `{}` failed during start",
            info.absolute_name
        ));
    }
    if entry.url_reference.is_some() {
        // URL refs are CVE territory — refuse, even though a real JDK
        // JNDI would have opened the network connection.
        return Err(format!(
            "NameNotFoundException: `{}` resolves to a URL reference (refused by JNDI \
             injection guard)",
            info.absolute_name
        ));
    }
    entry.value.ok_or_else(|| {
        format!(
            "NamingException: `{}` exists but has no value (binder still starting)",
            info.absolute_name
        )
    })
}

/// Return every direct child of the supplied parent JNDI prefix.  A
/// prefix of `java:jboss/datasources` with bindings
/// `java:jboss/datasources/KeycloakDS` and `java:jboss/datasources/Meta`
/// returns both.
///
/// The returned values are current only until the caller's next
/// allocation: pin them before allocating (see `native_context_list_bindings`).
pub fn list_bindings(vm: usize, parent: &str) -> Result<Vec<(JndiName, BindingEntry)>, String> {
    validate_jndi_name(parent)?;
    let canon = canonical_jndi_name(parent);
    let prefix: String = {
        // Ensure we only match immediate children: canonical prefix plus a '/'
        let mut s = canon.to_string();
        if !s.ends_with('/') && !s.ends_with(':') {
            s.push('/');
        }
        s
    };
    let out = BINDINGS
        .peek(vm, |store| {
            store
                .iter()
                .filter(|(k, _)| {
                    k.strip_prefix(prefix.as_str())
                        .is_some_and(|rest| !rest.contains('/'))
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    Ok(out)
}

/// Create a subcontext — in our flat model this is a no-op so long as
/// the name validates.  Real WildFly also wires a placeholder
/// `Context` service into MSC; our test surface does not exercise
/// that, so we simply succeed.
pub fn create_subcontext(name: &str) -> Result<(), String> {
    validate_jndi_name(name)?;
    Ok(())
}

/// Destroy a subcontext — remove every binding whose canonical name
/// starts with the subcontext path.
pub fn destroy_subcontext(vm: usize, name: &str) -> Result<(), String> {
    validate_jndi_name(name)?;
    let canon = canonical_jndi_name(name);
    let prefix = format!("{canon}/");
    BINDINGS.with(vm, |store| {
        store.retain(|k, _| !(k.as_ref() == canon.as_ref() || k.starts_with(&prefix)));
    });
    Ok(())
}

/// Mark a binder as failed (called from the MSC catch_unwind path when
/// a `start()` callback panics).  Subsequent lookups will throw
/// `NamingException`.
#[allow(dead_code)]
pub fn record_binder_failure(vm: usize, name: &str, _reason: &str) {
    let info = match context_names_bind_info_for(name) {
        Ok(i) => i,
        Err(_) => return,
    };
    BINDINGS.with(vm, |store| {
        if let Some(entry) = store.get_mut(&info.absolute_name) {
            entry.failed = true;
        }
    });
}

// ===========================================================================
// Java ↔ Rust glue layer
// ===========================================================================

// --- Field offsets (matched to class_manager's synthetic_stub_fields) ---
const INIT_CTX_FIELD_ENV: usize = 0;
#[allow(dead_code)]
const INIT_CTX_FIELD_DEFAULT: usize = 1;
const INIT_CTX_NUM_SLOTS: usize = 2;

const BINDING_FIELD_NAME: usize = 0;
const BINDING_FIELD_CLASS_NAME: usize = 1;
const BINDING_FIELD_OBJECT: usize = 2;
const BINDING_NUM_SLOTS: usize = 3;

#[allow(dead_code)]
const NAMING_STORE_FIELD_BINDINGS: usize = 0;
#[allow(dead_code)]
const NAMING_STORE_FIELD_SERVICE_BASE: usize = 1;
const NAMING_STORE_NUM_SLOTS: usize = 2;

const BIND_INFO_FIELD_PARENT: &str = "parentContextServiceName";
const BIND_INFO_FIELD_BINDER: &str = "binderServiceName";
const BIND_INFO_FIELD_BIND_NAME: &str = "bindName";
const BIND_INFO_FIELD_ABSOLUTE: &str = "absoluteJndiName";
const BIND_INFO_NUM_SLOTS: usize = 4;

/// Build a *real, catchable* `javax.naming.*` exception object and wrap it in
/// `MethodCallFailed::ExceptionThrown`.
///
/// This is the load-bearing correctness fix. The previous implementation
/// raised `RuntimeError::NotImplemented`, which the interpreter deliberately
/// treats as a **fatal, non-catchable** VM error. A JNDI miss is a perfectly
/// ordinary, *catchable* `NameNotFoundException` — application code (Apache
/// Tomcat's `NamingContextListener`, WebappClassLoader, etc.) routinely does
/// `try { ctx.lookup(...) } catch (NamingException e) { … }`. Raising
/// `NotImplemented` turned that recoverable miss into an immediate VM abort
/// (`runtime error: not implemented: NameNotFoundException: …` — observed as
/// Tomcat's `java:/ not bound` boot failure).
///
/// `alloc_concurrent_synthetic` resolves the genuine `javax/naming/*` class
/// so the thrown object carries the real `ClassId`; `catch` clauses for
/// `NameNotFoundException`, `NamingException`, or any superclass match it
/// correctly. The detail message is written through `detailMessage` by name
/// (real-JDK `Throwable` layout) with a slot-1 fallback for the
/// synthetic-stub layout.
fn throw_naming(
    ctx: &mut dyn NativeContext,
    exc_class: &str,
    msg: &str,
) -> Result<MethodCallFailed, MethodCallFailed> {
    // `NamingException` and subclasses extend `Throwable` (message, cause,
    // and JNDI-specific `resolvedName` / `remainingName` / `rootException`
    // / `resolvedObj`). Reserve a generous slot count; the real class load
    // will pad to its true field count anyway.
    let exc = try_alloc_concurrent_synthetic(ctx, exc_class, 6)?;
    // GC-SAFETY (gc-common w10-d): `create_string` can move `exc`.
    let exc_pin = ctx.pin_native_root(exc);
    let msg_str = ctx.create_string(msg);
    let exc = ctx.read_native_pin(exc_pin, exc);
    ctx.unpin_native_roots(exc_pin);
    // Real-JDK Throwable stores the message in `detailMessage`. Writing by
    // name resolves the slot through the class hierarchy.
    ctx.set_field_by_name(exc, "detailMessage", Value::Object(Some(msg_str)));
    Ok(MethodCallFailed::ExceptionThrown(exc))
}

fn throw_name_not_found(
    ctx: &mut dyn NativeContext,
    msg: &str,
) -> Result<MethodCallFailed, MethodCallFailed> {
    Ok(throw_naming(
        ctx,
        "javax/naming/NameNotFoundException",
        msg,
    )?)
}

fn throw_no_initial_context(
    ctx: &mut dyn NativeContext,
    msg: &str,
) -> Result<MethodCallFailed, MethodCallFailed> {
    Ok(throw_naming(
        ctx,
        "javax/naming/NoInitialContextException",
        msg,
    )?)
}

fn throw_naming_exception(
    ctx: &mut dyn NativeContext,
    msg: &str,
) -> Result<MethodCallFailed, MethodCallFailed> {
    Ok(throw_naming(ctx, "javax/naming/NamingException", msg)?)
}

fn throw_invalid_name(
    ctx: &mut dyn NativeContext,
    msg: &str,
) -> Result<MethodCallFailed, MethodCallFailed> {
    Ok(throw_naming(ctx, "javax/naming/InvalidNameException", msg)?)
}

/// Map a `Result<_, String>` error message from the flat-store helpers
/// (`lookup_value`, `bind_value`, …) onto the matching catchable
/// `javax.naming.*` exception, keying off the message prefix the helper
/// produced.
fn flat_store_error(
    ctx: &mut dyn NativeContext,
    msg: &str,
) -> Result<MethodCallFailed, MethodCallFailed> {
    if msg.starts_with("InvalidNameException") {
        throw_invalid_name(ctx, msg)
    } else if msg.starts_with("NameNotFoundException") {
        throw_name_not_found(ctx, msg)
    } else {
        throw_naming_exception(ctx, msg)
    }
}

fn native_initial_context_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Field 0 = environment table (null until `addToEnvironment` is called),
    // field 1 = default-init-ctx handle (we stash 0 since the Rust side owns
    // the real state).
    ctx.set_field(this, INIT_CTX_FIELD_ENV, Value::Object(None));
    let _ = INIT_CTX_NUM_SLOTS;
    Ok(None)
}

fn read_string_arg(ctx: &dyn NativeContext, args: &[Value], idx: usize) -> Option<String> {
    match args.get(idx).copied() {
        Some(Value::Object(Some(s))) => ctx.read_string(s),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// `java:` URL-context-factory delegation
//
// The flat process-wide binding store above is the *WildFly / Keycloak*
// naming substitute — WildFly's naming subsystem never runs the JDK's stock
// `InitialContext` bytecode, so the natives own the `java:jboss/...` /
// `java:comp/...` namespace directly.
//
// Apache Tomcat is the opposite case: it ships its own
// `org.apache.naming.*` `Context` implementations and relies on the *stock*
// JDK `InitialContext` → `NamingManager.getURLContext("java", env)` →
// `org.apache.naming.java.javaURLContextFactory` dispatch chain. A name
// like `java:/comp/env` must be resolved against Tomcat's per-thread
// `org.apache.naming.ContextBindings`, NOT against our flat store (where it
// was never bound, since Tomcat binds through its own `NamingContext`).
//
// Because a registered native unconditionally shadows the real JDK
// bytecode, the native cannot simply "fall through" to `InitialContext`'s
// own body. Instead it reproduces the spec'd `getURLOrDefaultInitCtx`
// step: ask `NamingManager.getURLContext` for the `java:` URL context and,
// if a real one comes back, delegate the operation onto it via *virtual*
// dispatch. The returned context's concrete class is Tomcat's
// `SelectorContext` / `javaURLContext` (never `InitialContext`), so this
// does not recurse back into these natives.
//
// When `getURLContext` returns null / throws (no URL factory installed —
// e.g. a plain WildFly run, or a unit test), the caller falls back to the
// flat in-memory store, preserving the existing WildFly behaviour.

/// True iff `name` is an absolute JNDI name in the `java:` URL scheme.
fn is_java_url_scheme(name: &str) -> bool {
    name.trim_start().starts_with("java:")
}

fn is_plain_relative_name(name: &str) -> bool {
    !name.trim_start().contains(':')
}

fn has_initial_context_provider(ctx: &mut dyn NativeContext) -> bool {
    if matches!(
        ctx.get_system_property("java.naming.factory.initial"),
        Some(s) if !s.trim().is_empty()
    ) {
        return true;
    }
    if ctx
        .ensure_class_initialized("javax/naming/spi/NamingManager")
        .is_err()
    {
        return false;
    }
    matches!(
        ctx.invoke(
            "javax/naming/spi/NamingManager",
            "hasInitialContextFactoryBuilder",
            "()Z",
            &[],
        ),
        Ok(Some(Value::Int(v))) if v != 0
    )
}

/// Ask the JDK's `NamingManager` for the `java:` URL context.
///
/// Returns:
/// * `Ok(Some(ctx))` — a real `Context` was produced; the caller delegates
///   the JNDI operation onto it (this is the Apache Tomcat path);
/// * `Ok(None)` — no `java:` URL factory is installed, so the caller
///   should fall back to the flat in-memory store;
/// * `Err(_)` — a non-exception VM failure escaped.
///
/// The `Ok(None)` result is also the WildFly / Keycloak path: a stock JDK
/// has *no* `java:` URL context factory configured, so `getURLContext("java",
/// …)` returns `null` there and the `java:jboss/...` traffic keeps using the
/// flat store. The decision is therefore made by the JDK's own
/// `getURLContext` contract — exactly the spec'd `getURLOrDefaultInitCtx`
/// behaviour — rather than by guessing which app server is running.
///
/// IMPORTANT — `getURLContext` resolves the URL-context factory from the
/// `java.naming.factory.url.pkgs` (`Context.URL_PKG_PREFIXES`) value found in
/// the **environment `Hashtable`** (and `jndi.properties` resources), NOT from
/// the system property. (Verified against HotSpot: `getURLContext("java",
/// null)` and `("java", emptyEnv)` both return `null`; only `("java", env)`
/// with `url.pkgs` set in `env` yields the `SelectorContext`.) Real
/// `InitialContext` works because its constructor copies the system JNDI
/// properties into `myProps`; our native `<init>` stub leaves the env empty, so
/// we must materialise that environment here. [`url_pkgs_env`] does so from the
/// `java.naming.factory.url.pkgs` system property Tomcat's `enableNaming()`
/// sets — giving the spec-correct factory dispatch the input it needs. When no
/// such property is set (plain WildFly / Keycloak) the env is left as-is and
/// `getURLContext` returns `null`, preserving the flat-store path.
///
/// Takes the `InitialContext` as `(this_pin, this)` and reads its
/// environment only after its own allocations (gc-common w10-d): the env
/// read up front was stale by the time `getURLContext` received it.
fn java_url_context(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    // `NamingManager` is a stock `java.naming` class. If the module is not
    // present (it always is for a real-JDK run) treat the scheme as
    // unhandled and let the caller fall back to the flat store.
    if ctx
        .ensure_class_initialized("javax/naming/spi/NamingManager")
        .is_err()
    {
        return Ok(None);
    }
    let scheme = ctx.create_string("java");
    let scheme_pin = ctx.pin_native_root(scheme);
    let this = ctx.read_native_pin(this_pin, this);
    let env = initial_context_env(ctx, this);
    // Allocates (a `Hashtable` copy) and returns a current address.
    let env = url_pkgs_env(ctx, env);
    let scheme = ctx.read_native_pin(scheme_pin, scheme);
    ctx.unpin_native_roots(scheme_pin);
    let result = ctx.invoke(
        "javax/naming/spi/NamingManager",
        "getURLContext",
        "(Ljava/lang/String;Ljava/util/Hashtable;)Ljavax/naming/Context;",
        &[Value::Object(Some(scheme)), env],
    );
    match result {
        Ok(Some(Value::Object(Some(c)))) => Ok(Some(c)),
        Ok(_) => Ok(None),
        // A throwing `getURLContext` (e.g. factory class missing) is not
        // fatal here: fall back to the flat store rather than aborting the
        // whole lookup.
        Err(MethodCallFailed::ExceptionThrown(_)) => Ok(None),
        Err(other) => Err(other),
    }
}

/// Materialise the environment `Hashtable` that `NamingManager.getURLContext`
/// needs to resolve the `java:` URL-context factory.
///
/// `getURLContext` reads `Context.URL_PKG_PREFIXES`
/// (`java.naming.factory.url.pkgs`) from the supplied environment, NOT from the
/// system property. Tomcat's `enableNaming()` publishes that value as a *system*
/// property and relies on `InitialContext`'s constructor to copy it into the
/// per-instance environment — a step our native `<init>` stub skips. So, when
/// the system property is present, build a `Hashtable` carrying it (preferring
/// any value already present in `incoming`). Returns `incoming` unchanged when
/// the property is unset (plain WildFly / Keycloak) or on any allocation error,
/// so the caller's `getURLContext` then returns `null` and the flat store wins.
fn environment_with_property(
    ctx: &mut dyn NativeContext,
    incoming: Value,
    property: &str,
    value: &str,
) -> Value {
    // Preserve a caller supplied real-JDK environment.  In particular, an
    // InitialDirContext carries both the initial factory *and* the provider
    // URL in `myProps`; replacing it with a one-entry table lets
    // NamingManager select LdapCtxFactory but leaves that factory without its
    // LDAP endpoint.  Hashtable's Map constructor makes the same shallow copy
    // that InitialContext uses for its private properties table.
    // GC-SAFETY (gc-common w10-d): the failure paths below return `incoming`
    // after allocating; pin it so they return a current address.
    let incoming_pin = match incoming {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };
    let result = environment_with_property_pinned(ctx, incoming, incoming_pin, property, value);
    if let Some((pin, _)) = incoming_pin {
        ctx.unpin_native_roots(pin);
    }
    result
}

/// The current address of a value pinned as `pin` (`None` for a non-object
/// value, which is returned as is).
fn read_pinned_value(
    ctx: &dyn NativeContext,
    pin: Option<(usize, ObjectRef)>,
    fallback: Value,
) -> Value {
    match pin {
        Some((handle, obj)) => Value::Object(Some(ctx.read_native_pin(handle, obj))),
        None => fallback,
    }
}

/// The environment of the `InitialContext` pinned as `this_pin`, read from
/// its current address.
fn pinned_context_env(ctx: &dyn NativeContext, this_pin: usize, this: ObjectRef) -> Value {
    let this = ctx.read_native_pin(this_pin, this);
    initial_context_env(ctx, this)
}

fn environment_with_property_pinned(
    ctx: &mut dyn NativeContext,
    incoming: Value,
    incoming_pin: Option<(usize, ObjectRef)>,
    property: &str,
    value: &str,
) -> Value {
    let copied = match incoming {
        Value::Object(Some(_)) => ctx
            .new_object_initialized("java/util/Hashtable", "(Ljava/util/Map;)V", &[incoming])
            .ok()
            .flatten()
            .and_then(|value| match value {
                Value::Object(Some(object)) => Some(object),
                _ => None,
            })
            .or_else(
                || match ctx.new_object_initialized("java/util/Hashtable", "()V", &[]) {
                    Ok(Some(Value::Object(Some(object)))) => Some(object),
                    _ => None,
                },
            ),
        _ => match ctx.new_object_initialized("java/util/Hashtable", "()V", &[]) {
            Ok(Some(Value::Object(Some(object)))) => Some(object),
            _ => None,
        },
    };
    let ht = match copied {
        Some(object) => object,
        _ => return read_pinned_value(ctx, incoming_pin, incoming),
    };
    // GC-SAFETY: `ht` is used again after its own `put` dispatch (the final
    // return), and `key` is used again after `val`'s own `create_string`
    // call -- both are moving-GC hazards. Pin as each is produced and
    // re-read before each subsequent use.
    let ht_pin = ctx.pin_native_root(ht);
    let key = ctx.create_string(property);
    let key_pin = ctx.pin_native_root(key);
    let val = ctx.create_string(value);
    let ht = ctx.read_native_pin(ht_pin, ht);
    let key = ctx.read_native_pin(key_pin, key);
    if ctx
        .invoke_virtual(
            ht,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(key)), Value::Object(Some(val))],
        )
        .is_err()
    {
        ctx.unpin_native_roots(ht_pin);
        return read_pinned_value(ctx, incoming_pin, incoming);
    }
    let ht = ctx.read_native_pin(ht_pin, ht);
    ctx.unpin_native_roots(ht_pin);
    Value::Object(Some(ht))
}

/// The T3 fallback registered `getEnvironment` against a two-slot synthetic
/// layout. In real-JDK mode that registration reads a boolean/private JDK
/// field as a Hashtable, so Spring's `JndiLocatorDelegate` treats a configured
/// JNDI environment as unavailable. Materialise the system-configured initial
/// factory property exactly as InitialContext's constructor would have copied
/// it into `myProps`; without a provider, retain the JDK's
/// NoInitialContextException contract.
fn native_initial_context_get_environment(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Real JDK: `getEnvironment()` is `getDefaultInitCtx().getEnvironment()`,
    // so an installed `InitialContextFactoryBuilder` answers it. Same omission
    // the lookup/bind/rebind/unbind natives already fixed — see
    // `builder_initial_context`. Spring's
    // `JndiLocatorDelegate.isDefaultJndiEnvironmentAvailable()` is exactly
    // `new InitialContext().getEnvironment()` inside a try/catch, so throwing
    // here told Spring that JNDI is unavailable even right after
    // `SimpleNamingContextBuilder.emptyActivatedContextBuilder()` — and
    // `StandardServletEnvironment` then skipped its `jndiProperties` source
    // entirely (`web.context.support.StandardServletEnvironmentTests
    // .propertySourceOrder`).
    let this_pin = ctx.pin_native_root(this);
    let delegate = builder_initial_context(ctx, this_pin, this);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    if let Some(delegate) = delegate? {
        return ctx.invoke_virtual(delegate, "getEnvironment", "()Ljava/util/Hashtable;", &[]);
    }
    let incoming = initial_context_env(ctx, this);
    if matches!(incoming, Value::Object(Some(_))) {
        return Ok(Some(incoming));
    }
    let factory = match ctx.get_system_property("java.naming.factory.initial") {
        Some(value) if !value.trim().is_empty() => value,
        _ => {
            return Err(throw_no_initial_context(
                ctx,
                "Need to specify class name in environment or system property: java.naming.factory.initial",
            )?)
        }
    };
    let env = environment_with_property(ctx, incoming, "java.naming.factory.initial", &factory);
    Ok(Some(env))
}

fn url_pkgs_env(ctx: &mut dyn NativeContext, incoming: Value) -> Value {
    match ctx.get_system_property("java.naming.factory.url.pkgs") {
        Some(pkgs) if !pkgs.trim().is_empty() => {
            environment_with_property(ctx, incoming, "java.naming.factory.url.pkgs", &pkgs)
        }
        _ => incoming,
    }
}

/// Read the `InitialContext` environment table to forward to
/// `getURLContext`.
///
/// Slot 0 is the environment table *only* under our synthetic layout
/// (see `class_manager::synthetic_stub_fields`). When the real JDK
/// `javax/naming/InitialContext` class is loaded its field order differs
/// (`myProps` / `defaultInitCtx` / `gotDefault`), so reading slot 0 would
/// hand `getURLContext` an arbitrary object. `getURLContext` accepts a
/// null `Hashtable` and Tomcat's `javaURLContextFactory` does not consult
/// the environment, so we conservatively pass null whenever the class is
/// not our synthetic stub.
fn initial_context_env(ctx: &dyn NativeContext, this: ObjectRef) -> Value {
    let is_synthetic = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .map(|n| ctx.is_class_synthetic_stub(&n))
        .unwrap_or(false);
    if is_synthetic {
        ctx.get_field(this, INIT_CTX_FIELD_ENV)
    } else {
        // Real JDK InitialContext stores its caller-supplied Hashtable in
        // myProps.  Returning null here loses InitialDirContext's LDAP
        // factory setting before NamingManager can select LdapCtxFactory.
        ctx.get_field_by_name(this, "myProps")
    }
}

/// When a custom `InitialContextFactoryBuilder` has been installed in the JDK's
/// `NamingManager` (e.g. Spring's `SimpleNamingContextBuilder`, which
/// `JtaTransactionManager` serialization tests use to stub a JNDI environment),
/// the stock `InitialContext` bytecode resolves every name through the `Context`
/// that builder produces. Our WildFly/Tomcat-oriented natives otherwise shadow
/// that bytecode and consult the flat store, so a `lookup` of a builder-bound
/// name misses with `NameNotFoundException` (unlike HotSpot). Reproduce the
/// spec'd `getDefaultInitCtx()` step: if a builder is installed, ask
/// `NamingManager.getInitialContext(env)` for the initial `Context` and let the
/// caller delegate to it via virtual dispatch. Returns `None` when no builder is
/// installed, preserving the flat-store / `java:` URL-context defaults for
/// WildFly / Keycloak / Tomcat (none of which install an
/// `InitialContextFactoryBuilder`). The returned context's concrete class is the
/// builder's own (e.g. `SimpleNamingContext`), never `InitialContext`, so the
/// delegation does not recurse back into these natives.
///
/// `this` is passed as `(this_pin, this)` and re-read after the class
/// initialisation and the `hasInitialContextFactoryBuilder` call, either of
/// which can move it (gc-common w10-d).
fn builder_initial_context(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    if ctx
        .ensure_class_initialized("javax/naming/spi/NamingManager")
        .is_err()
    {
        return Ok(None);
    }
    let has_builder = matches!(
        ctx.invoke(
            "javax/naming/spi/NamingManager",
            "hasInitialContextFactoryBuilder",
            "()Z",
            &[],
        )?,
        Some(Value::Int(v)) if v != 0
    );
    if !has_builder {
        return Ok(None);
    }
    let this = ctx.read_native_pin(this_pin, this);
    let env = initial_context_env(ctx, this);
    match ctx.invoke(
        "javax/naming/spi/NamingManager",
        "getInitialContext",
        "(Ljava/util/Hashtable;)Ljavax/naming/Context;",
        &[env],
    )? {
        Some(Value::Object(Some(c))) => Ok(Some(c)),
        _ => Ok(None),
    }
}

/// Resolve a configured JNDI initial-context provider when no explicit
/// `InitialContextFactoryBuilder` is installed. Spring's test JNDI fixtures
/// use `Context.INITIAL_CONTEXT_FACTORY` / `java.naming.factory.initial`, not
/// a builder, so their bindings were previously bypassed by our native
/// `InitialContext.lookup` interception and incorrectly fell through to the
/// WildFly flat store. This is the normal `InitialContext.getDefaultInitCtx()`
/// path; the returned provider context owns the namespace and does not recurse
/// through this `InitialContext` native.
///
/// `this` is passed as `(this_pin, this)`: its environment is re-read after
/// the key string's allocation, which can move it (gc-common w10-d).
fn configured_initial_context(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let factory = match ctx.get_system_property("java.naming.factory.initial") {
        Some(value) if !value.trim().is_empty() => Some(value),
        _ => match pinned_context_env(ctx, this_pin, this) {
            Value::Object(Some(_)) => {
                let key = ctx.create_string("java.naming.factory.initial");
                let Value::Object(Some(env)) = pinned_context_env(ctx, this_pin, this) else {
                    return Ok(None);
                };
                match ctx.invoke_virtual(
                    env,
                    "get",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                    &[Value::Object(Some(key))],
                )? {
                    Some(Value::Object(Some(value))) => ctx.read_string(value),
                    _ => None,
                }
            }
            _ => None,
        },
    };
    let Some(factory) = factory.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    // The native InitialContext constructor does not execute the JDK body that
    // copies system JNDI properties into `myProps`. Give NamingManager the
    // equivalent explicit environment so it can instantiate the configured
    // factory instead of throwing NoInitialContextException.
    let incoming = pinned_context_env(ctx, this_pin, this);
    let env = environment_with_property(ctx, incoming, "java.naming.factory.initial", &factory);
    match ctx.invoke(
        "javax/naming/spi/NamingManager",
        "getInitialContext",
        "(Ljava/util/Hashtable;)Ljavax/naming/Context;",
        &[env],
    )? {
        Some(Value::Object(Some(c))) => Ok(Some(c)),
        _ => Ok(None),
    }
}

/// True iff Apache Tomcat's `org.apache.naming.ContextBindings` reports a
/// thread or class-loader binding — i.e. a web-app naming context is active
/// on the current thread. This class is absent (or never bound) under
/// WildFly / Keycloak, which use our flat in-memory store, so it is the
/// precise gate that keeps the flat-store path untouched.
fn tomcat_context_bound(ctx: &mut dyn NativeContext) -> bool {
    const CB: &str = "org/apache/naming/ContextBindings";
    if ctx.ensure_class_initialized(CB).is_err() {
        return false;
    }
    if matches!(ctx.invoke(CB, "isThreadBound", "()Z", &[]), Ok(Some(Value::Int(v))) if v != 0) {
        return true;
    }
    matches!(ctx.invoke(CB, "isClassLoaderBound", "()Z", &[]), Ok(Some(Value::Int(v))) if v != 0)
}

/// Obtain Apache Tomcat's per-thread `java:` `Context` (a `SelectorContext`
/// in non-initial mode) so a `java:comp/env/...` operation resolves against
/// the live web-app naming context bound by `NamingContextListener`.
///
/// `NamingManager.getURLContext("java", null)` returns null here — with a
/// null environment the factory list never includes `org.apache.naming`, so
/// the real `InitialContext.getURLOrDefaultInitCtx` would fall through to
/// `getDefaultInitCtx()`. We replicate the *effective* result directly:
/// instantiate the `java.naming.factory.initial` factory (Tomcat's
/// `enableNaming()` sets it to `org.apache.naming.java.javaURLContextFactory`)
/// and call its **`ObjectFactory.getObjectInstance`** method. When
/// `ContextBindings` is bound that returns `new SelectorContext(env)` in
/// *non-initial* mode — whose `parseName` strips the `java:` prefix and whose
/// `getBoundContext()` returns `ContextBindings.getThread()` (the populated
/// context). NOTE: the factory's *other* entry point,
/// `getInitialContext(env)`, returns a SelectorContext in *initial* mode that
/// does NOT strip `java:` and delegates to a separate, empty initial context
/// — so it must not be used here.
///
/// Our native `InitialContext` intercept previously implemented only the
/// (null-env, always-null) URL-context step, so `java:` lookups dropped to
/// the flat WildFly store and raised `NameNotFoundException: java:comp/env
/// not bound` (TestNamingContextListener: context start → STOPPED).
///
/// Returns `Ok(None)` — caller keeps the flat store — unless BOTH a non-empty
/// `java.naming.factory.initial` is configured AND Tomcat's `ContextBindings`
/// reports a thread/CL binding (absent under WildFly / Keycloak).
///
/// The environment is read from the pinned `this` just before the call
/// (gc-common w10-d): an env read up front went stale across the
/// `ContextBindings` calls and the factory's allocation.
fn tomcat_default_init_ctx(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let factory = match ctx.get_system_property("java.naming.factory.initial") {
        Some(s) if !s.trim().is_empty() => s.trim().replace('.', "/"),
        _ => return Ok(None),
    };
    if !tomcat_context_bound(ctx) {
        return Ok(None);
    }
    if ctx.ensure_class_initialized(&factory).is_err() {
        return Ok(None);
    }
    let factory_obj = match ctx.new_object_initialized(&factory, "()V", &[]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return Ok(None),
    };
    let env = pinned_context_env(ctx, this_pin, this);
    // ObjectFactory.getObjectInstance(obj=null, name=null, nameCtx=null, env)
    // → non-initial SelectorContext when ContextBindings is bound, else null.
    match ctx.invoke_virtual(
        factory_obj,
        "getObjectInstance",
        "(Ljava/lang/Object;Ljavax/naming/Name;Ljavax/naming/Context;Ljava/util/Hashtable;)\
         Ljava/lang/Object;",
        &[
            Value::Object(None),
            Value::Object(None),
            Value::Object(None),
            env,
        ],
    ) {
        Ok(Some(Value::Object(Some(c)))) => Ok(Some(c)),
        Ok(_) => Ok(None),
        // A throwing factory is not fatal: fall back to the flat store.
        Err(MethodCallFailed::ExceptionThrown(_)) => Ok(None),
        Err(other) => Err(other),
    }
}

/// The `Context` a JNDI operation on `name` is delegated to, in the stock
/// `InitialContext` order, or `None` for the flat store:
///
/// 1. with `use_builder`, an installed `InitialContextFactoryBuilder`'s
///    context ([`builder_initial_context`]);
/// 2. for a `java:` name, the JDK `java:` URL context ([`java_url_context`]),
///    then, with `use_tomcat`, Tomcat's default initial context
///    ([`tomcat_default_init_ctx`]).
///
/// `this` is the pinned `InitialContext`. The returned context is a current
/// address; hand it straight to [`delegate_named`], which roots it before
/// allocating.
fn delegate_context(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
    name: &str,
    use_builder: bool,
    use_tomcat: bool,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    if use_builder {
        if let Some(c) = builder_initial_context(ctx, this_pin, this)? {
            return Ok(Some(c));
        }
    }
    if !is_java_url_scheme(name) {
        return Ok(None);
    }
    if let Some(c) = java_url_context(ctx, this_pin, this)? {
        return Ok(Some(c));
    }
    if use_tomcat {
        return tomcat_default_init_ctx(ctx, this_pin, this);
    }
    Ok(None)
}

/// Delegate a named JNDI operation to `target` (a builder, URL-scheme or
/// provider `Context`), keeping `target` rooted across the allocation of the
/// name string. `value`, when given, is the pinned bound object `(pin, obj)`.
///
/// gc-common w10-d: every delegation site used to allocate the name string
/// and then dispatch on the `target` it had read before that allocation.
fn delegate_named(
    ctx: &mut dyn NativeContext,
    target: ObjectRef,
    method: &str,
    descriptor: &str,
    name: &str,
    value: Option<(usize, ObjectRef)>,
) -> MethodCallResult {
    let target_pin = ctx.pin_native_root(target);
    let name_obj = ctx.create_string(name);
    let target = ctx.read_native_pin(target_pin, target);
    let mut call_args = vec![Value::Object(Some(name_obj))];
    if let Some((pin, obj)) = value {
        call_args.push(Value::Object(Some(ctx.read_native_pin(pin, obj))));
    }
    let result = ctx.invoke_virtual(target, method, descriptor, &call_args);
    ctx.unpin_native_roots(target_pin);
    result
}

/// The `javax.naming.*` exception a `null name` argument raises.
fn null_name(ctx: &mut dyn NativeContext) -> MethodCallFailed {
    match throw_invalid_name(ctx, "null name") {
        Ok(e) | Err(e) => e,
    }
}

fn native_context_lookup(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = match read_string_arg(ctx, args, 1) {
        Some(n) => n,
        None => return Err(null_name(ctx)),
    };
    let this_pin = ctx.pin_native_root(this);
    let result = do_context_lookup(ctx, this_pin, this, &name);
    ctx.unpin_native_roots(this_pin);
    result
}

/// `Context.lookup(Name)` — the `javax.naming.Name` overload. Real
/// `InitialContext.lookup(Name)` bytecode resolves a `java:` name via
/// `getURLOrDefaultInitCtx(name)` → `NamingManager.getURLContext("java",
/// myProps)`. That works on HotSpot because the `InitialContext` constructor
/// copies the system JNDI properties into `myProps` (so `URL_PKG_PREFIXES` is
/// present); our native `<init>` stub skips that, leaving `myProps` empty, so
/// the real path falls through to `getDefaultInitCtx()` →
/// `javaURLContextFactory.getInitialContext(env)`, which yields an *initial*-mode
/// `SelectorContext` backed by a fresh, empty `NamingContext` — the lookup
/// misses and the servlet sees a `NameNotFoundException` (Tomcat
/// `testBug52830`: `lookup(new CompositeName("java:comp/env/boolean"))` → 500).
///
/// We mirror the `lookup(String)` intercept: render the `Name` to its string
/// form (a `CompositeName` round-trips `java:comp/env/...` exactly) and run the
/// identical delegation, which materialises the `URL_PKG_PREFIXES` environment
/// (see [`url_pkgs_env`]) and reaches the *non-initial* SelectorContext bound to
/// the current web-app.
fn native_context_lookup_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // `Name.toString()` runs Java: root `this` first (gc-common w10-d).
    let this_pin = ctx.pin_native_root(this);
    let result = match name_arg_to_string(ctx, args, 1) {
        Ok(Some(name)) => do_context_lookup(ctx, this_pin, this, &name),
        Ok(None) => Err(null_name(ctx)),
        Err(e) => Err(e),
    };
    ctx.unpin_native_roots(this_pin);
    result
}

/// Shared `lookup` body for both the `String` and `Name` overloads. `this`
/// is the pinned `InitialContext`.
fn do_context_lookup(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
    name: &str,
) -> MethodCallResult {
    const DESC: &str = "(Ljava/lang/String;)Ljava/lang/Object;";
    // A user-installed `InitialContextFactoryBuilder` (e.g. Spring's
    // `SimpleNamingContextBuilder`) owns the whole namespace — resolve through
    // its `Context`, exactly as the stock `InitialContext` bytecode would.
    //
    // `java:` URL-scheme names: hand off to the JDK's URL-context-factory
    // chain so Tomcat's own `org.apache.naming` context resolves the name
    // against `ContextBindings`. Falls back to the flat store on null/err.
    if let Some(deleg) = delegate_context(ctx, this_pin, this, name, true, true)? {
        return delegate_named(ctx, deleg, "lookup", DESC, name, None);
    }

    // A `java.naming.factory.initial` provider is the standard fallback once
    // URL-context handling has had its chance. This covers Spring's
    // TestableInitialContextFactory (and user providers generally), whose
    // process-wide bindings must be visible to every native InitialContext.
    if let Some(deleg) = configured_initial_context(ctx, this_pin, this)? {
        return delegate_named(ctx, deleg, "lookup", DESC, name, None);
    }

    match lookup_value(ctx.vm_identity(), name) {
        Ok(obj) => Ok(Some(Value::Object(Some(obj)))),
        Err(msg)
            if msg.starts_with("NameNotFoundException")
                && is_plain_relative_name(name)
                && !has_initial_context_provider(ctx) =>
        {
            Err(throw_no_initial_context(
                ctx,
                "Need to specify class name in environment or system property: java.naming.factory.initial",
            )?)
        }
        Err(msg) => Err(flat_store_error(ctx, &msg)?),
    }
}

/// Read a `javax.naming.Name` argument and render it to the string form the
/// flat store / SelectorContext expect. `Name.toString()` on a `CompositeName`
/// re-joins components with `/`, so `new CompositeName("java:comp/env/x")`
/// round-trips back to `java:comp/env/x`. Returns `Ok(None)` for a null arg.
fn name_arg_to_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    idx: usize,
) -> Result<Option<String>, MethodCallFailed> {
    let name_obj = match args.get(idx).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(None),
    };
    match ctx.invoke_virtual(name_obj, "toString", "()Ljava/lang/String;", &[])? {
        Some(Value::Object(Some(s))) => Ok(ctx.read_string(s)),
        _ => Ok(None),
    }
}

fn native_context_bind(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    bind_or_rebind(ctx, args, false)
}

fn native_context_rebind(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    bind_or_rebind(ctx, args, true)
}

/// Shared body of `InitialContext.bind` and `rebind`.
fn bind_or_rebind(ctx: &mut dyn NativeContext, args: &[Value], rebind: bool) -> MethodCallResult {
    let verb = if rebind { "rebind" } else { "bind" };
    let this = obj_arg(args, 0)?;
    let name = match read_string_arg(ctx, args, 1) {
        Some(n) => n,
        None => return Err(null_name(ctx)),
    };
    let value = match args.get(2).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => {
            return Err(throw_naming_exception(
                ctx,
                &format!("{verb}: null value not supported"),
            )?)
        }
    };

    // GC-SAFETY: `this` and `value` are bare Rust locals read from `args`,
    // not themselves GC roots. The delegate lookups (each dispatches real
    // Java) and the name string's allocation can all trigger a moving GC.
    // Pin both up front; every use re-reads its pin.
    let this_pin = ctx.pin_native_root(this);
    let value_pin = ctx.pin_native_root(value);
    let result = bind_or_rebind_pinned(ctx, (this_pin, this), (value_pin, value), &name, rebind);
    ctx.unpin_native_roots(this_pin);
    result
}

fn bind_or_rebind_pinned(
    ctx: &mut dyn NativeContext,
    (this_pin, this): (usize, ObjectRef),
    (value_pin, value): (usize, ObjectRef),
    name: &str,
    rebind: bool,
) -> MethodCallResult {
    const DESC: &str = "(Ljava/lang/String;Ljava/lang/Object;)V";
    let method = if rebind { "rebind" } else { "bind" };
    // Delegate to a user-installed `InitialContextFactoryBuilder`'s context so
    // reads and writes share the same namespace (see `do_context_lookup`);
    // `java:` names go through the JDK URL-context factory so the binding
    // lands in Tomcat's own context.
    if let Some(deleg) = delegate_context(ctx, this_pin, this, name, true, true)? {
        return delegate_named(ctx, deleg, method, DESC, name, Some((value_pin, value)));
    }

    // No allocation from here to the store: `value` stays current.
    let value = ctx.read_native_pin(value_pin, value);
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(value))
        .unwrap_or_else(|| "java/lang/Object".to_string());
    let vm = ctx.vm_identity();
    let stored = if rebind {
        rebind_value(vm, name, &class_name, value)
    } else {
        bind_value(vm, name, &class_name, value)
    };
    match stored {
        Ok(()) => Ok(None),
        Err(msg) => Err(flat_store_error(ctx, &msg)?),
    }
}

fn native_context_unbind(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = match read_string_arg(ctx, args, 1) {
        Some(n) => n,
        None => return Err(null_name(ctx)),
    };

    let this_pin = ctx.pin_native_root(this);
    let deleg = delegate_context(ctx, this_pin, this, &name, true, true);
    ctx.unpin_native_roots(this_pin);
    if let Some(deleg) = deleg? {
        return delegate_named(ctx, deleg, "unbind", "(Ljava/lang/String;)V", &name, None);
    }

    if let Err(msg) = unbind_value(ctx.vm_identity(), &name) {
        return Err(flat_store_error(ctx, &msg)?);
    }
    Ok(None)
}

fn native_context_create_subcontext(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = match read_string_arg(ctx, args, 1) {
        Some(n) => n,
        None => return Err(null_name(ctx)),
    };

    let this_pin = ctx.pin_native_root(this);
    let deleg = delegate_context(ctx, this_pin, this, &name, false, false);
    ctx.unpin_native_roots(this_pin);
    if let Some(deleg) = deleg? {
        return delegate_named(
            ctx,
            deleg,
            "createSubcontext",
            "(Ljava/lang/String;)Ljavax/naming/Context;",
            &name,
            None,
        );
    }

    if let Err(msg) = create_subcontext(&name) {
        return Err(flat_store_error(ctx, &msg)?);
    }
    // Return a fresh synthetic Context so the caller can chain bind().
    let sub =
        try_alloc_concurrent_synthetic(ctx, "javax/naming/InitialContext", INIT_CTX_NUM_SLOTS)?;
    Ok(Some(Value::Object(Some(sub))))
}

fn native_context_destroy_subcontext(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = match read_string_arg(ctx, args, 1) {
        Some(n) => n,
        None => return Err(null_name(ctx)),
    };

    let this_pin = ctx.pin_native_root(this);
    let deleg = delegate_context(ctx, this_pin, this, &name, false, false);
    ctx.unpin_native_roots(this_pin);
    if let Some(deleg) = deleg? {
        return delegate_named(
            ctx,
            deleg,
            "destroySubcontext",
            "(Ljava/lang/String;)V",
            &name,
            None,
        );
    }

    if let Err(msg) = destroy_subcontext(ctx.vm_identity(), &name) {
        return Err(flat_store_error(ctx, &msg)?);
    }
    Ok(None)
}

fn native_context_close(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Nothing to release — the store outlives individual Contexts.
    Ok(None)
}

/// Build a `javax.naming.Binding(name, className, object)`.
///
/// GC-SAFETY: `object` is pinned BEFORE the `Binding` allocation (gc-common
/// w10-d; it used to be pinned only after it, i.e. already stale), and the
/// `Binding` itself across the two string allocations.
fn alloc_java_binding(
    ctx: &mut dyn NativeContext,
    name: &str,
    class_name: &str,
    object: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let object_pin = ctx.pin_native_root(object);
    let obj = match try_alloc_concurrent_synthetic(ctx, "javax/naming/Binding", BINDING_NUM_SLOTS) {
        Ok(o) => o,
        Err(e) => {
            ctx.unpin_native_roots(object_pin);
            return Err(e);
        }
    };
    let obj_pin = ctx.pin_native_root(obj);
    let name_s = ctx.create_string(name);
    let name_s_pin = ctx.pin_native_root(name_s);
    let cn_s = ctx.create_string(class_name);
    let obj = ctx.read_native_pin(obj_pin, obj);
    let object = ctx.read_native_pin(object_pin, object);
    let name_s = ctx.read_native_pin(name_s_pin, name_s);
    ctx.set_field(obj, BINDING_FIELD_NAME, Value::Object(Some(name_s)));
    ctx.set_field(obj, BINDING_FIELD_CLASS_NAME, Value::Object(Some(cn_s)));
    ctx.set_field(obj, BINDING_FIELD_OBJECT, Value::Object(Some(object)));
    ctx.unpin_native_roots(object_pin);
    Ok(obj)
}

fn native_context_list_bindings(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = match read_string_arg(ctx, args, 1) {
        Some(n) => n,
        None => return Err(null_name(ctx)),
    };

    let this_pin = ctx.pin_native_root(this);
    let deleg = delegate_context(ctx, this_pin, this, &name, false, true);
    ctx.unpin_native_roots(this_pin);
    if let Some(deleg) = deleg? {
        return delegate_named(
            ctx,
            deleg,
            "listBindings",
            "(Ljava/lang/String;)Ljavax/naming/NamingEnumeration;",
            &name,
            None,
        );
    }

    // Build a reference-array of `Binding` objects as the
    // NamingEnumeration backing.
    let binding_cid = match ctx.ensure_class_initialized("javax/naming/Binding") {
        Ok(cid) => cid,
        // Fallible since 2026-08-10 (JDK-only wave 2, step 3): `javax.naming`
        // is an enterprise namespace, so a fabricated `Binding` stand-in is the
        // substitution contract §5 refuses. On a run that has JNDI on the
        // classpath the `Ok` arm is what runs.
        Err(_) => crate::util_concurrent_ext::refused_class(ctx, "javax/naming/Binding", 8)?,
    };
    // Read the children only after the class initialisation above, and pin
    // every bound value before the first allocation below (gc-common w10-d:
    // the snapshot's values, and the array itself, used to be held unpinned
    // across every `Binding` allocation).
    let children = match list_bindings(ctx.vm_identity(), &name) {
        Ok(c) => c,
        Err(msg) => return Err(flat_store_error(ctx, &msg)?),
    };
    let value_pins: Vec<Option<usize>> = children
        .iter()
        .map(|(_, e)| e.value.map(|v| ctx.pin_native_root(v)))
        .collect();
    let arr = ctx.new_ref_array(binding_cid, children.len());
    let arr_pin = ctx.pin_native_root(arr);
    let base = value_pins
        .iter()
        .flatten()
        .next()
        .copied()
        .unwrap_or(arr_pin);
    for (i, ((child_name, entry), pin)) in children.iter().zip(value_pins.iter()).enumerate() {
        let obj = match (entry.value, pin) {
            (Some(v), Some(p)) => ctx.read_native_pin(*p, v),
            // Service still starting — return the entry with an empty
            // placeholder object; Java code can inspect `className`.
            _ => ctx.create_string(""),
        };
        let binding =
            match alloc_java_binding(ctx, child_name.as_ref(), entry.class_name.as_ref(), obj) {
                Ok(b) => b,
                Err(e) => {
                    ctx.unpin_native_roots(base);
                    return Err(e);
                }
            };
        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.set_array_element(arr, i, Value::Object(Some(binding)));
    }
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(base);
    Ok(Some(Value::Object(Some(arr))))
}

fn native_service_based_naming_store_bind(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Signature: ServiceBasedNamingStore.bind(JndiName, Object)
    let _this = obj_arg(args, 0)?;
    let name = match read_string_arg(ctx, args, 1) {
        Some(n) => n,
        None => return Err(null_name(ctx)),
    };
    let value = match args.get(2).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Err(throw_naming_exception(ctx, "bind: null value")?),
    };
    // No allocation between reading `value` and storing it.
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(value))
        .unwrap_or_else(|| "java/lang/Object".to_string());
    if let Err(msg) = bind_value(ctx.vm_identity(), &name, &class_name, value) {
        return Err(flat_store_error(ctx, &msg)?);
    }
    Ok(None)
}

fn native_service_based_naming_store_lookup(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let _this = obj_arg(args, 0)?;
    let name = match read_string_arg(ctx, args, 1) {
        Some(n) => n,
        None => return Err(null_name(ctx)),
    };
    match lookup_value(ctx.vm_identity(), &name) {
        Ok(v) => Ok(Some(Value::Object(Some(v)))),
        Err(msg) => Err(flat_store_error(ctx, &msg)?),
    }
}

fn native_context_names_bind_info_for(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // static ContextNames.bindInfoFor(String absolute) -> BindInfo
    let absolute = match read_string_arg(ctx, args, 0) {
        Some(n) => n,
        None => return Err(throw_invalid_name(ctx, "null absolute name")?),
    };
    let info = match context_names_bind_info_for(&absolute) {
        Ok(i) => i,
        Err(msg) => return Err(throw_invalid_name(ctx, &msg)?),
    };
    let obj = try_alloc_concurrent_synthetic(
        ctx,
        "org/jboss/as/naming/deployment/ContextNames$BindInfo",
        BIND_INFO_NUM_SLOTS,
    )?;
    // GC-SAFETY: each of `obj`/`parent_obj`/`binder_obj`/`bind_name_s` is
    // captured well before its own `set_field_by_name` use below, and every
    // intervening `alloc_java_service_name`/`create_string` call can trigger
    // a moving GC. Pin each as it's produced and re-read immediately before
    // its use.
    let obj_pin = ctx.pin_native_root(obj);
    // Mirror ContextNames$BindInfo's real field layout exactly: parent
    // ServiceName, binder ServiceName, bindName String, absolute name String.
    let parent_obj = alloc_java_service_name(ctx, &info.parent_context_service_name)?;
    let parent_pin = ctx.pin_native_root(parent_obj);
    let binder_obj = alloc_java_service_name(ctx, &info.binder_service_name)?;
    let binder_pin = ctx.pin_native_root(binder_obj);
    let bind_name_s = ctx.create_string(info.binding_name.as_ref());
    let bind_name_pin = ctx.pin_native_root(bind_name_s);
    let absolute_s = ctx.create_string(info.absolute_name.as_ref());
    let obj = ctx.read_native_pin(obj_pin, obj);
    let parent_obj = ctx.read_native_pin(parent_pin, parent_obj);
    let binder_obj = ctx.read_native_pin(binder_pin, binder_obj);
    let bind_name_s = ctx.read_native_pin(bind_name_pin, bind_name_s);
    ctx.set_field_by_name(obj, BIND_INFO_FIELD_PARENT, Value::Object(Some(parent_obj)));
    ctx.set_field_by_name(obj, BIND_INFO_FIELD_BINDER, Value::Object(Some(binder_obj)));
    ctx.set_field_by_name(
        obj,
        BIND_INFO_FIELD_BIND_NAME,
        Value::Object(Some(bind_name_s)),
    );
    ctx.set_field_by_name(
        obj,
        BIND_INFO_FIELD_ABSOLUTE,
        Value::Object(Some(absolute_s)),
    );
    ctx.unpin_native_roots(obj_pin);
    Ok(Some(Value::Object(Some(obj))))
}

fn native_service_based_naming_store_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    ctx.set_field(this, NAMING_STORE_FIELD_BINDINGS, Value::Object(None));
    ctx.set_field(this, NAMING_STORE_FIELD_SERVICE_BASE, Value::Object(None));
    Ok(None)
}

// ===========================================================================
// Registration
// ===========================================================================

/// Register the `java.naming` bridge independently of the WildFly pack.
pub fn register_jdk_naming_natives(r: &mut NativeMethodRegistry) {
    // --- javax.naming.InitialContext ---
    let ic = "javax/naming/InitialContext";
    r.register(ic, "<init>", "()V", native_initial_context_init);
    r.register(
        ic,
        "getEnvironment",
        "()Ljava/util/Hashtable;",
        native_initial_context_get_environment,
    );
    r.register(
        ic,
        "lookup",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        native_context_lookup,
    );
    r.register(
        ic,
        "lookup",
        "(Ljavax/naming/Name;)Ljava/lang/Object;",
        native_context_lookup_name,
    );
    r.register(
        ic,
        "bind",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
        native_context_bind,
    );
    r.register(
        ic,
        "rebind",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
        native_context_rebind,
    );
    r.register(ic, "unbind", "(Ljava/lang/String;)V", native_context_unbind);
    r.register(
        ic,
        "createSubcontext",
        "(Ljava/lang/String;)Ljavax/naming/Context;",
        native_context_create_subcontext,
    );
    r.register(
        ic,
        "destroySubcontext",
        "(Ljava/lang/String;)V",
        native_context_destroy_subcontext,
    );
    r.register(ic, "close", "()V", native_context_close);
    r.register(
        ic,
        "listBindings",
        "(Ljava/lang/String;)Ljavax/naming/NamingEnumeration;",
        native_context_list_bindings,
    );
}

/// Register only WildFly-owned naming compatibility classes.
pub fn register_wildfly_naming_natives(r: &mut NativeMethodRegistry) {
    // --- org.jboss.as.naming.ServiceBasedNamingStore ---
    let sbns = "org/jboss/as/naming/ServiceBasedNamingStore";
    r.register(
        sbns,
        "<init>",
        "()V",
        native_service_based_naming_store_init,
    );
    r.register(
        sbns,
        "bind",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
        native_service_based_naming_store_bind,
    );
    r.register(
        sbns,
        "lookup",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        native_service_based_naming_store_lookup,
    );

    // --- org.jboss.as.naming.deployment.ContextNames ---
    let cn = "org/jboss/as/naming/deployment/ContextNames";
    r.register(
        cn,
        "bindInfoFor",
        "(Ljava/lang/String;)Lorg/jboss/as/naming/deployment/ContextNames$BindInfo;",
        native_context_names_bind_info_for,
    );
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
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

    /// Counter so tests generate unique JNDI sub-roots (they also share the
    /// process-wide `ServiceName` intern table).
    static SEQ: AtomicU32 = AtomicU32::new(1);

    /// A private VM identity per test (gc-common w10-d). The store and the
    /// MSC container are per VM, so tests no longer share a map and need no
    /// module lock; before w10-d a sibling test's reset of the one
    /// process-global map could wipe a test's bindings mid-assertion.
    /// Dropping the guard forgets the VM's bindings and container, even when
    /// an assertion fails.
    struct TestVm(usize);

    impl TestVm {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0x10D0_1000);
            TestVm(NEXT.fetch_add(1, Ordering::Relaxed))
        }
    }

    impl Drop for TestVm {
        fn drop(&mut self) {
            forget_vm_jndi_bindings(self.0);
            crate::jboss_msc::forget_vm_msc_container(self.0);
        }
    }

    fn unique_subroot(tag: &str) -> String {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        format!("java:jboss/t19_2_b_{tag}_{n}")
    }

    /// Synthesize a dummy `ObjectRef` for tests that only care about
    /// round-tripping the binding.  We pick a distinctive pointer value
    /// so two fake refs don't collide.
    fn fake_object_ref(id: usize) -> ObjectRef {
        // SAFETY: this pointer is never dereferenced; it's used purely
        // as an identity key by the binding store's HashMap.  The tests
        // do not hand the ref to NativeContext / the heap, so there's
        // no VM invariant that it points at a live allocation.
        unsafe { ObjectRef::from_raw((id * 16 + 8) as *mut _) }
    }

    // ---------------------------------------------------------------
    // 1. InitialContext constructor
    // ---------------------------------------------------------------
    #[test]
    fn t19_2_b_initial_context_constructor_succeeds() {
        // Constructor logic is pure: we just need the allowlist to hold.
        assert!(validate_jndi_name("java:comp/env").is_ok());
        assert!(validate_jndi_name("java:").is_ok());
    }

    // ---------------------------------------------------------------
    // 2. bind / lookup round-trip
    // ---------------------------------------------------------------
    #[test]
    fn t19_2_b_bind_lookup_round_trip() {
        let t = TestVm::new();
        let vm = t.0;
        let name = unique_subroot("roundtrip");
        let full = format!("{name}/leaf");
        let obj = fake_object_ref(1);
        bind_value(vm, &full, "com/example/Thing", obj).expect("bind");
        let round = lookup_value(vm, &full).expect("lookup");
        assert_eq!(round.as_ptr() as usize, obj.as_ptr() as usize);
    }

    // ---------------------------------------------------------------
    // 3. rebind replaces previous
    // ---------------------------------------------------------------
    #[test]
    fn t19_2_b_rebind_replaces_previous() {
        let t = TestVm::new();
        let vm = t.0;
        let name = unique_subroot("rebind");
        let full = format!("{name}/thing");
        let a = fake_object_ref(11);
        let b = fake_object_ref(22);
        bind_value(vm, &full, "X", a).expect("bind");
        rebind_value(vm, &full, "Y", b).expect("rebind");
        let got = lookup_value(vm, &full).expect("lookup");
        assert_eq!(got.as_ptr() as usize, b.as_ptr() as usize);
    }

    // ---------------------------------------------------------------
    // 4. unbind removes entry
    // ---------------------------------------------------------------
    #[test]
    fn t19_2_b_unbind_removes_entry() {
        let t = TestVm::new();
        let vm = t.0;
        let name = unique_subroot("unbind");
        let full = format!("{name}/leaf");
        bind_value(vm, &full, "X", fake_object_ref(33)).unwrap();
        assert!(lookup_value(vm, &full).is_ok());
        unbind_value(vm, &full).unwrap();
        let err = lookup_value(vm, &full).unwrap_err();
        assert!(
            err.contains("NameNotFoundException"),
            "expected NameNotFoundException, got {err}"
        );
    }

    // ---------------------------------------------------------------
    // 5. lookup missing → NameNotFoundException
    // ---------------------------------------------------------------
    #[test]
    fn t19_2_b_lookup_missing_name_throws_name_not_found() {
        let t = TestVm::new();
        let vm = t.0;
        let name = unique_subroot("missing");
        let full = format!("{name}/nope");
        let err = lookup_value(vm, &full).unwrap_err();
        assert!(err.contains("NameNotFoundException"), "got: {err}");
    }

    // ---------------------------------------------------------------
    // 6. hierarchical path walk
    // ---------------------------------------------------------------
    #[test]
    fn t19_2_b_hierarchical_path_walk() {
        let t = TestVm::new();
        let vm = t.0;
        let root = unique_subroot("hier");
        let full = format!("{root}/ds/KeycloakDS");
        let obj = fake_object_ref(44);
        bind_value(vm, &full, "javax/sql/DataSource", obj).unwrap();

        // The bind_info_for parse must produce a 4-or-more-segment
        // ServiceName plus the stripped binding path.
        let info = context_names_bind_info_for(&full).unwrap();
        assert!(info
            .binder_service_name
            .canonical()
            .starts_with("java.jboss.t19_2_b_hier"));
        assert!(info
            .binder_service_name
            .canonical()
            .ends_with(".ds.KeycloakDS"));
        assert!(info.binding_name.contains("/ds/KeycloakDS"));

        // Walk the hierarchy: lookup the full name should succeed.
        let resolved = lookup_value(vm, &full).unwrap();
        assert_eq!(resolved.as_ptr() as usize, obj.as_ptr() as usize);

        // A lookup of an intermediate that was never bound must fail
        // (`java:jboss/<root>/ds` is a subcontext, not a value).
        let mid = format!("{root}/ds");
        let err = lookup_value(vm, &mid).unwrap_err();
        assert!(err.contains("NameNotFoundException"));
    }

    // ---------------------------------------------------------------
    // 7. ContextNames.bindInfoFor parses absolute name
    // ---------------------------------------------------------------
    #[test]
    fn t19_2_b_context_names_bind_info_parses_absolute_name() {
        let info = context_names_bind_info_for("java:jboss/datasources/KeycloakDS").unwrap();
        assert_eq!(
            info.binder_service_name.canonical(),
            "java.jboss.datasources.KeycloakDS"
        );
        assert_eq!(info.parent_context_service_name.canonical(), "java.jboss");
        assert_eq!(&*info.binding_name, "datasources/KeycloakDS");
        assert_eq!(&*info.absolute_name, "java:jboss/datasources/KeycloakDS");

        let exported = context_names_bind_info_for("ExampleDS").unwrap();
        assert_eq!(
            exported.parent_context_service_name.canonical(),
            "java.jboss.exported"
        );
        assert_eq!(&*exported.binding_name, "ExampleDS");

        let explicit_exported = context_names_bind_info_for("java:jboss/exported/Foo").unwrap();
        assert_eq!(
            explicit_exported.parent_context_service_name.canonical(),
            "java.jboss.exported"
        );
        assert_eq!(&*explicit_exported.binding_name, "Foo");

        // And ensure the injection-guard rejects hostile URL schemes.
        assert!(context_names_bind_info_for("ldap://evil.example/a").is_err());
        assert!(context_names_bind_info_for("rmi://evil.example/a").is_err());
        assert!(context_names_bind_info_for("dns:evil.example").is_err());
    }

    // ---------------------------------------------------------------
    // 8. ServiceBasedNamingStore registers an MSC service
    // ---------------------------------------------------------------
    #[test]
    fn t19_2_b_service_based_naming_store_registers_msc_service() {
        let t = TestVm::new();
        let vm = t.0;
        let name = unique_subroot("mscreg");
        let full = format!("{name}/SvcThing");
        let obj = fake_object_ref(55);
        bind_value(vm, &full, "java/lang/Object", obj).unwrap();

        // T19.1 container must now see the binder service.
        let info = context_names_bind_info_for(&full).unwrap();
        let state = crate::jboss_msc::container_for(vm).get_state(&info.binder_service_name);
        assert!(
            state.is_some(),
            "MSC container should see the binder service {:?}",
            info.binder_service_name.canonical()
        );
    }

    // ---------------------------------------------------------------
    // 9. listBindings returns all direct children
    // ---------------------------------------------------------------
    #[test]
    fn t19_2_b_list_bindings_returns_all_direct_children() {
        let t = TestVm::new();
        let vm = t.0;
        let root = unique_subroot("listbind");
        bind_value(
            vm,
            &format!("{root}/child1"),
            "java/lang/Object",
            fake_object_ref(61),
        )
        .unwrap();
        bind_value(
            vm,
            &format!("{root}/child2"),
            "java/lang/Object",
            fake_object_ref(62),
        )
        .unwrap();
        bind_value(
            vm,
            &format!("{root}/sub/grandchild"),
            "java/lang/Object",
            fake_object_ref(63),
        )
        .unwrap();

        let kids = list_bindings(vm, &root).unwrap();
        // Only the two direct children; grandchild must not leak into
        // the direct-children list.
        assert_eq!(
            kids.len(),
            2,
            "expected exactly 2 direct children, got {:?}",
            kids.iter().map(|(k, _)| k.to_string()).collect::<Vec<_>>()
        );
        let names: Vec<String> = kids.iter().map(|(k, _)| k.to_string()).collect();
        assert!(names.iter().any(|n| n.ends_with("/child1")));
        assert!(names.iter().any(|n| n.ends_with("/child2")));
        assert!(!names.iter().any(|n| n.contains("grandchild")));
    }

    // ---------------------------------------------------------------
    // 10. concurrent bind + lookup — no panic, no data race
    // ---------------------------------------------------------------
    #[test]
    fn t19_2_b_concurrent_bind_lookup_no_data_race() {
        let t = TestVm::new();
        let vm = t.0;
        let root = unique_subroot("race");
        let handles: Vec<_> = (0..4)
            .map(|tid| {
                let root = root.clone();
                std::thread::spawn(move || {
                    for i in 0..32 {
                        let full = format!("{root}/t{tid}_k{i}");
                        bind_value(
                            vm,
                            &full,
                            "java/lang/Object",
                            fake_object_ref(tid * 100 + i),
                        )
                        .expect("thread bind");
                        // Every writer also reads back its own key —
                        // exercises the RwLock read path concurrently
                        // with writers on other threads.
                        let got = lookup_value(vm, &full).expect("thread lookup");
                        assert_eq!(got.as_ptr() as usize % 2, 0);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("thread panicked");
        }
        // All four threads, 32 bindings each, still visible.
        let prefix = format!("{root}/");
        let survivors = BINDINGS
            .peek(vm, |store| {
                store.keys().filter(|k| k.starts_with(&prefix)).count()
            })
            .unwrap_or(0);
        assert_eq!(survivors, 4 * 32);
    }

    // ---------------------------------------------------------------
    // Extra coverage: the injection guard is the load-bearing piece.
    // ---------------------------------------------------------------
    #[test]
    fn t19_2_b_injection_guard_rejects_remote_schemes() {
        for bad in [
            "ldap://evil/a",
            "ldaps://evil/a",
            "rmi://evil/a",
            "dns:evil",
            "iiop://evil/a",
            "corbaname:evil",
            "http://evil/a",
            "file:/etc/passwd",
        ] {
            assert!(
                validate_jndi_name(bad).is_err(),
                "injection guard must reject {bad}"
            );
        }
    }

    #[test]
    fn t19_2_b_injection_guard_accepts_known_roots() {
        for good in [
            "java:",
            "java:jboss/datasources/KeycloakDS",
            "java:comp/env/thing",
            "java:global/app/mod/bean",
            "java:app/whatever",
            "java:module/thing",
        ] {
            assert!(
                validate_jndi_name(good).is_ok(),
                "valid name unexpectedly rejected: {good}"
            );
        }
    }

    #[test]
    fn t19_2_b_url_reference_lookup_refuses_to_connect() {
        let t = TestVm::new();
        let vm = t.0;
        let name = unique_subroot("urlref");
        let full = format!("{name}/thing");
        // Manually install a URL-ref entry to simulate what a
        // spec-compliant JDK would open as a network connection.
        {
            let info = context_names_bind_info_for(&full).unwrap();
            BINDINGS.with(vm, |store| {
                store.insert(
                    info.absolute_name.clone(),
                    BindingEntry {
                        service_name: info.binder_service_name,
                        class_name: Arc::<str>::from("javax/naming/Reference"),
                        value: None,
                        failed: false,
                        url_reference: Some(Arc::<str>::from("ldap://attacker/")),
                    },
                );
            });
        }
        let err = lookup_value(vm, &full).unwrap_err();
        assert!(
            err.contains("NameNotFoundException"),
            "URL-ref lookup must refuse with NameNotFoundException, got {err}"
        );
        assert!(err.contains("refused by JNDI injection guard"));
    }

    /// `common-w9b-jndi-bindings-store-holds-unrooted-object-refs`: two VMs
    /// bind the same name. Each VM's scan holds only its own value, a remap
    /// with one VM's pointer map moves only that VM's entry, each VM's lookup
    /// answers its own object, and teardown drops one VM's bindings only.
    #[test]
    fn w10d_bindings_are_per_vm_rooted_and_remapped() {
        let (ta, tb) = (TestVm::new(), TestVm::new());
        let (vm_a, vm_b) = (ta.0, tb.0);
        let name = format!("{}/cfg", unique_subroot("w10d_two_vms"));
        let (obj_a, obj_b) = (fake_object_ref(0x10D1), fake_object_ref(0x10D2));
        bind_value(vm_a, &name, "java/util/HashMap", obj_a).unwrap();
        bind_value(vm_b, &name, "java/util/HashMap", obj_b).unwrap();

        let scan = |vm: usize| {
            let mut out = Vec::new();
            gc_scan_jndi_binding_roots(vm, &mut out);
            out
        };
        assert_eq!(scan(vm_a), vec![obj_a], "A's scan roots A's value only");
        assert_eq!(scan(vm_b), vec![obj_b], "B's scan roots B's value only");

        // A moving collection of A relocates A's value.
        let moved_a = fake_object_ref(0x10D3);
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(obj_a.as_ptr() as usize, moved_a.as_ptr() as usize);
        gc_update_jndi_binding_refs(vm_b, &map);
        assert_eq!(
            lookup_value(vm_a, &name).unwrap(),
            obj_a,
            "B's map must not move A's entry"
        );
        gc_update_jndi_binding_refs(vm_a, &map);
        assert_eq!(
            lookup_value(vm_a, &name).unwrap(),
            moved_a,
            "A's lookup sees the move"
        );
        assert_eq!(
            lookup_value(vm_b, &name).unwrap(),
            obj_b,
            "B keeps its own object"
        );

        // Each VM's binder service lives in that VM's MSC container only.
        let info = context_names_bind_info_for(&name).unwrap();
        assert!(crate::jboss_msc::container_for(vm_a)
            .get_state(&info.binder_service_name)
            .is_some());

        forget_vm_jndi_bindings(vm_a);
        assert!(
            lookup_value(vm_a, &name).is_err(),
            "teardown drops A's bindings"
        );
        assert!(scan(vm_a).is_empty());
        assert_eq!(lookup_value(vm_b, &name).unwrap(), obj_b, "and leaves B's");
    }
}
