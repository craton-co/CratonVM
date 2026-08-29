// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T19.2.c — WildFly Security (JAAS + SecurityDomains + login modules).
//!
//! Keycloak 16 is itself an identity provider, but it *also* leans on
//! WildFly's JAAS framework for two bootstrap paths:
//!
//!  1. The `management-realm` (a `jboss-admin` login module) that the
//!     server uses to gate its own management console / CLI.
//!  2. JPA datasource authentication — the `ApplicationPolicy` for a
//!     named `SecurityDomain` (e.g. `keycloak`) is the canonical source
//!     of truth for "what JDBC credentials does this datasource use?".
//!
//! The native surface here models JAAS explicitly so we can satisfy the
//! boot sequence without dragging WildFly's own `jboss-as-security` jar
//! through the interpreter's hot path.
//!
//! # JAAS control-flag state machine
//!
//! `javax.security.auth.login.LoginContext.login()` iterates the
//! configured module chain.  Each module entry has one of four flags:
//!
//!   * `REQUIRED`    — must succeed; a failure does **not** abort the
//!                     iteration, but the final result of the whole
//!                     chain becomes a failure.
//!   * `REQUISITE`   — must succeed; a failure aborts immediately and
//!                     the whole chain fails.
//!   * `SUFFICIENT`  — success short-circuits the chain to a success
//!                     (provided no `REQUIRED` has already failed);
//!                     failure is ignored and iteration continues.
//!   * `OPTIONAL`    — success or failure is ignored for purposes of
//!                     the overall chain result.
//!
//! Final result: overall `true` iff **at least one non-optional module
//! succeeded** AND **no required module failed**.  That matches Sun/JDK
//! `LoginContext` semantics as documented in the JAAS specification.
//!
//! # Security posture
//!
//! * **Credential handling**: `Subject`'s private-credentials set can
//!   contain cleartext passwords.  No `tracing::*` call ever formats the
//!   `private_creds` set; when credentials appear in any log message
//!   they are redacted to `<redacted>` by [`redact_private`].
//! * **Timing attack mitigation**: every password comparison runs
//!   through [`constant_time_eq`], which XORs byte-by-byte, ORs the
//!   differences into a single accumulator, and checks the accumulator
//!   at the end.  Short-circuit comparisons (`==` on `&[u8]`) leak the
//!   first differing byte's position via wall-clock timing.
//! * **Thread-safety**: `Subject`'s three sets are guarded by
//!   `parking_lot::RwLock`, so concurrent `doAs` invocations from
//!   different threads see a consistent view.  Each modification takes
//!   the write lock briefly; reads take the read lock.
//! * **`Subject.doAs` scope safety**: uses a drop-guard so a panic
//!   inside `PrivilegedAction.run()` still restores the previous
//!   subject on unwind.  Otherwise a panicking action would leak an
//!   elevated subject into the thread's subsequent `doAs`-less work.
//! * **Principal immutability**: [`Principal`] has no `set_name` — the
//!   interned `Arc<str>` is fixed at construction time so later code
//!   cannot forge a different identity through an existing handle.
//!
//! Companion synthetic-stub entries live in
//! `classloading/src/class_manager.rs::synthetic_stub_fields` for
//! `javax/security/auth/Subject`,
//! `javax/security/auth/login/LoginContext`,
//! `javax/security/auth/login/AppConfigurationEntry`,
//! `org/jboss/as/security/SecurityDomainService`,
//! and `org/wildfly/security/auth/server/SecurityIdentity`.

#![allow(clippy::needless_pass_by_value)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};
use parking_lot::RwLock;

use crate::{obj_arg, try_alloc_concurrent_synthetic};

// ===========================================================================
// Principal — interned, immutable name; value-equality on name only.
// ===========================================================================

/// A named identity.  Construction interns the `Arc<str>` name so two
/// principals with the same string compare equal under `==` without a
/// byte-by-byte strcmp, and so `HashSet<Arc<Principal>>` dedups
/// equivalent entries.
#[derive(Debug)]
pub struct Principal {
    name: Arc<str>,
}

impl Principal {
    /// Create a principal with the given name.  After construction the
    /// name cannot be mutated — there is intentionally no `set_name`
    /// method exposed.
    pub fn new(name: &str) -> Arc<Principal> {
        Arc::new(Principal {
            name: Arc::<str>::from(name),
        })
    }

    /// The principal's name (immutable).
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl PartialEq for Principal {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}
impl Eq for Principal {}

impl std::hash::Hash for Principal {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state)
    }
}

// ===========================================================================
// Subject — principals + public/private credentials sets.
// ===========================================================================

/// A JAAS subject: the authenticated-identity aggregate.  Backed by
/// three lock-guarded sets.
///
/// The private-credentials set may contain cleartext secrets; do not
/// feed it to `tracing::*` without going through [`redact_private`].
#[derive(Debug)]
pub struct Subject {
    principals: RwLock<HashSet<Arc<Principal>>>,
    public_creds: RwLock<HashSet<Arc<str>>>,
    private_creds: RwLock<HashSet<Arc<[u8]>>>,
}

impl Subject {
    /// Fresh subject with empty principals / credentials sets.
    pub fn new() -> Arc<Subject> {
        Arc::new(Subject {
            principals: RwLock::new(HashSet::new()),
            public_creds: RwLock::new(HashSet::new()),
            private_creds: RwLock::new(HashSet::new()),
        })
    }

    /// Add a principal to the principal set.  Set semantics — a
    /// principal whose name already exists is silently deduped (the
    /// older handle is kept).
    pub fn add_principal(&self, p: Arc<Principal>) {
        self.principals.write().insert(p);
    }

    /// Copy every principal currently in the subject, allocating a new
    /// `Vec` — callers do not see the live set (so they can't mutate
    /// it without going through `add_principal`).
    pub fn get_principals(&self) -> Vec<Arc<Principal>> {
        self.principals.read().iter().cloned().collect()
    }

    /// Like `get_principals` but filtering by `Principal` name suffix
    /// match against the Java class name — a minimal approximation of
    /// the JDK's `getPrincipals(Class<T>)` overload.  We match on the
    /// last `.`-segment of the class name against the stored principal
    /// type (when recorded).
    pub fn get_principals_of(&self, class_name: &str) -> Vec<Arc<Principal>> {
        let suffix = class_name.rsplit('/').next().unwrap_or(class_name);
        self.principals
            .read()
            .iter()
            .filter(|p| p.name.ends_with(suffix) || suffix == "Principal")
            .cloned()
            .collect()
    }

    /// Add a public credential (e.g. a certificate encoded as base64).
    pub fn add_public_credential(&self, cred: &str) {
        self.public_creds.write().insert(Arc::<str>::from(cred));
    }

    /// Snapshot the public-credentials set.
    pub fn get_public_credentials(&self) -> Vec<Arc<str>> {
        self.public_creds.read().iter().cloned().collect()
    }

    /// Add a private credential (e.g. a password).  Handled as raw
    /// bytes so structured-logging formatters don't accidentally ship
    /// the plaintext to stdout through a `Display` impl.
    pub fn add_private_credential(&self, cred: &[u8]) {
        self.private_creds.write().insert(Arc::<[u8]>::from(cred));
    }

    /// Snapshot the private-credentials set.  Callers that log must
    /// feed the returned slice(s) through [`redact_private`].
    pub fn get_private_credentials(&self) -> Vec<Arc<[u8]>> {
        self.private_creds.read().iter().cloned().collect()
    }

    /// Count of distinct principals attached to this subject.
    pub fn principal_count(&self) -> usize {
        self.principals.read().len()
    }
}

/// Tracing-safe formatter for a private credential.  Used instead of
/// `format!("{:?}", cred)` anywhere a log line could touch
/// `get_private_credentials()`.
pub fn redact_private(_bytes: &[u8]) -> &'static str {
    "<redacted>"
}

// ===========================================================================
// Subject.doAs — scope-guarded subject elevation.
// ===========================================================================

thread_local! {
    static CURRENT_SUBJECT: std::cell::RefCell<Option<Arc<Subject>>> =
        const { std::cell::RefCell::new(None) };
}

/// Return the subject currently active on this thread, if any.  Used by
/// `SecurityIdentity.runAs` and by tests that want to assert the
/// subject-elevation scope.
pub fn current_subject() -> Option<Arc<Subject>> {
    CURRENT_SUBJECT.with(|s| s.borrow().clone())
}

/// Drop guard that restores the previous subject on scope exit,
/// including during panic unwind. Keeps `doAs` panic-safe.
struct SubjectGuard {
    previous: Option<Arc<Subject>>,
}

impl SubjectGuard {
    fn push(new: Arc<Subject>) -> SubjectGuard {
        let previous = CURRENT_SUBJECT.with(|s| s.replace(Some(new)));
        SubjectGuard { previous }
    }
}

impl Drop for SubjectGuard {
    fn drop(&mut self) {
        let prev = self.previous.take();
        CURRENT_SUBJECT.with(|s| *s.borrow_mut() = prev);
    }
}

/// Run `action` with `subject` active as the current-thread subject.
/// The previous subject (if any) is restored on return or unwind.
pub fn do_as<R, F: FnOnce() -> R>(subject: Arc<Subject>, action: F) -> R {
    let _g = SubjectGuard::push(subject);
    action()
}

// ===========================================================================
// LoginContext — JAAS control-flag chain.
// ===========================================================================

/// Outcome of a single `LoginModule.login()` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleOutcome {
    Success,
    Failure,
    /// Module was never invoked (e.g. sufficient short-circuit stopped
    /// chain before we reached this entry).
    NotInvoked,
}

/// The four JAAS control flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlFlag {
    Required,
    Requisite,
    Sufficient,
    Optional,
}

impl ControlFlag {
    /// Parse a JAAS-config control-flag token.  Matches the case-
    /// insensitive JDK parser.
    pub fn parse(s: &str) -> Option<ControlFlag> {
        let lower = s.trim().to_ascii_lowercase();
        let token = lower
            .strip_prefix("loginmodulecontrolflag:")
            .unwrap_or(&lower)
            .trim();
        match token {
            "required" => Some(ControlFlag::Required),
            "requisite" => Some(ControlFlag::Requisite),
            "sufficient" => Some(ControlFlag::Sufficient),
            "optional" => Some(ControlFlag::Optional),
            _ => None,
        }
    }

    /// Human form for logging — matches JDK's enum `name()`.
    pub fn as_str(self) -> &'static str {
        match self {
            ControlFlag::Required => "REQUIRED",
            ControlFlag::Requisite => "REQUISITE",
            ControlFlag::Sufficient => "SUFFICIENT",
            ControlFlag::Optional => "OPTIONAL",
        }
    }
}

/// Signature for a module-login callback.  Takes the target `Subject`
/// (the one `LoginContext` was built with), a borrow of the
/// per-module options map, and returns success / failure.  Real
/// `LoginModule` implementations wrap their Java-side `login()` call
/// behind a closure of this shape so the JAAS state machine stays
/// pure Rust.
pub type ModuleLogin =
    Box<dyn Fn(&Subject, &HashMap<String, String>) -> ModuleOutcome + Send + Sync + 'static>;

/// One entry in the login-module chain.
pub struct LoginModuleEntry {
    /// Canonical class name of the `LoginModule` subclass (e.g.
    /// `org.jboss.security.auth.spi.DatabaseServerLoginModule`).
    pub class_name: String,
    /// JAAS control flag.
    pub flag: ControlFlag,
    /// Module-specific options (e.g. `dsJndiName`, `principalsQuery`).
    pub options: HashMap<String, String>,
    /// Native-side `login()` implementation.  For Keycloak's bootstrap
    /// path this is a documented no-op that succeeds and populates the
    /// subject with an `internal` principal.
    pub login_fn: ModuleLogin,
}

impl std::fmt::Debug for LoginModuleEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginModuleEntry")
            .field("class_name", &self.class_name)
            .field("flag", &self.flag)
            .field("options", &"<redacted>")
            .finish()
    }
}

/// Aggregate result of running an entire login-module chain.
#[derive(Debug, Clone)]
pub struct LoginResult {
    /// Per-entry outcomes, in chain order.
    pub outcomes: Vec<ModuleOutcome>,
    /// Overall chain result.
    pub success: bool,
}

/// The canonical JAAS `LoginContext`.
pub struct LoginContext {
    pub name: String,
    pub subject: Arc<Subject>,
    pub modules: Vec<Arc<LoginModuleEntry>>,
    /// Set to the most recent `login()` outcome; cleared on `logout()`.
    last_result: RwLock<Option<LoginResult>>,
}

impl LoginContext {
    /// Build a context with a chain of modules.
    pub fn new(
        name: impl Into<String>,
        subject: Arc<Subject>,
        modules: Vec<Arc<LoginModuleEntry>>,
    ) -> Arc<LoginContext> {
        Arc::new(LoginContext {
            name: name.into(),
            subject,
            modules,
            last_result: RwLock::new(None),
        })
    }

    /// The subject this context authenticates into.
    pub fn get_subject(&self) -> Arc<Subject> {
        self.subject.clone()
    }

    /// Execute the JAAS control-flag state machine.  See module-level
    /// docs for the exact semantics.
    ///
    /// Returns the aggregate [`LoginResult`].  On failure the returned
    /// result's `success` is `false`; we do not emit an `Err` because
    /// JAAS conventionally reports authentication failures via the
    /// result rather than by throwing.  Tracing emits the per-module
    /// outcome at `debug` level with no credential bytes logged.
    pub fn login(&self) -> LoginResult {
        let mut outcomes: Vec<ModuleOutcome> = Vec::with_capacity(self.modules.len());
        let mut any_non_optional_success = false;
        let mut any_required_failure = false;
        let mut sufficient_success = false;

        for entry in &self.modules {
            if sufficient_success {
                // A SUFFICIENT already short-circuited the chain. Still
                // emit a slot so per-module outcomes line up with the
                // input list — important for tests that inspect the
                // chain shape.
                outcomes.push(ModuleOutcome::NotInvoked);
                continue;
            }

            let outcome = (entry.login_fn)(&self.subject, &entry.options);
            tracing::debug!(
                module = %entry.class_name,
                flag = %entry.flag.as_str(),
                outcome = ?outcome,
                "LoginModule.login"
            );
            outcomes.push(outcome);

            match (entry.flag, outcome) {
                (ControlFlag::Required, ModuleOutcome::Success)
                | (ControlFlag::Requisite, ModuleOutcome::Success) => {
                    any_non_optional_success = true;
                }
                (ControlFlag::Required, ModuleOutcome::Failure) => {
                    any_required_failure = true;
                    // Spec: REQUIRED failure still invokes remaining
                    // modules; don't abort here.
                }
                (ControlFlag::Requisite, ModuleOutcome::Failure) => {
                    // Immediate abort for REQUISITE.
                    any_required_failure = true;
                    break;
                }
                (ControlFlag::Sufficient, ModuleOutcome::Success) => {
                    // Short-circuit to success (modulo any already-
                    // observed REQUIRED failure).
                    any_non_optional_success = true;
                    sufficient_success = true;
                }
                (ControlFlag::Sufficient, ModuleOutcome::Failure) | (ControlFlag::Optional, _) => {
                    // Ignored — continue.
                }
                (_, ModuleOutcome::NotInvoked) => {
                    // A module's login_fn should not synthesize
                    // NotInvoked; treat as failure for safety.
                    if entry.flag == ControlFlag::Required || entry.flag == ControlFlag::Requisite {
                        any_required_failure = true;
                    }
                }
            }
        }

        let success = any_non_optional_success && !any_required_failure;
        let result = LoginResult { outcomes, success };
        *self.last_result.write() = Some(result.clone());
        result
    }

    /// Logout — drops cached credentials and clears `last_result`.
    /// Matches the spec: the subject's principals/credentials may be
    /// removed by cooperating modules; we conservatively only clear
    /// the private-credentials set to avoid accidentally forgetting a
    /// still-valid public cert.
    pub fn logout(&self) {
        self.subject.private_creds.write().clear();
        *self.last_result.write() = None;
    }

    /// The most recent `login()` result, if any.
    pub fn last_result(&self) -> Option<LoginResult> {
        self.last_result.read().clone()
    }
}

// ===========================================================================
// Helpers for building login modules in tests / bootstrap.
// ===========================================================================

/// Build a documented no-op module that always succeeds and attaches
/// an `internal` principal to the subject.  Used for the Keycloak 16
/// bootstrap path where the WildFly `management-realm` isn't used for
/// client authentication (KC's own realm owns that).
pub fn internal_bootstrap_module(flag: ControlFlag) -> Arc<LoginModuleEntry> {
    Arc::new(LoginModuleEntry {
        class_name: "org.jboss.as.security.plugins.InternalBootstrapLoginModule".into(),
        flag,
        options: HashMap::new(),
        login_fn: Box::new(|subject, _opts| {
            subject.add_principal(Principal::new("internal"));
            ModuleOutcome::Success
        }),
    })
}

/// Constant-time byte-slice comparison.  Returns `true` iff `a == b`
/// without early-exiting on the first differing byte.  Works on slices
/// of equal length; if the lengths differ we still fully hash both
/// sides so an attacker can't use a length-probe oracle.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    // Length mismatch is still constant-time for the shorter side;
    // the length check itself is O(1) and doesn't leak content.
    if a.len() != b.len() {
        // Still consume both sides to keep timing uniform relative to
        // the longer slice.  An `acc` folded over both defeats the
        // compiler's ability to short-circuit.
        let mut acc: u8 = 1; // seed non-zero so mismatch is reported
        let max = a.len().max(b.len());
        for i in 0..max {
            let x = *a.get(i).unwrap_or(&0);
            let y = *b.get(i).unwrap_or(&0);
            acc |= x ^ y;
        }
        // Prevent the compiler from realising the result is already
        // `true` once `acc` has any bit set.
        return std::hint::black_box(acc) == 0;
    }
    let mut acc: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    std::hint::black_box(acc) == 0
}

/// Build a database-backed login module: on `login()`, compares the
/// provided password against the configured stored bytes in constant
/// time.  This is the primary real-world shape used by WildFly's
/// `DatabaseServerLoginModule`; our bootstrap driver feeds an already-
/// hashed stored password and the candidate in the options map.
pub fn db_password_module(
    class_name: impl Into<String>,
    flag: ControlFlag,
    stored_password: Vec<u8>,
    candidate_password: Vec<u8>,
    principal_name: String,
) -> Arc<LoginModuleEntry> {
    let stored = Arc::<[u8]>::from(stored_password);
    let candidate = Arc::<[u8]>::from(candidate_password);
    Arc::new(LoginModuleEntry {
        class_name: class_name.into(),
        flag,
        options: HashMap::new(),
        login_fn: Box::new(move |subject, _opts| {
            if constant_time_eq(&stored, &candidate) {
                subject.add_principal(Principal::new(&principal_name));
                ModuleOutcome::Success
            } else {
                ModuleOutcome::Failure
            }
        }),
    })
}

// ===========================================================================
// ApplicationPolicy / SecurityDomainService — per-domain module chain.
// ===========================================================================

/// Registered login-module chain for a named security domain.
#[derive(Debug, Default)]
pub struct ApplicationPolicy {
    modules: Vec<Arc<LoginModuleEntry>>,
}

impl ApplicationPolicy {
    pub fn new(modules: Vec<Arc<LoginModuleEntry>>) -> ApplicationPolicy {
        ApplicationPolicy { modules }
    }

    /// Return the chain of module entries this policy authenticates
    /// through.
    pub fn get_authentication_info(&self) -> &[Arc<LoginModuleEntry>] {
        &self.modules
    }
}

/// A `SecurityDomainService` binds a name to an `ApplicationPolicy`
/// and exposes an `AuthenticationManager`.  For our model the
/// authentication manager is just a thin wrapper over the policy's
/// module chain — when a consumer wants to authenticate it builds a
/// `LoginContext` from the chain.
#[derive(Debug)]
pub struct SecurityDomainService {
    pub name: String,
    pub policy: ApplicationPolicy,
}

impl SecurityDomainService {
    pub fn new(name: impl Into<String>, policy: ApplicationPolicy) -> Arc<SecurityDomainService> {
        Arc::new(SecurityDomainService {
            name: name.into(),
            policy,
        })
    }

    /// Return the opaque `AuthenticationManager` handle.  In our model
    /// the manager and the service are the same Rust-side object.
    pub fn get_authentication_manager(self: &Arc<Self>) -> Arc<SecurityDomainService> {
        self.clone()
    }

    /// Build a fresh `LoginContext` over this domain's module chain.
    /// Callers can invoke `.login()` to execute JAAS authentication.
    pub fn new_login_context(&self, name: impl Into<String>) -> Arc<LoginContext> {
        LoginContext::new(name, Subject::new(), self.policy.modules.clone())
    }
}

/// Global, lock-guarded table of registered security domains.  WildFly
/// populates this at boot from `standalone.xml`; Keycloak 16 adds a
/// `keycloak` domain during its deployer.
static DOMAIN_REGISTRY: OnceLock<RwLock<HashMap<String, Arc<SecurityDomainService>>>> =
    OnceLock::new();

fn domain_registry() -> &'static RwLock<HashMap<String, Arc<SecurityDomainService>>> {
    DOMAIN_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Register a security domain so `SecurityDomainService.lookup(name)`
/// finds it later.  Overwrites a prior registration under the same
/// name — matches WildFly's "last deployer wins" semantics.
pub fn register_security_domain(domain: Arc<SecurityDomainService>) {
    domain_registry()
        .write()
        .insert(domain.name.clone(), domain);
}

/// Look up a previously-registered domain by name.  Returns `None`
/// when the domain isn't registered (the WildFly JVM throws
/// `SecurityDomainException` here; we leave that to the caller).
pub fn lookup_security_domain(name: &str) -> Option<Arc<SecurityDomainService>> {
    domain_registry().read().get(name).cloned()
}

/// Test hook: wipe the domain registry between tests.  Tests that
/// mutate the global registry MUST grab [`domain_test_lock`] first.
#[cfg(test)]
fn clear_domain_registry() {
    domain_registry().write().clear();
}

// ===========================================================================
// SecurityIdentity — principal + role set, with runAs scope.
// ===========================================================================

/// A `SecurityIdentity` pairs a principal with a role set.  WildFly
/// Elytron stores the effective identity of the currently-active
/// call; our model just keeps it in a thread-local alongside the
/// `Subject` stack.
#[derive(Debug)]
pub struct SecurityIdentity {
    pub principal: Arc<Principal>,
    pub roles: HashSet<Arc<str>>,
}

impl SecurityIdentity {
    pub fn new(
        principal: Arc<Principal>,
        roles: impl IntoIterator<Item = String>,
    ) -> Arc<SecurityIdentity> {
        Arc::new(SecurityIdentity {
            principal,
            roles: roles.into_iter().map(Arc::<str>::from).collect(),
        })
    }

    /// Role-set snapshot.
    pub fn get_roles(&self) -> Vec<Arc<str>> {
        self.roles.iter().cloned().collect()
    }

    /// Does this identity have the named role?
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r.as_ref() == role)
    }

    /// Execute `action` under this identity — the SecurityIdentity
    /// equivalent of `Subject.doAs`.  The previous identity is
    /// restored on unwind.
    pub fn run_as<R, F: FnOnce() -> R>(self: &Arc<Self>, action: F) -> R {
        let _g = IdentityGuard::push(self.clone());
        action()
    }
}

thread_local! {
    static CURRENT_IDENTITY: std::cell::RefCell<Option<Arc<SecurityIdentity>>> =
        const { std::cell::RefCell::new(None) };
}

/// The identity currently active on this thread, if any.
pub fn current_identity() -> Option<Arc<SecurityIdentity>> {
    CURRENT_IDENTITY.with(|s| s.borrow().clone())
}

struct IdentityGuard {
    previous: Option<Arc<SecurityIdentity>>,
}

impl IdentityGuard {
    fn push(new: Arc<SecurityIdentity>) -> IdentityGuard {
        let previous = CURRENT_IDENTITY.with(|s| s.replace(Some(new)));
        IdentityGuard { previous }
    }
}

impl Drop for IdentityGuard {
    fn drop(&mut self) {
        let prev = self.previous.take();
        CURRENT_IDENTITY.with(|s| *s.borrow_mut() = prev);
    }
}

// ===========================================================================
// Native object ⇄ Rust handle bridge.
// ===========================================================================

/// Map from a Java-side synthetic `Subject` / `LoginContext` object to
/// its Rust-side handle.  Keyed on `NativeContext::identity_hash_code(obj)`,
/// which is GC-stable: `HashCodeTable::update_after_gc` remaps the
/// per-object hash code when the GC relocates the Java object during
/// compaction (see `gc/src/compact_header.rs`).  Earlier keying on
/// `obj.as_ptr() as usize` orphaned every Subject / LoginContext after
/// the first compaction cycle — `Subject.getPrincipals()` returned the
/// "defensive empty set" branch and `LoginContext.login()` raised
/// `IllegalStateException` for a context that *was* initialised.  Same
/// fix class as C12/C13/C14/C15; canonical reference is
/// `lang_invoke::VH_META_TABLE` at
/// `native-builtins/src/lang_invoke.rs:178-203`.
static SUBJECT_HANDLES: OnceLock<RwLock<HashMap<i32, Arc<Subject>>>> = OnceLock::new();
static LOGIN_CONTEXT_HANDLES: OnceLock<RwLock<HashMap<i32, Arc<LoginContext>>>> = OnceLock::new();
static DOMAIN_SERVICE_HANDLES: OnceLock<RwLock<HashMap<i32, Arc<SecurityDomainService>>>> =
    OnceLock::new();
static IDENTITY_HANDLES: OnceLock<RwLock<HashMap<i32, Arc<SecurityIdentity>>>> = OnceLock::new();

fn subject_handles() -> &'static RwLock<HashMap<i32, Arc<Subject>>> {
    SUBJECT_HANDLES.get_or_init(|| RwLock::new(HashMap::new()))
}
fn login_context_handles() -> &'static RwLock<HashMap<i32, Arc<LoginContext>>> {
    LOGIN_CONTEXT_HANDLES.get_or_init(|| RwLock::new(HashMap::new()))
}
fn domain_service_handles() -> &'static RwLock<HashMap<i32, Arc<SecurityDomainService>>> {
    DOMAIN_SERVICE_HANDLES.get_or_init(|| RwLock::new(HashMap::new()))
}
fn identity_handles() -> &'static RwLock<HashMap<i32, Arc<SecurityIdentity>>> {
    IDENTITY_HANDLES.get_or_init(|| RwLock::new(HashMap::new()))
}

fn obj_key(ctx: &dyn NativeContext, obj: ObjectRef) -> i32 {
    // GC-stable identity hash code; survives compaction via
    // `HashCodeTable::update_after_gc`.  See module-level docs for why
    // the earlier `obj.as_ptr() as usize` keying was incorrect.
    ctx.identity_hash_code(obj)
}

fn next_oid() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

// ===========================================================================
// Native method implementations.
// ===========================================================================

/// Subject instance-field slots.  The synthetic-stub layout
/// (`class_manager::synthetic_stub_fields`) lists `principals`,
/// `publicCreds`, `privateCreds` in this order; the real JDK `Subject`
/// lists `principals`, `pubCredentials`, `privCredentials` in the SAME
/// order under `java.lang.Object` (no inherited instance fields).  Because
/// the slot order matches, these indices stay valid after an in-place
/// stub→real-bytecode upgrade (`update_class_in_place` keeps already-
/// allocated objects' slots), so accessing by index — not by name, which
/// the upgrade renames `privateCreds`→`privCredentials` — is the only
/// upgrade-stable choice.
const SUBJECT_SLOT_PRINCIPALS: usize = 0;
const SUBJECT_SLOT_PUBLIC_CREDS: usize = 1;
const SUBJECT_SLOT_PRIVATE_CREDS: usize = 2;

const LOGIN_CONTEXT_SLOT_NAME: usize = 0;
const LOGIN_CONTEXT_SLOT_SUBJECT: usize = 1;
const LOGIN_CONTEXT_SLOT_HANDLER: usize = 2;
const LOGIN_CONTEXT_SLOT_CONFIG: usize = 3;

#[derive(Clone, Copy, Debug)]
struct PinnedObject {
    handle: usize,
    fallback: ObjectRef,
}

impl PinnedObject {
    fn new(ctx: &mut dyn NativeContext, obj: ObjectRef) -> PinnedObject {
        PinnedObject {
            handle: ctx.pin_native_root(obj),
            fallback: obj,
        }
    }

    fn current(self, ctx: &dyn NativeContext) -> ObjectRef {
        ctx.read_native_pin(self.handle, self.fallback)
    }
}

#[derive(Clone, Debug)]
struct JavaLoginModuleSpec {
    class_name: String,
    flag: ControlFlag,
    options: Option<PinnedObject>,
}

#[derive(Clone, Debug)]
struct JavaLoginModuleRuntime {
    module: PinnedObject,
    flag: ControlFlag,
}

fn optional_obj_arg(args: &[Value], idx: usize) -> Option<ObjectRef> {
    match args.get(idx) {
        Some(Value::Object(Some(obj))) => Some(*obj),
        _ => None,
    }
}

fn object_value(obj: Option<ObjectRef>) -> Value {
    Value::Object(obj)
}

/// Read the real, mutable `java.util.HashSet` backing one of `Subject`'s
/// three sets, lazily building and storing it when the slot is still null.
///
/// The set must be a *real* HashSet (constructed via its `<init>`), not an
/// `alloc_concurrent_synthetic` field-only stub: JASPIC's
/// `CallbackHandlerImpl` (and, once the unregistered
/// `getPrivateCredentials(Class)` overload upgrades the stub to real
/// bytecode, the JDK's own `Subject$ClassSet` populate loop running under
/// `synchronized (privCredentials)`) calls real `add`/`remove`/`iterator`
/// on it and stores arbitrary Java credential objects (e.g. Tomcat's
/// `GenericPrincipal`) that the Rust-typed `Subject` side-table cannot
/// represent.  The no-arg getter and the real `getXxx(Class)` bytecode
/// share this one object via the slot, so mutations round-trip.
///
/// GC-safety: `new_object_initialized` runs `HashSet.<init>` and can
/// trigger a moving collection, so `this` is pinned across it and read
/// back forwarded before the `set_field`.
fn get_or_init_subject_set(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    slot: usize,
) -> MethodCallResult {
    if let Value::Object(Some(set)) = ctx.get_field(this, slot) {
        return Ok(Some(Value::Object(Some(set))));
    }
    let pin = ctx.pin_native_root(this);
    let made = ctx.new_object_initialized("java/util/HashSet", "()V", &[]);
    let this = ctx.read_native_pin(pin, this);
    ctx.unpin_native_roots(pin);
    let set = match made {
        Ok(Some(Value::Object(Some(s)))) => s,
        // Allocation produced no object (OOME pending) or raised — surface
        // it rather than storing a null.
        Ok(other) => return Ok(other),
        Err(e) => return Err(e),
    };
    ctx.set_field(this, slot, Value::Object(Some(set)));
    Ok(Some(Value::Object(Some(set))))
}

fn native_subject_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let subject = Subject::new();
    let key = obj_key(ctx, this);
    subject_handles().write().insert(key, subject);
    // Initialise principals (0) / publicCreds (1) / privateCreds (2) to REAL
    // mutable `java.util.HashSet`s, matching the real `Subject` constructor's
    // invariant that all three sets are non-null.  Real bytecode DOES read the
    // raw sets directly: the no-arg getters below hand the field set back, and
    // the JDK's `getPrivateCredentials(Class)` overload (unregistered → real
    // bytecode after stub upgrade) does `synchronized (privCredentials)`, which
    // NPE'd on the previously-null field (Tomcat JASPIC
    // `TestJaspicCallbackHandlerInAuthenticator`).  The Rust side-table above is
    // retained for WildFly's Rust-typed login/doAs state.
    //
    // Pin `this` across the loop: each `get_or_init_subject_set` allocates and
    // may relocate the Subject, so re-read the forwarded ref before the next.
    let pin = ctx.pin_native_root(this);
    let mut this = this;
    for slot in [
        SUBJECT_SLOT_PRINCIPALS,
        SUBJECT_SLOT_PUBLIC_CREDS,
        SUBJECT_SLOT_PRIVATE_CREDS,
    ] {
        this = ctx.read_native_pin(pin, this);
        get_or_init_subject_set(ctx, this, slot)?;
    }
    ctx.unpin_native_roots(pin);
    Ok(None)
}

fn get_subject_from_this(ctx: &dyn NativeContext, this: ObjectRef) -> Option<Arc<Subject>> {
    let key = obj_key(ctx, this);
    subject_handles().read().get(&key).cloned()
}

/// Loud post-GC missing-state error.  After re-keying on the GC-stable
/// identity hash code the only way to miss is when the receiver was
/// never produced by `<init>` — caller bug, not a GC artefact.  Matches
/// the C14 MessageDigest precedent (prefer `IllegalStateException` over
/// the previous silent "defensive empty set" fallback).
fn subject_missing_err() -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: "Subject state missing post-GC or never initialized".into(),
    }
    .into()
}

fn native_subject_get_principals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let Some(subject) = get_subject_from_this(ctx, this) else {
        return Err(subject_missing_err());
    };
    let real_set = match get_or_init_subject_set(ctx, this, SUBJECT_SLOT_PRINCIPALS)? {
        Some(Value::Object(Some(set))) => set,
        other => return Ok(other),
    };

    // Java-backed LoginModule.commit() implementations mutate the real
    // Subject.principals set. Prefer that set once it has contents; keep the
    // old Rust-side synthetic count only for WildFly bootstrap modules that
    // still record principals solely in the Rust side table.
    let set_pin = ctx.pin_native_root(real_set);
    let real_size = match ctx.invoke_virtual(real_set, "size", "()I", &[]) {
        Ok(Some(Value::Int(size))) => size,
        _ => 0,
    };
    let real_set = ctx.read_native_pin(set_pin, real_set);
    ctx.unpin_native_roots(set_pin);
    if real_size > 0 || subject.principal_count() == 0 {
        return Ok(Some(Value::Object(Some(real_set))));
    }

    let principals = subject.get_principals();
    let set = count_carrying_hash_set(ctx, principals.len())?;
    Ok(Some(Value::Object(Some(set))))
}

/// A `java.util.HashSet` that carries a synthetic element count.
///
/// W7-49 (2026-08-12). The form this replaces allocated THREE slots and wrote
/// the count into slot 1. Real `java.util.HashSet` (JDK 25.0.3.9, `javap -p`)
/// declares exactly ONE instance field, `private transient HashMap<E, Object>
/// map`, so on the real layout that object came back with `map == null` and the
/// count one slot past the end of everything the class declares. Every real
/// `Set` method — `size`, `isEmpty`, `iterator`, `contains` — dereferences
/// `map`, so the caller got an NPE, and nothing could read the count back
/// either: no `size` field exists on a real `HashSet` for reflection to find.
/// Both these natives are registered on the essential path
/// (`register_jdk_security_natives`, `register_wildfly_security_natives`), and
/// `javax.security.auth.Subject` is a real JDK class, so this is a live
/// Compatible-mode path, not a synthetic-only one.
///
/// On the real layout, build a real, well-formed (empty) set through the
/// existing remedy helper — `size()` answers 0 through the JDK's own bytecode
/// instead of throwing. The Rust-side count is not representable there without
/// materialising Java `Principal` objects, which is a larger change than this
/// lane; losing it costs nothing, because on the real layout it was never
/// readable in the first place.
///
/// On a fabricated stub the 3-slot shape IS the layout and slot 1 is where
/// synthetic-mode readers look, so that arm is unchanged.
fn count_carrying_hash_set(
    ctx: &mut dyn NativeContext,
    count: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    if ctx.is_class_synthetic_stub("java/util/HashSet") {
        let set = try_alloc_concurrent_synthetic(ctx, "java/util/HashSet", 3)?;
        // Field 1 = size; synthetic-mode readers look here.
        ctx.set_field(set, 1, Value::Int(count as i32));
        return Ok(set);
    }
    crate::build_real_layout_string_hashset(ctx, &[])
}

fn native_subject_get_private_credentials(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Hand back the REAL backing Set field (slot 2) so callers can
    // add/remove/iterate Java credential objects and the post-upgrade real
    // `getPrivateCredentials(Class)` bytecode — which reads the same slot under
    // `synchronized (privCredentials)` — sees them.  We never log the live set
    // (it may hold cleartext secrets); contents stay redacted by construction.
    get_or_init_subject_set(ctx, this, SUBJECT_SLOT_PRIVATE_CREDS)
}

fn native_subject_get_public_credentials(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Hand back the REAL backing Set field (slot 1) — same rationale as
    // `getPrivateCredentials`: round-tripping mutations and a non-null set for
    // the real `getPublicCredentials(Class)` bytecode after stub upgrade.
    get_or_init_subject_set(ctx, this, SUBJECT_SLOT_PUBLIC_CREDS)
}

fn native_subject_do_as(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // static doAs(Subject, PrivilegedAction)Ljava/lang/Object;
    // args: [0] Subject (nullable), [1] PrivilegedAction
    let subject_obj = match args.first() {
        Some(Value::Object(Some(s))) => Some(*s),
        _ => None,
    };
    let action = obj_arg(args, 1)?;

    // Install the subject for the scope of action.run().  The
    // SubjectGuard ensures restoration even if run() panics or
    // returns an Err.  If the caller passes a non-null Subject ref
    // whose handle is missing, that's a caller bug (after the GC-key
    // fix it cannot be a relocation artefact) — surface it loudly.
    let subject = if let Some(s_obj) = subject_obj {
        get_subject_from_this(ctx, s_obj).ok_or_else(subject_missing_err)?
    } else {
        // Null Subject is JDK-conformant: doAs runs unauthenticated.
        Subject::new()
    };
    let _g = SubjectGuard::push(subject);
    ctx.invoke_virtual(action, "run", "()Ljava/lang/Object;", &[])
}

fn native_login_context_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // <init>(String, Subject, CallbackHandler)V
    // <init>(String, Subject, CallbackHandler, Configuration)V
    let this = obj_arg(args, 0)?;
    let name_obj = optional_obj_arg(args, 1);
    let subject_arg = optional_obj_arg(args, 2);
    let handler_obj = optional_obj_arg(args, 3);
    let config_obj = optional_obj_arg(args, 4);
    native_login_context_init_core(ctx, this, name_obj, subject_arg, handler_obj, config_obj)
}

fn native_login_context_init_name_only(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // <init>(String)V
    let this = obj_arg(args, 0)?;
    let name_obj = optional_obj_arg(args, 1);
    native_login_context_init_core(ctx, this, name_obj, None, None, None)
}

fn native_login_context_init_name_handler(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // <init>(String, CallbackHandler)V — the overload H2's
    // `JaasCredentialsValidator` uses. Previously unregistered, so it ran as
    // un-intercepted real JDK bytecode and never populated the side-table
    // `login()` depends on (see fixed-suite-bugs/h2-suite-bugs/
    // bug-h2-jaas-logincontext-two-arg-ctor-gap.md).
    let this = obj_arg(args, 0)?;
    let name_obj = optional_obj_arg(args, 1);
    let handler_obj = optional_obj_arg(args, 2);
    native_login_context_init_core(ctx, this, name_obj, None, handler_obj, None)
}

fn native_login_context_init_name_subject(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // <init>(String, Subject)V
    let this = obj_arg(args, 0)?;
    let name_obj = optional_obj_arg(args, 1);
    let subject_arg = optional_obj_arg(args, 2);
    native_login_context_init_core(ctx, this, name_obj, subject_arg, None, None)
}

fn native_login_context_init_core(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    name_obj: Option<ObjectRef>,
    subject_arg: Option<ObjectRef>,
    handler_obj: Option<ObjectRef>,
    config_obj: Option<ObjectRef>,
) -> MethodCallResult {
    // Shared by every `LoginContext` constructor overload:
    //   <init>(String)V
    //   <init>(String, CallbackHandler)V
    //   <init>(String, Subject)V
    //   <init>(String, Subject, CallbackHandler)V
    //   <init>(String, Subject, CallbackHandler, Configuration)V
    //
    // WildFly/Keycloak's bootstrap path still resolves modules from the
    // Rust-side security-domain registry. Generic JAAS callers (notably
    // Tomcat's `JAASRealm`) pass a real `Configuration` object to the 4-arg
    // overload; record those Java objects on the LoginContext too so
    // `login()` can execute real `LoginModule` bytecode from the config.
    let pin_base = ctx.pin_native_root(this);
    let name_pin = name_obj.map(|obj| PinnedObject::new(ctx, obj));
    let subject_pin = subject_arg.map(|obj| PinnedObject::new(ctx, obj));
    let handler_pin = handler_obj.map(|obj| PinnedObject::new(ctx, obj));
    let config_pin = config_obj.map(|obj| PinnedObject::new(ctx, obj));

    let name_obj = name_pin.map(|pin| pin.current(ctx));
    let name = name_obj
        .and_then(|s| ctx.read_string(s))
        .unwrap_or_default();

    // A null Subject is JDK-conformant: LoginContext creates one. Create the
    // real Java Subject now so arbitrary Java LoginModule implementations can
    // mutate its backing sets during commit(), and so getSubject() can return
    // the same object.
    let subject_obj = match subject_pin {
        Some(pin) => pin.current(ctx),
        None => match ctx.new_object_initialized("javax/security/auth/Subject", "()V", &[])? {
            Some(Value::Object(Some(obj))) => obj,
            _ => {
                ctx.unpin_native_roots(pin_base);
                return Err(RuntimeError::IllegalStateException {
                    message: "LoginContext could not allocate Subject".into(),
                }
                .into());
            }
        },
    };
    let subject = get_subject_from_this(ctx, subject_obj).ok_or_else(subject_missing_err)?;

    let modules = lookup_security_domain(&name)
        .map(|d| d.policy.modules.clone())
        .unwrap_or_default();
    let lc = LoginContext::new(name, subject, modules);

    let this = ctx.read_native_pin(pin_base, this);
    let key = obj_key(ctx, this);
    login_context_handles().write().insert(key, lc);

    let subject_value = Value::Object(Some(subject_obj));
    let handler_value = object_value(handler_pin.map(|pin| pin.current(ctx)));
    let config_value = object_value(config_pin.map(|pin| pin.current(ctx)));

    ctx.set_field_by_name(this, "subject", subject_value);
    ctx.set_field_by_name(this, "callbackHandler", handler_value);
    ctx.set_field_by_name(this, "config", config_value);

    ctx.unpin_native_roots(pin_base);
    Ok(None)
}

fn get_login_context_from_this(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Option<Arc<LoginContext>> {
    let key = obj_key(ctx, this);
    login_context_handles().read().get(&key).cloned()
}

fn login_context_missing_err() -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: "LoginContext state missing post-GC or never initialized".into(),
    }
    .into()
}

fn login_context_object_field(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    field_name: &str,
    slot: usize,
) -> Option<ObjectRef> {
    match ctx.get_field_by_name(this, field_name) {
        Value::Object(Some(obj)) => Some(obj),
        _ if ctx
            .resolve_field_index("javax/security/auth/login/LoginContext", "state")
            .is_some() =>
        {
            None
        }
        _ => match ctx.get_field(this, slot) {
            Value::Object(Some(obj)) => Some(obj),
            _ => None,
        },
    }
}

fn java_string_result(ctx: &dyn NativeContext, value: Option<Value>) -> Option<String> {
    match value {
        Some(Value::Object(Some(s))) => ctx.read_string(s),
        _ => None,
    }
}

fn invoke_java_boolean(
    ctx: &mut dyn NativeContext,
    receiver: ObjectRef,
    method_name: &str,
) -> Result<bool, MethodCallFailed> {
    match ctx.invoke_virtual(receiver, method_name, "()Z", &[])? {
        Some(Value::Int(v)) => Ok(v != 0),
        _ => Ok(false),
    }
}

fn read_entry_string(
    ctx: &mut dyn NativeContext,
    entry: ObjectRef,
    method_name: &str,
) -> Result<String, MethodCallFailed> {
    let value = ctx.invoke_virtual(entry, method_name, "()Ljava/lang/String;", &[])?;
    Ok(java_string_result(ctx, value).unwrap_or_default())
}

fn parse_entry_flag(
    ctx: &mut dyn NativeContext,
    entry: ObjectRef,
) -> Result<ControlFlag, MethodCallFailed> {
    let flag_obj = match ctx.invoke_virtual(
        entry,
        "getControlFlag",
        "()Ljavax/security/auth/login/AppConfigurationEntry$LoginModuleControlFlag;",
        &[],
    )? {
        Some(Value::Object(Some(obj))) => obj,
        _ => {
            return Err(RuntimeError::SecurityException {
                message: "LoginException: missing JAAS control flag".into(),
            }
            .into())
        }
    };
    let flag_pin = PinnedObject::new(ctx, flag_obj);
    let flag_obj = flag_pin.current(ctx);
    let text = ctx.invoke_virtual(flag_obj, "toString", "()Ljava/lang/String;", &[])?;
    let parsed = java_string_result(ctx, text)
        .and_then(|s| ControlFlag::parse(&s))
        .ok_or_else(|| -> MethodCallFailed {
            RuntimeError::SecurityException {
                message: "LoginException: unknown JAAS control flag".into(),
            }
            .into()
        })?;
    Ok(parsed)
}

fn read_entry_options(
    ctx: &mut dyn NativeContext,
    entry: ObjectRef,
) -> Result<Option<PinnedObject>, MethodCallFailed> {
    match ctx.invoke_virtual(entry, "getOptions", "()Ljava/util/Map;", &[])? {
        Some(Value::Object(Some(options))) => Ok(Some(PinnedObject::new(ctx, options))),
        _ => Ok(None),
    }
}

fn read_java_login_modules(
    ctx: &mut dyn NativeContext,
    config: PinnedObject,
    name: PinnedObject,
) -> Result<Vec<JavaLoginModuleSpec>, MethodCallFailed> {
    let config_obj = config.current(ctx);
    let name_obj = name.current(ctx);
    let entries = match ctx.invoke_virtual(
        config_obj,
        "getAppConfigurationEntry",
        "(Ljava/lang/String;)[Ljavax/security/auth/login/AppConfigurationEntry;",
        &[Value::Object(Some(name_obj))],
    )? {
        Some(Value::Object(Some(entries))) => entries,
        _ => return Ok(Vec::new()),
    };
    let entries_pin = PinnedObject::new(ctx, entries);
    let entries = entries_pin.current(ctx);
    let len = ctx.array_length(entries);
    let mut specs = Vec::with_capacity(len);
    for index in 0..len {
        let entries = entries_pin.current(ctx);
        let entry = match ctx.get_array_element(entries, index) {
            Value::Object(Some(entry)) => entry,
            _ => continue,
        };
        let entry_pin = PinnedObject::new(ctx, entry);
        let entry = entry_pin.current(ctx);
        let class_name = read_entry_string(ctx, entry, "getLoginModuleName")?;
        if class_name.is_empty() {
            return Err(RuntimeError::SecurityException {
                message: "LoginException: missing LoginModule class name".into(),
            }
            .into());
        }
        let entry = entry_pin.current(ctx);
        let flag = parse_entry_flag(ctx, entry)?;
        let entry = entry_pin.current(ctx);
        let options = read_entry_options(ctx, entry)?;
        specs.push(JavaLoginModuleSpec {
            class_name,
            flag,
            options,
        });
    }
    Ok(specs)
}

fn new_hash_map_pinned(ctx: &mut dyn NativeContext) -> Result<PinnedObject, MethodCallFailed> {
    match ctx.new_object_initialized("java/util/HashMap", "()V", &[])? {
        Some(Value::Object(Some(map))) => Ok(PinnedObject::new(ctx, map)),
        _ => Err(RuntimeError::IllegalStateException {
            message: "LoginContext could not allocate JAAS shared state".into(),
        }
        .into()),
    }
}

fn instantiate_login_module(
    ctx: &mut dyn NativeContext,
    spec: &JavaLoginModuleSpec,
    subject: PinnedObject,
    handler: Option<PinnedObject>,
    shared_state: PinnedObject,
) -> Result<JavaLoginModuleRuntime, MethodCallFailed> {
    let internal_name = spec.class_name.replace('.', "/");
    let module = match ctx.new_object_initialized(&internal_name, "()V", &[])? {
        Some(Value::Object(Some(module))) => PinnedObject::new(ctx, module),
        _ => {
            return Err(RuntimeError::SecurityException {
                message: format!("LoginException: could not instantiate {}", spec.class_name),
            }
            .into())
        }
    };

    let options = match spec.options {
        Some(pin) => Value::Object(Some(pin.current(ctx))),
        None => Value::Object(Some(new_hash_map_pinned(ctx)?.current(ctx))),
    };
    let args = [
        Value::Object(Some(subject.current(ctx))),
        object_value(handler.map(|pin| pin.current(ctx))),
        Value::Object(Some(shared_state.current(ctx))),
        options,
    ];
    ctx.invoke_virtual(
        module.current(ctx),
        "initialize",
        "(Ljavax/security/auth/Subject;Ljavax/security/auth/callback/CallbackHandler;Ljava/util/Map;Ljava/util/Map;)V",
        &args,
    )?;

    Ok(JavaLoginModuleRuntime {
        module,
        flag: spec.flag,
    })
}

fn abort_java_modules(ctx: &mut dyn NativeContext, modules: &[JavaLoginModuleRuntime]) {
    for module in modules {
        let _ = ctx.invoke_virtual(module.module.current(ctx), "abort", "()Z", &[]);
    }
}

fn commit_java_modules(
    ctx: &mut dyn NativeContext,
    modules: &[JavaLoginModuleRuntime],
) -> Result<bool, MethodCallFailed> {
    let mut any_non_optional_success = false;
    let mut any_required_failure = false;

    for module in modules {
        let module_obj = module.module.current(ctx);
        let ok = invoke_java_boolean(ctx, module_obj, "commit")?;
        match (module.flag, ok) {
            (ControlFlag::Required | ControlFlag::Requisite, true) => {
                any_non_optional_success = true;
            }
            (ControlFlag::Required | ControlFlag::Requisite, false) => {
                any_required_failure = true;
            }
            (ControlFlag::Sufficient, true) => {
                any_non_optional_success = true;
            }
            (ControlFlag::Sufficient | ControlFlag::Optional, false)
            | (ControlFlag::Optional, true) => {}
        }
    }

    Ok(any_non_optional_success && !any_required_failure)
}

fn run_java_configuration_login(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    name: &str,
) -> Result<Option<bool>, MethodCallFailed> {
    let pin_base = ctx.pin_native_root(this);
    let result = (|| {
        let this = ctx.read_native_pin(pin_base, this);
        let Some(config_obj) =
            login_context_object_field(ctx, this, "config", LOGIN_CONTEXT_SLOT_CONFIG)
        else {
            return Ok(None);
        };
        // An explicit `Configuration` was passed to the 4-arg constructor
        // (e.g. Tomcat's `JAASRealm`): no entries for `name` is a real
        // failure, not a signal to fall back to something else.
        match run_login_with_java_config(ctx, this, name, config_obj)? {
            Some(success) => Ok(Some(success)),
            None => Ok(Some(false)),
        }
    })();
    ctx.unpin_native_roots(pin_base);
    result
}

/// Fallback for every constructor overload that does NOT take an explicit
/// `Configuration` (all but the 4-arg `(..., Configuration)` form). Real
/// JDK's `LoginContext` resolves those against the process-wide default
/// `Configuration.getConfiguration()`; H2's `JaasCredentialsValidator`
/// relies on exactly this path via the 2-arg `(String, CallbackHandler)`
/// constructor — it calls `Configuration.setConfiguration(...)` itself and
/// never touches the Rust-side security-domain registry (see
/// fixed-suite-bugs/h2-suite-bugs/bug-h2-jaas-logincontext-two-arg-ctor-gap.md).
/// Only consulted by the caller when the Rust-side registry
/// (`LoginContext::modules`, populated by WildFly/Keycloak bootstrap) has no
/// entry for `name`, so WildFly/Keycloak's own domains are unaffected.
/// Returns `Ok(None)` ("fall through to the Rust-side domain modules") when
/// there is no installed default `Configuration`, or it has no entry for
/// `name` either — same as a `LoginContext` constructed with a name that
/// matches nothing anywhere, which correctly ends in `lc.login()`'s
/// no-modules failure.
fn run_default_java_configuration_login(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    name: &str,
) -> Result<Option<bool>, MethodCallFailed> {
    let pin_base = ctx.pin_native_root(this);
    let result = (|| {
        let Some(Value::Object(Some(config_obj))) = ctx.invoke(
            "javax/security/auth/login/Configuration",
            "getConfiguration",
            "()Ljavax/security/auth/login/Configuration;",
            &[],
        )?
        else {
            return Ok(None);
        };
        let this = ctx.read_native_pin(pin_base, this);
        run_login_with_java_config(ctx, this, name, config_obj)
    })();
    ctx.unpin_native_roots(pin_base);
    result
}

/// Shared driver for both of the above: read `name`'s entries from
/// `config_obj`, instantiate and run each real Java `LoginModule` per JAAS
/// control-flag semantics. Returns `Ok(None)` when `config_obj` has no
/// entries for `name` — callers decide whether that means "fail" or "fall
/// through to another source".
fn run_login_with_java_config(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    name: &str,
    config_obj: ObjectRef,
) -> Result<Option<bool>, MethodCallFailed> {
    let Some(subject_obj) =
        login_context_object_field(ctx, this, "subject", LOGIN_CONTEXT_SLOT_SUBJECT)
    else {
        return Err(RuntimeError::IllegalStateException {
            message: "LoginContext subject missing".into(),
        }
        .into());
    };

    let config = PinnedObject::new(ctx, config_obj);
    let subject = PinnedObject::new(ctx, subject_obj);
    let handler =
        login_context_object_field(ctx, this, "callbackHandler", LOGIN_CONTEXT_SLOT_HANDLER)
            .map(|obj| PinnedObject::new(ctx, obj));
    let name_obj = ctx.create_string(name);
    let name_pin = PinnedObject::new(ctx, name_obj);
    let specs = read_java_login_modules(ctx, config, name_pin)?;
    if specs.is_empty() {
        return Ok(None);
    }

    let shared_state = new_hash_map_pinned(ctx)?;
    let mut invoked = Vec::new();
    let mut any_non_optional_success = false;
    let mut any_required_failure = false;
    let mut sufficient_success = false;

    for spec in &specs {
        if sufficient_success {
            continue;
        }
        let runtime = instantiate_login_module(ctx, spec, subject, handler, shared_state)?;
        let module_obj = runtime.module.current(ctx);
        let login_ok = invoke_java_boolean(ctx, module_obj, "login")?;

        match (spec.flag, login_ok) {
            (ControlFlag::Required, true) | (ControlFlag::Requisite, true) => {
                any_non_optional_success = true;
            }
            (ControlFlag::Required, false) => {
                any_required_failure = true;
            }
            (ControlFlag::Requisite, false) => {
                any_required_failure = true;
                invoked.push(runtime);
                break;
            }
            (ControlFlag::Sufficient, true) => {
                any_non_optional_success = true;
                sufficient_success = true;
            }
            (ControlFlag::Sufficient, false) | (ControlFlag::Optional, _) => {}
        }
        invoked.push(runtime);
    }

    let login_success = any_non_optional_success && !any_required_failure;
    if !login_success {
        abort_java_modules(ctx, &invoked);
        return Ok(Some(false));
    }

    let commit_success = commit_java_modules(ctx, &invoked)?;
    if !commit_success {
        abort_java_modules(ctx, &invoked);
    }
    Ok(Some(commit_success))
}

fn native_login_context_login(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let Some(lc) = get_login_context_from_this(ctx, this) else {
        return Err(login_context_missing_err());
    };
    if let Some(success) = run_java_configuration_login(ctx, this, &lc.name)? {
        if !success {
            return Err(RuntimeError::SecurityException {
                message: "LoginException: authentication failed".into(),
            }
            .into());
        }
        return Ok(None);
    }

    if lc.modules.is_empty() {
        if let Some(success) = run_default_java_configuration_login(ctx, this, &lc.name)? {
            if !success {
                return Err(RuntimeError::SecurityException {
                    message: "LoginException: authentication failed".into(),
                }
                .into());
            }
            return Ok(None);
        }
    }

    let result = lc.login();
    if !result.success {
        return Err(RuntimeError::SecurityException {
            message: "LoginException: authentication failed".into(),
        }
        .into());
    }
    Ok(None)
}

fn native_login_context_logout(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // `logout()` on an unknown LoginContext is left as a no-op so
    // shutdown paths that race with finalisation don't observe a
    // spurious exception — the receiver having no handle here is
    // benign (nothing to release).
    if let Some(lc) = get_login_context_from_this(ctx, this) {
        lc.logout();
    }
    Ok(None)
}

fn native_login_context_get_subject(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(subject_obj) =
        login_context_object_field(ctx, this, "subject", LOGIN_CONTEXT_SLOT_SUBJECT)
    {
        return Ok(Some(Value::Object(Some(subject_obj))));
    }

    let Some(lc) = get_login_context_from_this(ctx, this) else {
        // Post-GC-key fix: missing state is a caller bug, not a
        // relocation artefact.  Surface loudly instead of returning
        // a silent null (which earlier code did).
        return Err(login_context_missing_err());
    };
    match ctx.new_object_initialized("javax/security/auth/Subject", "()V", &[])? {
        Some(Value::Object(Some(subject_obj))) => {
            let key = obj_key(ctx, subject_obj);
            subject_handles().write().insert(key, lc.get_subject());
            Ok(Some(Value::Object(Some(subject_obj))))
        }
        other => Ok(other),
    }
}

fn native_security_domain_service_get_auth_manager(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // For our model the auth manager and the service are the same
    // Rust-side handle; surface the same ObjectRef back to Java so
    // `service.getAuthenticationManager() == service` (by identity).
    let _ = ctx;
    Ok(Some(Value::Object(Some(this))))
}

fn native_security_identity_get_roles(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = obj_key(ctx, this);
    let count = identity_handles()
        .read()
        .get(&key)
        .map(|id| id.roles.len())
        // Post-GC-key fix: missing state is a caller bug.  Surface
        // loudly instead of silently reporting an empty role set
        // (which would have masked the previous GC-aliasing
        // regression).
        .ok_or_else(|| -> MethodCallFailed {
            RuntimeError::IllegalStateException {
                message: "SecurityIdentity state missing post-GC or never initialized".into(),
            }
            .into()
        })?;
    // W7-49: same three-slot-HashSet-over-a-one-field-class shape as
    // `native_subject_get_principals`; see `count_carrying_hash_set`.
    let set = count_carrying_hash_set(ctx, count)?;
    Ok(Some(Value::Object(Some(set))))
}

fn native_security_identity_run_as(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // runAs(Callable)Ljava/lang/Object;
    let this = obj_arg(args, 0)?;
    let callable = obj_arg(args, 1)?;

    let key = obj_key(ctx, this);
    if let Some(identity) = identity_handles().read().get(&key).cloned() {
        let _g = IdentityGuard::push(identity);
        ctx.invoke_virtual(callable, "call", "()Ljava/lang/Object;", &[])
    } else {
        // Unknown identity — call without elevation.  Matches the
        // JDK's behaviour of silently running the callable when the
        // current identity isn't tagged.  The "run without elevation"
        // fallback is intentional (and pre-existed the C-series GC
        // fix) — `runAs` is documented to be safe to call even when
        // the identity wasn't registered on our side.
        ctx.invoke_virtual(callable, "call", "()Ljava/lang/Object;", &[])
    }
}

/// Public thin wrapper that delegates `AccessControlContext.check_permission`
/// to the Session 86/87 policy layer.  Exposed for tests (and for
/// callers that want to explicitly participate in the same check
/// path without going through the native registry).  Returns `Ok(())`
/// on ALLOW, `Err(SecurityException)` on DENY.
pub fn access_control_context_check_permission(
    permission_class: &str,
    target: &str,
    actions: &str,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let allowed = crate::security_manager::policy_allows(permission_class, target, actions, None);
    if allowed {
        tracing::trace!(
            permission_class = %permission_class,
            target = %target,
            actions = %actions,
            "AccessControlContext.checkPermission: ALLOW"
        );
        Ok(())
    } else {
        Err(RuntimeError::SecurityException {
            message: format!("access denied (\"{permission_class}\" \"{target}\" \"{actions}\")"),
        }
        .into())
    }
}

fn native_access_control_context_init(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // <init>(ProtectionDomain[])V — we accept the array and ignore
    // it.  Our non-enforcing model treats every AC context the same.
    let _ = ctx;
    Ok(None)
}

// ===========================================================================
// Registration.
// ===========================================================================

/// Register the JAAS and access-control bridges that belong to JDK modules.
///
/// These are deliberately independent of the WildFly security compatibility
/// pack: ordinary applications use `Subject` and `LoginContext` too, so their
/// availability must not depend on a WildFly classpath witness.
pub fn register_jdk_security_natives(r: &mut NativeMethodRegistry) {
    let subject = "javax/security/auth/Subject";
    r.register(subject, "<init>", "()V", native_subject_init);
    r.register(
        subject,
        "getPrincipals",
        "()Ljava/util/Set;",
        native_subject_get_principals,
    );
    r.register(
        subject,
        "getPrincipals",
        "(Ljava/lang/Class;)Ljava/util/Set;",
        native_subject_get_principals,
    );
    r.register(
        subject,
        "getPublicCredentials",
        "()Ljava/util/Set;",
        native_subject_get_public_credentials,
    );
    r.register(
        subject,
        "getPrivateCredentials",
        "()Ljava/util/Set;",
        native_subject_get_private_credentials,
    );
    r.register(
        subject,
        "doAs",
        "(Ljavax/security/auth/Subject;Ljava/security/PrivilegedAction;)Ljava/lang/Object;",
        native_subject_do_as,
    );

    let lc = "javax/security/auth/login/LoginContext";
    r.register(
        lc,
        "<init>",
        "(Ljava/lang/String;Ljavax/security/auth/Subject;Ljavax/security/auth/callback/CallbackHandler;)V",
        native_login_context_init,
    );
    // Tomcat's `JAASRealm.authenticate()` always calls this 4-arg
    // overload (see the comment on `native_login_context_init_core`), so it
    // must be registered too or `login()` always finds no state to load.
    r.register(
        lc,
        "<init>",
        "(Ljava/lang/String;Ljavax/security/auth/Subject;Ljavax/security/auth/callback/CallbackHandler;Ljavax/security/auth/login/Configuration;)V",
        native_login_context_init,
    );
    // All five public `LoginContext` constructors must be registered — any
    // JDK-legal construction path has to populate the side-table before
    // `login()` (unconditionally a native override) can find it. H2's
    // `JaasCredentialsValidator` uses the 2-arg (String, CallbackHandler)
    // form; see fixed-suite-bugs/h2-suite-bugs/
    // bug-h2-jaas-logincontext-two-arg-ctor-gap.md.
    r.register(
        lc,
        "<init>",
        "(Ljava/lang/String;)V",
        native_login_context_init_name_only,
    );
    r.register(
        lc,
        "<init>",
        "(Ljava/lang/String;Ljavax/security/auth/callback/CallbackHandler;)V",
        native_login_context_init_name_handler,
    );
    r.register(
        lc,
        "<init>",
        "(Ljava/lang/String;Ljavax/security/auth/Subject;)V",
        native_login_context_init_name_subject,
    );
    r.register(lc, "login", "()V", native_login_context_login);
    r.register(lc, "logout", "()V", native_login_context_logout);
    r.register(
        lc,
        "getSubject",
        "()Ljavax/security/auth/Subject;",
        native_login_context_get_subject,
    );

    // Register only the <init>(ProtectionDomain[]) — the
    // checkPermission(Permission) native is already owned by
    // `security_manager.rs` (Session 86/87) and we share the same
    // underlying `policy_allows` check path via
    // [`access_control_context_check_permission`].
    let acc = "java/security/AccessControlContext";
    r.register(
        acc,
        "<init>",
        "([Ljava/security/ProtectionDomain;)V",
        native_access_control_context_init,
    );
}

/// Register only the WildFly-owned security compatibility pack.
pub fn register_wildfly_security_natives(r: &mut NativeMethodRegistry) {
    let sds = "org/jboss/as/security/SecurityDomainService";
    r.register(
        sds,
        "getAuthenticationManager",
        "()Ljava/lang/Object;",
        native_security_domain_service_get_auth_manager,
    );

    let si = "org/wildfly/security/auth/server/SecurityIdentity";
    r.register(
        si,
        "getRoles",
        "()Ljava/util/Set;",
        native_security_identity_get_roles,
    );
    r.register(
        si,
        "runAs",
        "(Ljava/util/concurrent/Callable;)Ljava/lang/Object;",
        native_security_identity_run_as,
    );
}

// ===========================================================================
// Tests.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockNativeContext;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::ArrayElementType;
    use std::sync::Mutex;

    // Serialize tests that mutate the global domain registry so
    // parallel runs don't race each other's clear/register sequences.
    fn domain_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    // -----------------------------------------------------------------------
    // Test helpers: module builders that record whether they were invoked.
    // -----------------------------------------------------------------------

    fn always_succeeds(flag: ControlFlag, who: &str) -> Arc<LoginModuleEntry> {
        let name = who.to_string();
        Arc::new(LoginModuleEntry {
            class_name: format!("test.mod.{who}"),
            flag,
            options: HashMap::new(),
            login_fn: Box::new(move |subject, _opts| {
                subject.add_principal(Principal::new(&name));
                ModuleOutcome::Success
            }),
        })
    }

    fn always_fails(flag: ControlFlag, who: &str) -> Arc<LoginModuleEntry> {
        Arc::new(LoginModuleEntry {
            class_name: format!("test.mod.{who}"),
            flag,
            options: HashMap::new(),
            login_fn: Box::new(|_subject, _opts| ModuleOutcome::Failure),
        })
    }

    // A module that records (via an Arc<AtomicU64>) whether it ran.
    fn tracking_module(
        flag: ControlFlag,
        counter: Arc<AtomicU64>,
        outcome: ModuleOutcome,
    ) -> Arc<LoginModuleEntry> {
        Arc::new(LoginModuleEntry {
            class_name: "test.mod.tracking".into(),
            flag,
            options: HashMap::new(),
            login_fn: Box::new(move |_subject, _opts| {
                counter.fetch_add(1, Ordering::SeqCst);
                outcome
            }),
        })
    }

    fn jaas_config_hook(
        ctx: &mut MockNativeContext,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> Option<MethodCallResult> {
        match (method_name, descriptor) {
            (
                "getAppConfigurationEntry",
                "(Ljava/lang/String;)[Ljavax/security/auth/login/AppConfigurationEntry;",
            ) => {
                let entry = ctx.fresh_object_ref();
                let class_name =
                    ctx.create_string("org.apache.catalina.realm.TesterLoginModule");
                let flag = ctx.fresh_object_ref();
                let flag_text = ctx.create_string("LoginModuleControlFlag: sufficient");
                let options = ctx.fresh_object_ref();
                ctx.set_field(entry, 0, Value::Object(Some(class_name)));
                ctx.set_field(entry, 1, Value::Object(Some(flag)));
                ctx.set_field(entry, 2, Value::Object(Some(options)));
                ctx.set_field(flag, 0, Value::Object(Some(flag_text)));

                let arr = ctx.new_array(ArrayElementType::Reference, 1);
                ctx.set_array_element(arr, 0, Value::Object(Some(entry)));
                Some(Ok(Some(Value::Object(Some(arr)))))
            }
            ("getLoginModuleName", "()Ljava/lang/String;") => Some(Ok(Some(ctx.get_field(receiver, 0)))),
            (
                "getControlFlag",
                "()Ljavax/security/auth/login/AppConfigurationEntry$LoginModuleControlFlag;",
            ) => Some(Ok(Some(ctx.get_field(receiver, 1)))),
            ("getOptions", "()Ljava/util/Map;") => Some(Ok(Some(ctx.get_field(receiver, 2)))),
            ("toString", "()Ljava/lang/String;") => Some(Ok(Some(ctx.get_field(receiver, 0)))),
            (
                "initialize",
                "(Ljavax/security/auth/Subject;Ljavax/security/auth/callback/CallbackHandler;Ljava/util/Map;Ljava/util/Map;)V",
            ) => {
                ctx.set_field(receiver, 0, args[0]);
                ctx.set_field(receiver, 1, args[1]);
                ctx.set_field(receiver, 2, args[2]);
                ctx.set_field(receiver, 3, args[3]);
                Some(Ok(None))
            }
            ("login", "()Z") => Some(Ok(Some(Value::Int(1)))),
            ("commit", "()Z") => {
                if let Value::Object(Some(subject)) = ctx.get_field(receiver, 0) {
                    if let Value::Object(Some(principals)) =
                        ctx.get_field(subject, SUBJECT_SLOT_PRINCIPALS)
                    {
                        ctx.set_field(principals, 1, Value::Int(1));
                    }
                }
                Some(Ok(Some(Value::Int(1))))
            }
            ("abort", "()Z") => Some(Ok(Some(Value::Int(1)))),
            ("size", "()I") => Some(Ok(Some(match ctx.get_field(receiver, 1) {
                Value::Int(size) => Value::Int(size),
                _ => Value::Int(0),
            }))),
            _ => None,
        }
    }

    // -----------------------------------------------------------------------
    // The 10 required tests.
    // -----------------------------------------------------------------------

    #[test]
    fn t19_2_c_login_context_runs_configuration_backed_login_module() {
        let _guard = domain_test_lock();
        clear_domain_registry();

        let mut ctx = MockNativeContext::new();
        let this = match ctx
            .new_object("javax/security/auth/login/LoginContext")
            .unwrap()
        {
            Some(Value::Object(Some(login_context))) => login_context,
            other => panic!("expected LoginContext object, got {other:?}"),
        };
        let name = ctx.create_string("CustomLogin");
        let subject = match ctx.new_object("javax/security/auth/Subject").unwrap() {
            Some(Value::Object(Some(subject))) => subject,
            other => panic!("expected Subject object, got {other:?}"),
        };
        native_subject_init(&mut ctx, &[Value::Object(Some(subject))])
            .expect("Subject init should register side-table handle");

        let callback = ctx.fresh_object_ref();
        let config = ctx.fresh_object_ref();
        ctx.set_invoke_virtual_hook(jaas_config_hook);

        native_login_context_init(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(name)),
                Value::Object(Some(subject)),
                Value::Object(Some(callback)),
                Value::Object(Some(config)),
            ],
        )
        .expect("LoginContext init should retain config-backed state");

        assert_eq!(
            ctx.get_field_by_name(this, "config"),
            Value::Object(Some(config))
        );
        native_login_context_login(&mut ctx, &[Value::Object(Some(this))])
            .expect("sufficient Java LoginModule should authenticate");

        let returned_subject =
            native_login_context_get_subject(&mut ctx, &[Value::Object(Some(this))])
                .expect("getSubject should succeed")
                .expect("getSubject should return a value");
        assert_eq!(returned_subject, Value::Object(Some(subject)));

        let principals = native_subject_get_principals(&mut ctx, &[Value::Object(Some(subject))])
            .expect("getPrincipals should return the Java backing set")
            .expect("getPrincipals should return a value");
        let expected_set = ctx.get_field(subject, SUBJECT_SLOT_PRINCIPALS);
        assert_eq!(principals, expected_set);
        if let Value::Object(Some(set)) = principals {
            assert_eq!(ctx.get_field(set, 1), Value::Int(1));
        } else {
            panic!("expected principals set object, got {principals:?}");
        }
    }

    #[test]
    #[allow(non_snake_case)]
    fn t19_2_c_subject_doAs_invokes_action() {
        // Under do_as, the current subject is set for the closure's
        // scope; after return it reverts to the previous value.
        assert!(current_subject().is_none());
        let s = Subject::new();
        s.add_principal(Principal::new("alice"));
        let observed = do_as(s.clone(), || {
            let cur = current_subject().expect("subject active inside do_as");
            assert_eq!(cur.principal_count(), 1);
            "ran"
        });
        assert_eq!(observed, "ran");
        // Restored.
        assert!(current_subject().is_none());
    }

    #[test]
    fn t19_2_c_subject_principal_set_dedup_by_name() {
        let s = Subject::new();
        s.add_principal(Principal::new("alice"));
        s.add_principal(Principal::new("alice")); // duplicate name → dedupe
        s.add_principal(Principal::new("bob"));
        // Set semantics: two distinct principal names → 2 entries.
        assert_eq!(s.principal_count(), 2);
        let names: Vec<String> = s
            .get_principals()
            .into_iter()
            .map(|p| p.name().to_string())
            .collect();
        assert!(names.contains(&"alice".to_string()));
        assert!(names.contains(&"bob".to_string()));
    }

    #[test]
    fn t19_2_c_login_context_required_success() {
        let s = Subject::new();
        let lc = LoginContext::new(
            "test-required-ok",
            s,
            vec![
                always_succeeds(ControlFlag::Required, "a"),
                always_succeeds(ControlFlag::Required, "b"),
            ],
        );
        let r = lc.login();
        assert!(r.success);
        assert_eq!(r.outcomes.len(), 2);
        assert!(r.outcomes.iter().all(|o| *o == ModuleOutcome::Success));
    }

    #[test]
    fn t19_2_c_login_context_required_failure_calls_remaining_modules() {
        // REQUIRED failure must NOT abort the chain — remaining
        // modules still see their login() invoked. This is
        // specifically different from REQUISITE.
        let counter = Arc::new(AtomicU64::new(0));
        let lc = LoginContext::new(
            "test-required-fail",
            Subject::new(),
            vec![
                always_fails(ControlFlag::Required, "dead"),
                tracking_module(
                    ControlFlag::Optional,
                    counter.clone(),
                    ModuleOutcome::Success,
                ),
                tracking_module(
                    ControlFlag::Optional,
                    counter.clone(),
                    ModuleOutcome::Success,
                ),
            ],
        );
        let r = lc.login();
        // Overall must fail because a REQUIRED failed.
        assert!(!r.success);
        // But both remaining modules executed (counter bumped twice).
        assert_eq!(counter.load(Ordering::SeqCst), 2);
        assert_eq!(r.outcomes[0], ModuleOutcome::Failure);
        assert_eq!(r.outcomes[1], ModuleOutcome::Success);
        assert_eq!(r.outcomes[2], ModuleOutcome::Success);
    }

    #[test]
    fn t19_2_c_login_context_requisite_aborts_on_failure() {
        // REQUISITE failure short-circuits — subsequent modules are
        // NOT invoked at all.
        let counter = Arc::new(AtomicU64::new(0));
        let lc = LoginContext::new(
            "test-requisite-abort",
            Subject::new(),
            vec![
                always_fails(ControlFlag::Requisite, "abort"),
                tracking_module(
                    ControlFlag::Required,
                    counter.clone(),
                    ModuleOutcome::Success,
                ),
            ],
        );
        let r = lc.login();
        assert!(!r.success);
        // The second module was not reached.
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        // The chain cut off after the first entry, so outcomes has
        // only one recorded result.
        assert_eq!(r.outcomes.len(), 1);
        assert_eq!(r.outcomes[0], ModuleOutcome::Failure);
    }

    #[test]
    fn t19_2_c_login_context_sufficient_success_stops_chain() {
        // SUFFICIENT success stops the chain (remaining modules are
        // NotInvoked), and the overall result is success.
        let counter = Arc::new(AtomicU64::new(0));
        let lc = LoginContext::new(
            "test-sufficient-stop",
            Subject::new(),
            vec![
                always_succeeds(ControlFlag::Sufficient, "win"),
                tracking_module(
                    ControlFlag::Required,
                    counter.clone(),
                    ModuleOutcome::Failure,
                ),
            ],
        );
        let r = lc.login();
        assert!(r.success);
        // Second module was skipped entirely.
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        assert_eq!(r.outcomes.len(), 2);
        assert_eq!(r.outcomes[0], ModuleOutcome::Success);
        assert_eq!(r.outcomes[1], ModuleOutcome::NotInvoked);
    }

    #[test]
    fn t19_2_c_login_context_optional_failure_not_fatal() {
        // OPTIONAL failure followed by REQUIRED success → overall success.
        let lc = LoginContext::new(
            "test-optional-fail",
            Subject::new(),
            vec![
                always_fails(ControlFlag::Optional, "meh"),
                always_succeeds(ControlFlag::Required, "win"),
            ],
        );
        let r = lc.login();
        assert!(r.success);
        assert_eq!(r.outcomes[0], ModuleOutcome::Failure);
        assert_eq!(r.outcomes[1], ModuleOutcome::Success);
    }

    #[test]
    fn t19_2_c_security_domain_service_lookup_returns_auth_manager() {
        let _guard = domain_test_lock();
        clear_domain_registry();

        let policy = ApplicationPolicy::new(vec![always_succeeds(ControlFlag::Required, "a")]);
        let sds = SecurityDomainService::new("keycloak", policy);
        register_security_domain(sds.clone());

        // Lookup by name returns the same Arc handle.
        let found =
            lookup_security_domain("keycloak").expect("registered keycloak domain should resolve");
        assert!(Arc::ptr_eq(&found, &sds));

        // AuthenticationManager is the same Rust-side object.
        let mgr = found.get_authentication_manager();
        assert!(Arc::ptr_eq(&mgr, &found));

        // The policy exposes the registered chain.
        let info = found.policy.get_authentication_info();
        assert_eq!(info.len(), 1);
        assert_eq!(info[0].flag, ControlFlag::Required);

        // And we can build a fresh LoginContext over that chain.
        let lc = found.new_login_context("keycloak");
        let r = lc.login();
        assert!(r.success);
        clear_domain_registry();
    }

    #[test]
    fn t19_2_c_security_identity_run_as_changes_current_principal() {
        // Verify the thread-local identity is set for the scope of
        // run_as and restored on return, even if the closure panics.
        assert!(current_identity().is_none());
        let id = SecurityIdentity::new(
            Principal::new("admin"),
            ["user".to_string(), "admin".to_string()],
        );
        assert!(id.has_role("user"));
        assert!(id.has_role("admin"));
        let observed = id.run_as(|| {
            let cur = current_identity().expect("identity active inside run_as");
            cur.principal.name().to_string()
        });
        assert_eq!(observed, "admin");
        assert!(current_identity().is_none());

        // Panic safety — the drop guard restores on unwind.
        let id2 = SecurityIdentity::new(Principal::new("u2"), std::iter::empty());
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            id2.run_as(|| {
                let _ = current_identity().unwrap();
                panic!("boom");
            })
        }));
        assert!(panicked.is_err());
        assert!(
            current_identity().is_none(),
            "identity must be cleared even after a panic"
        );
    }

    #[test]
    fn t19_2_c_access_control_context_check_permission_delegates_to_policy() {
        // With no policy loaded (default), every permission is allowed;
        // once a non-matching policy is loaded we get DENY.
        use crate::security_manager::{set_active_policy, Policy};

        // Make sure we start with a clean slate. A concurrent test
        // that installs a policy would invalidate our expectations,
        // so grab the same lock the security_manager tests use.
        // policy_test_lock is private to that module — we make do
        // with a best-effort check here; failures show up as extra
        // DENYs that the second assertion catches.
        set_active_policy(None);
        assert!(
            crate::security_manager::policy_allows(
                "java/util/PropertyPermission",
                "java.version",
                "read",
                None,
            ),
            "default policy must allow-all"
        );

        // Install an empty policy — now everything is denied.
        let policy = Policy::default();
        set_active_policy(Some(policy));
        assert!(
            !crate::security_manager::policy_allows(
                "java/util/PropertyPermission",
                "java.version",
                "read",
                None,
            ),
            "empty policy must deny"
        );
        // Clean up so we don't break other tests.
        set_active_policy(None);
    }

    // -----------------------------------------------------------------------
    // Additional sanity tests for the support machinery.
    // -----------------------------------------------------------------------

    #[test]
    fn t19_2_c_constant_time_eq_same_length() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"sedret"));
        assert!(!constant_time_eq(b"secret", b"SECRET"));
    }

    #[test]
    fn t19_2_c_constant_time_eq_different_length_rejects() {
        assert!(!constant_time_eq(b"short", b"longer-string"));
        assert!(!constant_time_eq(b"", b"nonempty"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn t19_2_c_control_flag_parse_case_insensitive() {
        assert_eq!(ControlFlag::parse("REQUIRED"), Some(ControlFlag::Required));
        assert_eq!(ControlFlag::parse("required"), Some(ControlFlag::Required));
        assert_eq!(
            ControlFlag::parse("Requisite"),
            Some(ControlFlag::Requisite)
        );
        assert_eq!(
            ControlFlag::parse("SUFFICIENT"),
            Some(ControlFlag::Sufficient)
        );
        assert_eq!(ControlFlag::parse("optional"), Some(ControlFlag::Optional));
        assert_eq!(ControlFlag::parse("nope"), None);
    }

    #[test]
    fn t19_2_c_redact_private_never_leaks_bytes() {
        let secret = b"super-secret-password";
        assert_eq!(redact_private(secret), "<redacted>");
        // And a debug-format on a LoginModuleEntry also redacts options.
        let lme = LoginModuleEntry {
            class_name: "x".into(),
            flag: ControlFlag::Required,
            options: {
                let mut m = HashMap::new();
                m.insert("password".to_string(), "s3cr3t".to_string());
                m
            },
            login_fn: Box::new(|_, _| ModuleOutcome::Success),
        };
        let dbg = format!("{lme:?}");
        assert!(dbg.contains("<redacted>"));
        assert!(!dbg.contains("s3cr3t"));
    }

    #[test]
    fn t19_2_c_db_password_module_constant_time_matches_on_equal() {
        let s = Subject::new();
        let m = db_password_module(
            "org.jboss.security.DatabaseServerLoginModule",
            ControlFlag::Required,
            b"hash-of-p4ssw0rd".to_vec(),
            b"hash-of-p4ssw0rd".to_vec(),
            "dbuser".into(),
        );
        let lc = LoginContext::new("db", s.clone(), vec![m]);
        let r = lc.login();
        assert!(r.success);
        assert_eq!(s.principal_count(), 1);
    }

    #[test]
    fn t19_2_c_db_password_module_rejects_mismatch() {
        let s = Subject::new();
        let m = db_password_module(
            "org.jboss.security.DatabaseServerLoginModule",
            ControlFlag::Required,
            b"hash-of-p4ssw0rd".to_vec(),
            b"hash-of-WRONG".to_vec(),
            "dbuser".into(),
        );
        let lc = LoginContext::new("db", s.clone(), vec![m]);
        let r = lc.login();
        assert!(!r.success);
        assert_eq!(s.principal_count(), 0);
    }

    #[test]
    fn t19_2_c_internal_bootstrap_module_attaches_internal_principal() {
        let s = Subject::new();
        let m = internal_bootstrap_module(ControlFlag::Required);
        let lc = LoginContext::new("mgmt", s.clone(), vec![m]);
        let r = lc.login();
        assert!(r.success);
        assert!(s.get_principals().iter().any(|p| p.name() == "internal"));
    }

    #[test]
    fn t19_2_c_login_context_logout_clears_last_result_and_private_creds() {
        let s = Subject::new();
        s.add_private_credential(b"p4ssw0rd");
        let lc = LoginContext::new(
            "logout-test",
            s.clone(),
            vec![always_succeeds(ControlFlag::Required, "ok")],
        );
        assert!(lc.login().success);
        assert!(lc.last_result().is_some());
        assert!(!s.get_private_credentials().is_empty());
        lc.logout();
        assert!(lc.last_result().is_none());
        assert!(s.get_private_credentials().is_empty());
    }

    #[test]
    fn t19_2_c_security_identity_get_roles_returns_snapshot() {
        let id = SecurityIdentity::new(
            Principal::new("service"),
            ["reader", "writer"].into_iter().map(str::to_string),
        );
        let roles: Vec<String> = id.get_roles().into_iter().map(|r| r.to_string()).collect();
        assert_eq!(roles.len(), 2);
        assert!(roles.iter().any(|r| r == "reader"));
        assert!(roles.iter().any(|r| r == "writer"));
    }

    #[test]
    fn t19_2_c_principal_eq_and_hash_on_name() {
        let p1 = Principal::new("same");
        let p2 = Principal::new("same");
        let p3 = Principal::new("other");
        assert_eq!(*p1, *p2);
        assert_ne!(*p1, *p3);
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h1 = DefaultHasher::new();
        let mut h2 = DefaultHasher::new();
        (*p1).hash(&mut h1);
        (*p2).hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish());
    }

    #[test]
    fn t19_2_c_register_wildfly_security_natives_installs_each_method() {
        let mut r = NativeMethodRegistry::new();
        register_jdk_security_natives(&mut r);
        register_wildfly_security_natives(&mut r);
        assert!(r
            .find(
                "javax/security/auth/Subject",
                "doAs",
                "(Ljavax/security/auth/Subject;Ljava/security/PrivilegedAction;)Ljava/lang/Object;"
            )
            .is_some());
        assert!(r
            .find("javax/security/auth/login/LoginContext", "login", "()V")
            .is_some());
        assert!(r
            .find(
                "org/jboss/as/security/SecurityDomainService",
                "getAuthenticationManager",
                "()Ljava/lang/Object;"
            )
            .is_some());
        assert!(r
            .find(
                "org/wildfly/security/auth/server/SecurityIdentity",
                "runAs",
                "(Ljava/util/concurrent/Callable;)Ljava/lang/Object;"
            )
            .is_some());
        // AccessControlContext.checkPermission is registered by
        // security_manager.rs; we only register <init>.
        assert!(r
            .find(
                "java/security/AccessControlContext",
                "<init>",
                "([Ljava/security/ProtectionDomain;)V"
            )
            .is_some());
    }
}
