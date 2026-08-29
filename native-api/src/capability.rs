// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Per-VM capability gate for native and foreign (FFM/Panama) operations.
//!
//! # Why this exists
//!
//! CratonVM's security-relevant native surface is gated today by a scatter of
//! **process-global** switches, each answering a different question, each
//! reachable (or not) from a different subset of the call sites that actually
//! perform the operation:
//!
//! | mechanism | lives in | scope | gates |
//! |---|---|---|---|
//! | `SECURITY_MANAGER` singleton | `native-builtins/src/security_manager.rs:118` | process | `Runtime.exec`, `System.loadLibrary`, `new SecurityManager()` |
//! | `NATIVE_ACCESS_POLICY` | `native-builtins/src/panama.rs:100` | process | FFM downcalls + raw-address `MemorySegment` access |
//! | `PATH_CONFINE_TO_CWD` + `SANDBOX_ROOTS` | `native-io/src/lib.rs:149,299` | process | `native-io`'s own file natives (opt-in, off by default) |
//! | outbound-host policy hook | `native-io/src/outbound_policy.rs` | process | blocking/async TCP connect |
//!
//! Three consequences follow, and all three are why this module is not simply
//! another switch:
//!
//! 1. **They are process-global.** An embedding that hosts two `Vm`s in one
//!    process gets one policy for both; `System.setSecurityManager` from
//!    inside VM A changes what VM B may do. That is the cross-VM policy
//!    interference the C2 review calls out.
//! 2. **They are not uniformly consulted.** `native-builtins`' `java.nio.file`
//!    natives open files straight through
//!    [`FileDescriptorTable`](crate::fd_table::FileDescriptorTable) without
//!    ever calling `native-io`'s `validate_path`, and no file path anywhere
//!    consults `SecurityManager.checkRead`/`checkWrite` even though those
//!    natives are registered.
//! 3. **They are keyed to deprecated Java policy APIs.** `SecurityManager` and
//!    `java.security.Policy` are terminally deprecated (JEP 411 / JEP 486); a
//!    deployment cannot express "this VM may read `/data` and nothing else"
//!    without them.
//!
//! [`CapabilitySet`] is the replacement: an explicit, **per-VM**, scoped grant
//! set that is independent of any Java-visible policy object.
//!
//! # Default is permissive — deliberately
//!
//! This pass builds the mechanism and wires the chokepoints that live inside
//! this crate. [`CapabilityMode::Permissive`] is the default and it **allows
//! everything**, counting each use so a deployment can derive its own
//! least-privilege grant set from a real run
//! ([`capability_audit`]). Flipping the default to
//! [`CapabilityMode::Enforce`] would break every existing test and embedding
//! at once; `docs/security/native-capabilities.md` records the exact ordered
//! plan to get there and what breaks first.
//!
//! # The API shape forbids a global
//!
//! There is deliberately **no** `CapabilitySet::current()`, no
//! `check(cap)` free function, and no `Default` impl. Every accessor —
//! [`capabilities_for`], [`capability_audit`], [`uninstall_capabilities`] —
//! takes a [`VmId`], and [`CapabilitySet::new`] requires one. A future caller
//! cannot accidentally consult "the" policy because there is no expression
//! that names one without first naming a VM.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;

use cratonvm_types::error::{MethodCallFailed, RuntimeError};

// ===========================================================================
// VM identity
// ===========================================================================

/// Stable identity of one VM instance, as reported by
/// [`NativeContext::vm_identity`](crate::registry::NativeContext::vm_identity).
///
/// This is the handle every capability API demands. It exists as a distinct
/// newtype rather than a bare `usize` so that a call site cannot pass a
/// `ClassId`, an fd, or a "0 means global" sentinel by accident, and so that
/// `grep VmId` finds every place a policy decision is scoped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VmId(usize);

impl VmId {
    /// The identity of the VM behind `ctx`.
    ///
    /// This is the intended constructor: it forces the caller to hold a live
    /// context, so a policy lookup cannot be made "for no VM in particular".
    pub fn of(ctx: &dyn crate::registry::NativeContext) -> Self {
        VmId(ctx.vm_identity())
    }

    /// Wrap a raw `vm_identity()` value.
    ///
    /// For the VM's own init path (which holds the identity before it holds a
    /// `NativeContext`) and for tests. Prefer [`VmId::of`] everywhere else.
    pub const fn from_raw(raw: usize) -> Self {
        VmId(raw)
    }

    /// The underlying `vm_identity()` value.
    pub const fn as_usize(self) -> usize {
        self.0
    }
}

impl fmt::Display for VmId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "vm#{:#x}", self.0)
    }
}

// ===========================================================================
// Call sites
// ===========================================================================

/// Where a capability check was made, captured with `#[track_caller]`.
///
/// Two `&'static str`/`u32` words produced by the compiler — no allocation and
/// no `format!` until something renders the report. Same technique
/// `NativeMethodRegistry::register` uses for registration provenance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CallSite {
    /// Source file of the checking call.
    pub file: &'static str,
    /// Line of the checking call.
    pub line: u32,
}

impl CallSite {
    /// The caller's source location.
    #[track_caller]
    pub fn here() -> Self {
        let loc = core::panic::Location::caller();
        CallSite {
            file: loc.file(),
            line: loc.line(),
        }
    }

    /// Placeholder for a check made from a context with no source location
    /// (e.g. one reconstructed from a serialized grant file).
    pub const fn unknown() -> Self {
        CallSite {
            file: "<unknown>",
            line: 0,
        }
    }
}

impl fmt::Display for CallSite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.file, self.line)
    }
}

// ===========================================================================
// Scopes
// ===========================================================================

/// The port half of a network [`Scope`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PortSpec {
    /// Any port. As a grant this is a wildcard; as a *request* it means the
    /// call site could not name a port, which a narrower grant refuses.
    Any,
    /// Exactly this port.
    Exact(u16),
    /// An inclusive range, `lo..=hi`.
    Range(u16, u16),
}

impl PortSpec {
    /// Whether this (grant-side) spec admits `req` (a request-side spec).
    ///
    /// A request that cannot name its port (`Any`) is admitted only by a grant
    /// that is itself unbounded — fail closed, so an unknown port can never
    /// slip through a `443`-only grant.
    pub fn admits(self, req: PortSpec) -> bool {
        match (self, req) {
            (PortSpec::Any, _) => true,
            (_, PortSpec::Any) => false,
            (PortSpec::Exact(a), PortSpec::Exact(b)) => a == b,
            (PortSpec::Exact(a), PortSpec::Range(lo, hi)) => a == lo && a == hi,
            (PortSpec::Range(lo, hi), PortSpec::Exact(b)) => lo <= b && b <= hi,
            (PortSpec::Range(lo, hi), PortSpec::Range(rlo, rhi)) => lo <= rlo && rhi <= hi,
        }
    }

    /// Parse `*`, `443`, or `1024-65535`. Returns `None` on anything else.
    pub fn parse(text: &str) -> Option<PortSpec> {
        let text = text.trim();
        if text == "*" {
            return Some(PortSpec::Any);
        }
        if let Some((lo, hi)) = text.split_once('-') {
            let lo: u16 = lo.trim().parse().ok()?;
            let hi: u16 = hi.trim().parse().ok()?;
            if lo > hi {
                return None;
            }
            return Some(PortSpec::Range(lo, hi));
        }
        text.parse().ok().map(PortSpec::Exact)
    }
}

impl fmt::Display for PortSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PortSpec::Any => f.write_str("*"),
            PortSpec::Exact(p) => write!(f, "{p}"),
            PortSpec::Range(lo, hi) => write!(f, "{lo}-{hi}"),
        }
    }
}

/// What a capability applies to.
///
/// A `Scope` is used in two roles, and the asymmetry between them is the whole
/// point: on a **grant** it is a pattern, on a **request** it is the concrete
/// thing the call site is about to touch. [`Scope::admits`] reads
/// grant-admits-request, never the reverse.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// Unrestricted. As a grant, a wildcard; as a request, "the call site could
    /// not name what it is touching", which only an `Any` grant admits.
    Any,
    /// A filesystem path. **Stored normalized** (see [`normalize_path`]):
    /// separators folded to `/`, `.` dropped, interior `..` resolved, and on
    /// Windows lower-cased. Constructing through [`Scope::path`] is the only
    /// supported way to build one, so a `..` can never be smuggled past a
    /// prefix comparison.
    Path(String),
    /// A network endpoint. `host` is lower-cased; a grant host may be `*`
    /// (any) or `*.suffix` (the suffix domain and everything under it).
    Endpoint {
        /// Host name or literal address, lower-cased.
        host: String,
        /// Port, or a pattern on a grant.
        port: PortSpec,
    },
    /// An opaque named resource: a library name, a foreign symbol, an
    /// `Unsafe` operation, a `class.method` pair. A grant may be `*` (any) or
    /// end in `*` (prefix match).
    Name(String),
}

impl Scope {
    /// A path scope, normalized. See [`normalize_path`].
    pub fn path(raw: &str) -> Scope {
        Scope::Path(normalize_path(raw))
    }

    /// An endpoint scope for a concrete host and port.
    pub fn endpoint(host: &str, port: u16) -> Scope {
        Scope::Endpoint {
            host: host.trim().to_ascii_lowercase(),
            port: PortSpec::Exact(port),
        }
    }

    /// An endpoint scope from a `host:port` string, as the fd-table openers
    /// receive it. A malformed or portless value degrades to
    /// [`PortSpec::Any`], which a narrower grant refuses — fail closed.
    pub fn endpoint_str(addr: &str) -> Scope {
        let addr = addr.trim();
        // IPv6 literal: `[::1]:8080`.
        if let Some(rest) = addr.strip_prefix('[') {
            if let Some((host, tail)) = rest.split_once(']') {
                let port = tail
                    .strip_prefix(':')
                    .and_then(|p| p.parse::<u16>().ok())
                    .map(PortSpec::Exact)
                    .unwrap_or(PortSpec::Any);
                return Scope::Endpoint {
                    host: host.to_ascii_lowercase(),
                    port,
                };
            }
        }
        match addr.rsplit_once(':') {
            Some((host, port)) => match port.parse::<u16>() {
                Ok(p) => Scope::endpoint(host, p),
                Err(_) => Scope::Endpoint {
                    host: addr.to_ascii_lowercase(),
                    port: PortSpec::Any,
                },
            },
            None => Scope::Endpoint {
                host: addr.to_ascii_lowercase(),
                port: PortSpec::Any,
            },
        }
    }

    /// A named-resource scope.
    pub fn name(raw: &str) -> Scope {
        Scope::Name(raw.trim().to_string())
    }

    /// Whether this (grant-side) scope admits `req` (a request-side scope).
    ///
    /// The relation is deliberately not symmetric and deliberately not
    /// reflexive across variants: a `Path` grant never admits an `Endpoint`
    /// request, and a request whose scope is [`Scope::Any`] (the call site
    /// could not say what it touches) is admitted only by an `Any` grant.
    pub fn admits(&self, req: &Scope) -> bool {
        match (self, req) {
            (Scope::Any, _) => true,
            (_, Scope::Any) => false,
            (Scope::Path(root), Scope::Path(child)) => path_within(child, root),
            (
                Scope::Endpoint {
                    host: ghost,
                    port: gport,
                },
                Scope::Endpoint {
                    host: rhost,
                    port: rport,
                },
            ) => host_matches(ghost, rhost) && gport.admits(*rport),
            (Scope::Name(pattern), Scope::Name(value)) => name_matches(pattern, value),
            // A `Name` grant is allowed to cover a `Path` request so that a
            // deployment can write `library-load:libssl*` and have it cover
            // both `System.loadLibrary("ssl")` and an absolute `System.load`
            // path. The reverse is NOT allowed: a `Path` grant is a
            // containment claim and must not be satisfied by a bare name.
            (Scope::Name(pattern), Scope::Path(value)) => name_matches(pattern, value),
            _ => false,
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::Any => f.write_str("*"),
            Scope::Path(p) => f.write_str(p),
            Scope::Endpoint { host, port } => write!(f, "{host}:{port}"),
            Scope::Name(n) => f.write_str(n),
        }
    }
}

/// Lexically normalize a path so a prefix comparison is a containment test.
///
/// * `\` folds to `/`;
/// * `.` components are dropped;
/// * `..` cancels the preceding component, and at depth zero is **kept** on a
///   relative path (marking a genuine escape) and **dropped** on an absolute
///   one (`/..` clamps at the root, as every OS does);
/// * on Windows the result is lower-cased, because the filesystem is
///   case-insensitive and a case-sensitive prefix test would be a bypass.
///
/// This is the traversal defence: `/data/../etc/passwd` normalizes to
/// `/etc/passwd`, which is not under a `/data` grant, so the grant cannot be
/// escaped by spelling the path differently.
pub fn normalize_path(raw: &str) -> String {
    let folded = raw.replace('\\', "/");
    // A leading `/` (or a `c:` drive prefix) anchors the path.
    let absolute = folded.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    // Windows drive prefix (`c:`) is an anchor of its own and is never popped.
    let mut anchored = absolute;
    for (i, part) in folded.split('/').enumerate() {
        if part.is_empty() || part == "." {
            continue;
        }
        let is_drive_prefix = i == 0
            && part.len() == 2
            && part.ends_with(':')
            && part.starts_with(|c: char| c.is_ascii_alphabetic());
        if is_drive_prefix {
            out.push(part);
            anchored = true;
            continue;
        }
        if part == ".." {
            // The drive prefix, when present, occupies slot 0 and is an anchor.
            let floor = usize::from(anchored && !absolute);
            if out.len() > floor && out[out.len() - 1] != ".." {
                out.pop();
            } else if !anchored {
                out.push("..");
            }
            // Anchored and already at the floor: `..` clamps, drop it.
            continue;
        }
        out.push(part);
    }
    let joined = out.join("/");
    let normalized = if absolute {
        format!("/{joined}")
    } else {
        joined
    };
    if cfg!(windows) {
        normalized.to_ascii_lowercase()
    } else {
        normalized
    }
}

/// Whether normalized `child` is `root` or lies under it.
fn path_within(child: &str, root: &str) -> bool {
    if root.is_empty() {
        // An empty grant root is the relative CWD anchor: it contains every
        // relative path that does not escape, and no absolute path.
        return !child.starts_with('/') && !child.starts_with("..") && !child.contains(':');
    }
    if root == "/" {
        return child.starts_with('/');
    }
    if child == root {
        return true;
    }
    let root_slash = if root.ends_with('/') {
        root.to_string()
    } else {
        format!("{root}/")
    };
    child.starts_with(&root_slash)
}

/// Whether a grant host pattern matches a concrete request host.
fn host_matches(pattern: &str, host: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix("*.") {
        return host == suffix || host.ends_with(&format!(".{suffix}"));
    }
    pattern.eq_ignore_ascii_case(host)
}

/// Whether a grant name pattern matches a concrete request name.
fn name_matches(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return value.starts_with(prefix);
    }
    pattern == value
}

// ===========================================================================
// Capabilities
// ===========================================================================

/// The kind half of a [`Capability`], with the scope erased.
///
/// Carried by [`CapabilityDenied`] and used to classify a registered native
/// at dispatch (see [`classify_native`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum CapabilityKind {
    /// Reading a file or directory.
    FileRead,
    /// Creating, writing, truncating, renaming, or deleting a file.
    FileWrite,
    /// Any outbound or inbound socket operation (connect, bind, accept, send).
    Network,
    /// Spawning a host process.
    ProcessSpawn,
    /// `dlopen`/`LoadLibrary` of a native library.
    LibraryLoad,
    /// Raw memory access outside the managed heap: `Unsafe`, direct
    /// `ByteBuffer`, FFM `MemorySegment` at a caller-supplied address.
    RawMemory,
    /// Calling out to native code through an FFM downcall handle.
    ForeignDowncall,
    /// Exposing a Java method to native code as an FFM upcall stub.
    ForeignUpcall,
    /// Installing a native method implementation into the registry.
    NativeRegister,
}

impl CapabilityKind {
    /// Every kind, in declaration order.
    pub const ALL: [CapabilityKind; 9] = [
        CapabilityKind::FileRead,
        CapabilityKind::FileWrite,
        CapabilityKind::Network,
        CapabilityKind::ProcessSpawn,
        CapabilityKind::LibraryLoad,
        CapabilityKind::RawMemory,
        CapabilityKind::ForeignDowncall,
        CapabilityKind::ForeignUpcall,
        CapabilityKind::NativeRegister,
    ];

    /// Stable kebab-case name, as used in `CRATONVM_CAPABILITY_GRANTS` and in
    /// the audit report.
    pub const fn as_str(self) -> &'static str {
        match self {
            CapabilityKind::FileRead => "file-read",
            CapabilityKind::FileWrite => "file-write",
            CapabilityKind::Network => "network",
            CapabilityKind::ProcessSpawn => "process-spawn",
            CapabilityKind::LibraryLoad => "library-load",
            CapabilityKind::RawMemory => "raw-memory",
            CapabilityKind::ForeignDowncall => "foreign-downcall",
            CapabilityKind::ForeignUpcall => "foreign-upcall",
            CapabilityKind::NativeRegister => "native-register",
        }
    }

    /// Inverse of [`as_str`](Self::as_str).
    pub fn parse(text: &str) -> Option<CapabilityKind> {
        let text = text.trim();
        CapabilityKind::ALL.into_iter().find(|k| k.as_str() == text)
    }

    /// Whether this kind's scopes are paths (so a bare scope string in a grant
    /// list should be normalized as one).
    const fn scope_is_path(self) -> bool {
        matches!(
            self,
            CapabilityKind::FileRead | CapabilityKind::FileWrite | CapabilityKind::ProcessSpawn
        )
    }
}

/// One capability, either requested by a call site or granted by a deployment.
///
/// # Why every variant carries a [`Scope`]
///
/// The C2 exit criterion names `ProcessSpawn`, `LibraryLoad`, `RawMemory`,
/// `ForeignDowncall`, `ForeignUpcall` and `NativeRegister` without a scope.
/// They carry one here anyway, for two reasons: [`CapabilityDenied`] is
/// required to report *the scope* of the refusal (a bare `ProcessSpawn` denial
/// that cannot say which program was refused is not actionable), and the audit
/// report's whole purpose is to hand a deployment the narrowest grant that
/// still works. A call site with nothing narrower to say passes
/// [`Scope::Any`], which behaves exactly like an unscoped variant.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Capability {
    /// Read a file. Scope: the path.
    FileRead(Scope),
    /// Write, create, truncate, rename, or delete a file. Scope: the path.
    FileWrite(Scope),
    /// Use a socket. Scope: the remote (or local, for a bind) endpoint.
    Network(Scope),
    /// Spawn a host process. Scope: the program path.
    ProcessSpawn(Scope),
    /// Load a native library. Scope: the library name or path.
    LibraryLoad(Scope),
    /// Access raw memory. Scope: the operation name.
    RawMemory(Scope),
    /// Invoke a foreign function. Scope: the symbol or address.
    ForeignDowncall(Scope),
    /// Expose a Java method as a native callback. Scope: the target method.
    ForeignUpcall(Scope),
    /// Install a native method. Scope: `class.method`.
    NativeRegister(Scope),
}

impl Capability {
    /// This capability's kind, with the scope erased.
    pub const fn kind(&self) -> CapabilityKind {
        match self {
            Capability::FileRead(_) => CapabilityKind::FileRead,
            Capability::FileWrite(_) => CapabilityKind::FileWrite,
            Capability::Network(_) => CapabilityKind::Network,
            Capability::ProcessSpawn(_) => CapabilityKind::ProcessSpawn,
            Capability::LibraryLoad(_) => CapabilityKind::LibraryLoad,
            Capability::RawMemory(_) => CapabilityKind::RawMemory,
            Capability::ForeignDowncall(_) => CapabilityKind::ForeignDowncall,
            Capability::ForeignUpcall(_) => CapabilityKind::ForeignUpcall,
            Capability::NativeRegister(_) => CapabilityKind::NativeRegister,
        }
    }

    /// This capability's scope.
    pub const fn scope(&self) -> &Scope {
        match self {
            Capability::FileRead(s)
            | Capability::FileWrite(s)
            | Capability::Network(s)
            | Capability::ProcessSpawn(s)
            | Capability::LibraryLoad(s)
            | Capability::RawMemory(s)
            | Capability::ForeignDowncall(s)
            | Capability::ForeignUpcall(s)
            | Capability::NativeRegister(s) => s,
        }
    }

    /// Build a capability from a kind and a scope.
    pub fn of(kind: CapabilityKind, scope: Scope) -> Capability {
        match kind {
            CapabilityKind::FileRead => Capability::FileRead(scope),
            CapabilityKind::FileWrite => Capability::FileWrite(scope),
            CapabilityKind::Network => Capability::Network(scope),
            CapabilityKind::ProcessSpawn => Capability::ProcessSpawn(scope),
            CapabilityKind::LibraryLoad => Capability::LibraryLoad(scope),
            CapabilityKind::RawMemory => Capability::RawMemory(scope),
            CapabilityKind::ForeignDowncall => Capability::ForeignDowncall(scope),
            CapabilityKind::ForeignUpcall => Capability::ForeignUpcall(scope),
            CapabilityKind::NativeRegister => Capability::NativeRegister(scope),
        }
    }

    /// Read of a concrete path.
    pub fn file_read(path: &str) -> Capability {
        Capability::FileRead(Scope::path(path))
    }

    /// Write/create/delete of a concrete path.
    pub fn file_write(path: &str) -> Capability {
        Capability::FileWrite(Scope::path(path))
    }

    /// Socket use against a concrete `host:port`.
    pub fn network(addr: &str) -> Capability {
        Capability::Network(Scope::endpoint_str(addr))
    }

    /// Spawn of a concrete program.
    pub fn process_spawn(program: &str) -> Capability {
        Capability::ProcessSpawn(Scope::path(program))
    }

    /// Load of a concrete library name or path.
    pub fn library_load(name: &str) -> Capability {
        Capability::LibraryLoad(Scope::name(name))
    }

    /// A named raw-memory operation (`Unsafe.putLong`, `MemorySegment.get`, …).
    pub fn raw_memory(op: &str) -> Capability {
        Capability::RawMemory(Scope::name(op))
    }

    /// A downcall to a named symbol (or `0x…` address when unnamed).
    pub fn foreign_downcall(symbol: &str) -> Capability {
        Capability::ForeignDowncall(Scope::name(symbol))
    }

    /// An upcall stub for a named Java target.
    pub fn foreign_upcall(target: &str) -> Capability {
        Capability::ForeignUpcall(Scope::name(target))
    }

    /// Registration of a native for `class.method`.
    pub fn native_register(class_name: &str, method_name: &str) -> Capability {
        Capability::NativeRegister(Scope::Name(format!("{class_name}.{method_name}")))
    }

    /// Whether this (grant-side) capability admits `req`.
    pub fn admits(&self, req: &Capability) -> bool {
        self.kind() == req.kind() && self.scope().admits(req.scope())
    }

    /// Parse one `kind:scope` grant entry, as it appears in
    /// `CRATONVM_CAPABILITY_GRANTS`.
    ///
    /// * `file-read:/data` — path prefix
    /// * `network:*.example.com:443` — host pattern + port
    /// * `network:*` — any endpoint
    /// * `library-load:libssl*` — name prefix
    /// * `raw-memory:*` — any raw-memory op
    pub fn parse_grant(text: &str) -> Option<Capability> {
        let (kind_text, scope_text) = text.trim().split_once(':')?;
        let kind = CapabilityKind::parse(kind_text)?;
        let scope_text = scope_text.trim();
        if scope_text == "*" {
            return Some(Capability::of(kind, Scope::Any));
        }
        let scope = match kind {
            CapabilityKind::Network => {
                let (host, port) = match scope_text.rsplit_once(':') {
                    Some((h, p)) => (h, PortSpec::parse(p)?),
                    None => (scope_text, PortSpec::Any),
                };
                Scope::Endpoint {
                    host: host.trim().to_ascii_lowercase(),
                    port,
                }
            }
            k if k.scope_is_path() => Scope::path(scope_text),
            _ => Scope::name(scope_text),
        };
        Some(Capability::of(kind, scope))
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind().as_str(), self.scope())
    }
}

// ===========================================================================
// Denial
// ===========================================================================

/// A capability check that failed under [`CapabilityMode::Enforce`].
///
/// Carries everything an operator needs to turn the denial into a grant: the
/// capability kind, the concrete scope that was refused, which VM refused it,
/// and the source location of the gate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityDenied {
    /// The kind that was refused.
    pub capability: CapabilityKind,
    /// The concrete scope the call site asked for.
    pub scope: Scope,
    /// The VM whose policy refused it.
    pub vm: VmId,
    /// Where the gate that refused it lives.
    pub site: CallSite,
}

impl CapabilityDenied {
    /// The grant line that would have allowed this exact request, ready to be
    /// pasted into `CRATONVM_CAPABILITY_GRANTS`.
    pub fn suggested_grant(&self) -> String {
        format!("{}:{}", self.capability.as_str(), self.scope)
    }
}

impl fmt::Display for CapabilityDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "capability denied: {} scope={} ({}, gate at {}) — grant with CRATONVM_CAPABILITY_GRANTS={}",
            self.capability.as_str(),
            self.scope,
            self.vm,
            self.site,
            self.suggested_grant()
        )
    }
}

impl std::error::Error for CapabilityDenied {}

impl From<CapabilityDenied> for RuntimeError {
    fn from(denied: CapabilityDenied) -> RuntimeError {
        RuntimeError::SecurityException {
            message: denied.to_string(),
        }
    }
}

impl From<CapabilityDenied> for MethodCallFailed {
    fn from(denied: CapabilityDenied) -> MethodCallFailed {
        RuntimeError::from(denied).into()
    }
}

// ===========================================================================
// Modes
// ===========================================================================

/// Environment variable selecting the capability mode.
pub const MODE_VAR: &str = "CRATONVM_CAPABILITY_MODE";
/// Environment variable carrying the `;`-separated grant list.
pub const GRANTS_VAR: &str = "CRATONVM_CAPABILITY_GRANTS";
/// Environment variable that turns on per-first-use stderr logging.
pub const LOG_VAR: &str = "CRATONVM_CAPABILITY_LOG";

/// How a [`CapabilitySet`] responds to a check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum CapabilityMode {
    /// **Default.** Allow everything; count each distinct
    /// `(kind, scope)` so the audit report can derive a grant set. Logs the
    /// first use of each distinct capability when [`LOG_VAR`] is set.
    #[default]
    Permissive,
    /// Allow everything, and additionally evaluate each request against the
    /// grants so the report can say *which* uses an `Enforce` flip would
    /// refuse. This is the mode to run a suite in before flipping.
    Audit,
    /// Deny anything not admitted by a grant.
    Enforce,
}

impl CapabilityMode {
    /// Stable lowercase name.
    pub const fn as_str(self) -> &'static str {
        match self {
            CapabilityMode::Permissive => "permissive",
            CapabilityMode::Audit => "audit",
            CapabilityMode::Enforce => "enforce",
        }
    }

    /// Parse a mode name. Accepts a few operator-friendly aliases.
    pub fn parse(text: &str) -> Option<CapabilityMode> {
        match text.trim().to_ascii_lowercase().as_str() {
            "permissive" | "off" | "0" | "false" | "" => Some(CapabilityMode::Permissive),
            "audit" | "report" | "warn" => Some(CapabilityMode::Audit),
            "enforce" | "strict" | "deny" | "1" | "true" => Some(CapabilityMode::Enforce),
            _ => None,
        }
    }

    /// The mode requested by [`MODE_VAR`], or [`CapabilityMode::Permissive`].
    ///
    /// Read through `cratonvm_types::flags::runtime_var` so it stays inside
    /// the declared-flag boundary, matching `native_ring::track_enabled` and
    /// `NativeMethodRegistry::new`'s `CRATONVM_NO_STUBS` read. Deliberately
    /// **not** memoized in a `OnceLock`: this runs once per VM construction,
    /// and latching it process-wide would reintroduce exactly the cross-VM
    /// coupling this module exists to remove.
    pub fn from_env() -> CapabilityMode {
        match cratonvm_types::flags::runtime_var(MODE_VAR) {
            Ok(value) => match CapabilityMode::parse(&value) {
                Some(mode) => mode,
                None => {
                    eprintln!(
                        "[CAPABILITY] unrecognised {MODE_VAR}={value:?}; \
                         using permissive (allow-all). Valid: permissive|audit|enforce"
                    );
                    CapabilityMode::Permissive
                }
            },
            Err(_) => CapabilityMode::Permissive,
        }
    }
}

impl fmt::Display for CapabilityMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ===========================================================================
// Audit records
// ===========================================================================

/// Upper bound on distinct `(kind, scope)` pairs a set will remember.
///
/// The audit map is keyed by concrete scope, and a workload that opens a
/// million distinct temp files would otherwise grow it without bound. Past the
/// cap the counters for already-known entries keep moving and new entries are
/// dropped, with [`CapabilityAuditReport::truncated`] set so the report never
/// silently claims to be complete.
pub const MAX_AUDIT_ENTRIES: usize = 4096;

/// One row of the audit report: a capability that was actually exercised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityUse {
    /// Kind plus the concrete scope the call site asked for.
    pub capability: Capability,
    /// How many times it was checked.
    pub count: u64,
    /// Where it was first checked.
    pub first_site: CallSite,
    /// How many of those checks the current grant set does **not** admit.
    ///
    /// Under [`CapabilityMode::Enforce`] these were actually refused. Under
    /// [`CapabilityMode::Audit`] they were allowed, and this is precisely the
    /// count of what an `Enforce` flip would break.
    pub ungranted: u64,
}

/// Everything one VM exercised, and what a default-deny flip would cost.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityAuditReport {
    /// The VM this report describes.
    pub vm: VmId,
    /// The mode that was in force.
    pub mode: CapabilityMode,
    /// Distinct capabilities exercised, sorted (kind, then scope).
    pub uses: Vec<CapabilityUse>,
    /// Whether [`MAX_AUDIT_ENTRIES`] was hit and rows were dropped.
    pub truncated: bool,
}

impl CapabilityAuditReport {
    /// Total checks recorded.
    pub fn total_checks(&self) -> u64 {
        self.uses.iter().map(|u| u.count).sum()
    }

    /// Total checks the current grants do not admit — the size of the gap
    /// between where this VM is and default-deny.
    pub fn total_ungranted(&self) -> u64 {
        self.uses.iter().map(|u| u.ungranted).sum()
    }

    /// A `CRATONVM_CAPABILITY_GRANTS` value that admits everything this run
    /// exercised — the least-privilege grant set derived from a real run.
    ///
    /// Scopes are emitted verbatim (one grant per distinct scope), so the
    /// output is exact rather than generalized; widening `.../a`, `.../b` into
    /// a shared prefix is a judgement call left to the operator.
    pub fn suggested_grants(&self) -> String {
        let mut lines: Vec<String> = self
            .uses
            .iter()
            .map(|u| format!("{}:{}", u.capability.kind().as_str(), u.capability.scope()))
            .collect();
        lines.sort();
        lines.dedup();
        lines.join(";")
    }
}

impl fmt::Display for CapabilityAuditReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "capability audit for {} (mode={}, {} distinct, {} checks, {} ungranted{})",
            self.vm,
            self.mode,
            self.uses.len(),
            self.total_checks(),
            self.total_ungranted(),
            if self.truncated { ", TRUNCATED" } else { "" }
        )?;
        for use_row in &self.uses {
            writeln!(
                f,
                "  {:<16} {:<48} n={:<8} ungranted={:<8} first={}",
                use_row.capability.kind().as_str(),
                use_row.capability.scope().to_string(),
                use_row.count,
                use_row.ungranted,
                use_row.first_site
            )?;
        }
        Ok(())
    }
}

/// Internal per-entry accumulator.
#[derive(Clone, Debug)]
struct UseRecord {
    count: u64,
    ungranted: u64,
    first_site: CallSite,
}

// ===========================================================================
// The set
// ===========================================================================

/// A VM's capability policy.
///
/// Owned by one VM instance. There is no process-global instance and no
/// accessor that produces one without a [`VmId`]; see the module docs.
///
/// Interior mutability is confined to the audit log (a `Mutex<BTreeMap>`) so
/// [`check`](Self::check) takes `&self` and a `CapabilitySet` can sit behind an
/// `Arc` shared by every thread of its VM. The **grant set itself is
/// immutable after construction** — `grant`/`grant_all` take `&mut self`, so a
/// set that has been published behind an `Arc` cannot have its policy widened
/// from inside a running native.
pub struct CapabilitySet {
    vm: VmId,
    mode: CapabilityMode,
    grants: Vec<Capability>,
    log_first_use: bool,
    audit: Mutex<BTreeMap<Capability, UseRecord>>,
    /// Bumped whenever an entry could not be recorded because the map was at
    /// [`MAX_AUDIT_ENTRIES`]. Atomic so the fast path never has to widen the
    /// `Mutex` critical section.
    dropped_entries: AtomicU64,
}

impl CapabilitySet {
    /// A new, empty set for `vm` in `mode`.
    ///
    /// Empty means *no grants*: harmless in `Permissive`/`Audit`, and a
    /// deny-everything policy in `Enforce`. Note there is no `Default` impl —
    /// a set cannot be created without naming its VM.
    pub fn new(vm: VmId, mode: CapabilityMode) -> CapabilitySet {
        CapabilitySet {
            vm,
            mode,
            grants: Vec::new(),
            log_first_use: cratonvm_types::flags::runtime_var_os(LOG_VAR).is_some(),
            audit: Mutex::new(BTreeMap::new()),
            dropped_entries: AtomicU64::new(0),
        }
    }

    /// A set for `vm` configured from [`MODE_VAR`] and [`GRANTS_VAR`].
    ///
    /// This is what a VM init path calls. With neither variable set the result
    /// is `Permissive` with no grants — allow-everything, which is today's
    /// behaviour exactly.
    pub fn from_env(vm: VmId) -> CapabilitySet {
        let mut set = CapabilitySet::new(vm, CapabilityMode::from_env());
        if let Ok(list) = cratonvm_types::flags::runtime_var(GRANTS_VAR) {
            for (entry, parsed) in parse_grant_list(&list) {
                match parsed {
                    Some(cap) => set.grants.push(cap),
                    None => {
                        eprintln!("[CAPABILITY] ignoring unparseable {GRANTS_VAR} entry {entry:?}")
                    }
                }
            }
        }
        set
    }

    /// The VM this set belongs to.
    pub const fn vm(&self) -> VmId {
        self.vm
    }

    /// The mode in force.
    pub const fn mode(&self) -> CapabilityMode {
        self.mode
    }

    /// Whether this set will refuse anything. `false` for
    /// `Permissive`/`Audit`, so a caller can skip building an expensive scope.
    #[inline]
    pub const fn enforcing(&self) -> bool {
        matches!(self.mode, CapabilityMode::Enforce)
    }

    /// The grants in force.
    pub fn grants(&self) -> &[Capability] {
        &self.grants
    }

    /// Add one grant. Builder-style; `&mut self` by design (see the type docs).
    pub fn grant(&mut self, cap: Capability) -> &mut Self {
        self.grants.push(cap);
        self
    }

    /// Add every grant in an iterator.
    pub fn grant_all<I: IntoIterator<Item = Capability>>(&mut self, caps: I) -> &mut Self {
        self.grants.extend(caps);
        self
    }

    /// Parse and add a `;`-separated grant list. Returns the entries that did
    /// not parse, so a caller can surface them rather than silently dropping.
    pub fn grant_from_list(&mut self, list: &str) -> Vec<String> {
        let mut bad = Vec::new();
        for (entry, parsed) in parse_grant_list(list) {
            match parsed {
                Some(cap) => self.grants.push(cap),
                None => bad.push(entry.to_string()),
            }
        }
        bad
    }

    /// Whether the grants admit `req`, ignoring mode.
    pub fn is_granted(&self, req: &Capability) -> bool {
        self.grants.iter().any(|g| g.admits(req))
    }

    /// Check a capability.
    ///
    /// * `Permissive` — always `Ok`; the use is counted.
    /// * `Audit` — always `Ok`; the use is counted and, when the grants do not
    ///   admit it, tallied into [`CapabilityUse::ungranted`] so the report can
    ///   price a default-deny flip.
    /// * `Enforce` — `Ok` iff some grant admits `req`; otherwise a
    ///   [`CapabilityDenied`] naming the kind, the scope, the VM and this call
    ///   site.
    ///
    /// The call site is captured with `#[track_caller]`, so it names the gate
    /// (the native that is about to perform the operation), not this function.
    #[track_caller]
    pub fn check(&self, req: Capability) -> Result<(), CapabilityDenied> {
        let granted = self.is_granted(&req);
        let site = CallSite::here();
        self.record(&req, granted, site);
        if self.enforcing() && !granted {
            return Err(CapabilityDenied {
                capability: req.kind(),
                scope: req.scope().clone(),
                vm: self.vm,
                site,
            });
        }
        Ok(())
    }

    /// [`check`](Self::check), mapped into the error type native methods
    /// return, so a gate can simply write `ctx…?`.
    #[track_caller]
    pub fn check_or_throw(&self, req: Capability) -> Result<(), MethodCallFailed> {
        self.check(req).map_err(MethodCallFailed::from)
    }

    fn record(&self, req: &Capability, granted: bool, site: CallSite) {
        let mut audit = self.audit.lock();
        if let Some(existing) = audit.get_mut(req) {
            existing.count = existing.count.saturating_add(1);
            if !granted {
                existing.ungranted = existing.ungranted.saturating_add(1);
            }
            return;
        }
        if audit.len() >= MAX_AUDIT_ENTRIES {
            self.dropped_entries.fetch_add(1, Ordering::Relaxed);
            return;
        }
        audit.insert(
            req.clone(),
            UseRecord {
                count: 1,
                ungranted: u64::from(!granted),
                first_site: site,
            },
        );
        drop(audit);
        if self.log_first_use {
            eprintln!(
                "[CAPABILITY] {} {} {} at {}",
                self.vm,
                if granted { "granted" } else { "ungranted" },
                req,
                site
            );
        }
    }

    /// Everything this set has been asked for, sorted and deterministic.
    pub fn audit_report(&self) -> CapabilityAuditReport {
        let audit = self.audit.lock();
        let uses = audit
            .iter()
            .map(|(capability, record)| CapabilityUse {
                capability: capability.clone(),
                count: record.count,
                first_site: record.first_site,
                ungranted: record.ungranted,
            })
            .collect();
        CapabilityAuditReport {
            vm: self.vm,
            mode: self.mode,
            uses,
            truncated: self.dropped_entries.load(Ordering::Relaxed) > 0,
        }
    }

    /// Drop every recorded use, keeping mode and grants. For a harness that
    /// wants a per-test report.
    pub fn reset_audit(&self) {
        self.audit.lock().clear();
        self.dropped_entries.store(0, Ordering::Relaxed);
    }
}

impl fmt::Debug for CapabilitySet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapabilitySet")
            .field("vm", &self.vm)
            .field("mode", &self.mode)
            .field("grants", &self.grants)
            .finish_non_exhaustive()
    }
}

/// Split a `;`-separated grant list, pairing each raw entry with its parse.
fn parse_grant_list(list: &str) -> Vec<(&str, Option<Capability>)> {
    list.split(';')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(|entry| (entry, Capability::parse_grant(entry)))
        .collect()
}

// ===========================================================================
// Per-VM installation (transitional bridge)
// ===========================================================================

/// `VmId -> CapabilitySet`, so a native that holds only a `&dyn NativeContext`
/// can reach its own VM's policy before every `NativeContext` implementor
/// carries one.
///
/// This is a process-global *index*, not a process-global *policy*: nothing
/// can be read out of it without a [`VmId`], and two VMs get two entries. The
/// distinction matters — the failure this module exists to fix is one policy
/// shared by two VMs, not one lookup table holding two policies.
///
/// A `Vec` rather than a map: a process hosts a handful of VMs at most, the
/// list is written once per VM at init, and a linear scan of three entries
/// beats hashing a `usize`.
static VM_CAPABILITIES: OnceLock<Mutex<Vec<(VmId, Arc<CapabilitySet>)>>> = OnceLock::new();

fn vm_capabilities() -> &'static Mutex<Vec<(VmId, Arc<CapabilitySet>)>> {
    VM_CAPABILITIES.get_or_init(|| Mutex::new(Vec::new()))
}

/// Install `set` as the policy for the VM it names, replacing any previous one.
///
/// Returns the displaced set, if there was one. The VM is taken from
/// [`CapabilitySet::vm`] rather than passed separately, so a set can never be
/// filed under a VM it was not built for.
pub fn install_capabilities(set: Arc<CapabilitySet>) -> Option<Arc<CapabilitySet>> {
    let vm = set.vm();
    let mut table = vm_capabilities().lock();
    if let Some(slot) = table.iter_mut().find(|(id, _)| *id == vm) {
        return Some(std::mem::replace(&mut slot.1, set));
    }
    table.push((vm, set));
    None
}

/// The policy installed for `vm`, if any.
pub fn capabilities_for(vm: VmId) -> Option<Arc<CapabilitySet>> {
    vm_capabilities()
        .lock()
        .iter()
        .find(|(id, _)| *id == vm)
        .map(|(_, set)| Arc::clone(set))
}

/// Remove and return the policy installed for `vm`. Call from VM teardown so a
/// long-lived host process does not accumulate dead entries.
pub fn uninstall_capabilities(vm: VmId) -> Option<Arc<CapabilitySet>> {
    let mut table = vm_capabilities().lock();
    let idx = table.iter().position(|(id, _)| *id == vm)?;
    Some(table.remove(idx).1)
}

/// The audit report for `vm`, or `None` when no policy is installed for it.
///
/// This is the entry point a deployment uses to derive its least-privilege
/// grant set from a real run: run the workload in
/// [`CapabilityMode::Audit`], then render
/// [`CapabilityAuditReport::suggested_grants`].
///
/// Note the [`VmId`] parameter. The C2 exit criterion sketches this as
/// `capability_audit()`; a nullary version would have to consult a global,
/// which is the defect being fixed, so the VM is explicit.
pub fn capability_audit(vm: VmId) -> Option<CapabilityAuditReport> {
    capabilities_for(vm).map(|set| set.audit_report())
}

// ===========================================================================
// Reaching the policy from a NativeContext
// ===========================================================================

/// Capability checking for anything that can name its VM.
///
/// Blanket-implemented for every [`NativeContext`](crate::registry::NativeContext),
/// including `dyn NativeContext`, exactly like
/// [`ClassDiscriminator`](crate::ClassDiscriminator) — so a native gate is one
/// line:
///
/// ```ignore
/// ctx.check_capability_or_throw(Capability::file_read(&path))?;
/// ```
///
/// With no policy installed for the VM this is a lock, a scan of a
/// three-element `Vec`, and `Ok(())`. Gates on genuinely hot paths should hold
/// the `Arc` from [`CapabilityCheck::vm_capabilities`] rather than re-resolving
/// per call.
pub trait CapabilityCheck {
    /// The policy installed for this context's VM, if any.
    fn vm_capabilities(&self) -> Option<Arc<CapabilitySet>>;

    /// Check `cap` against this context's VM policy. `Ok(())` when no policy is
    /// installed (the permissive default).
    #[track_caller]
    fn check_capability(&self, cap: Capability) -> Result<(), CapabilityDenied>;

    /// [`check_capability`](Self::check_capability), mapped into the native
    /// method error type.
    #[track_caller]
    fn check_capability_or_throw(&self, cap: Capability) -> Result<(), MethodCallFailed> {
        self.check_capability(cap).map_err(MethodCallFailed::from)
    }
}

impl<C: crate::registry::NativeContext + ?Sized> CapabilityCheck for C {
    fn vm_capabilities(&self) -> Option<Arc<CapabilitySet>> {
        capabilities_for(VmId::from_raw(self.vm_identity()))
    }

    #[track_caller]
    fn check_capability(&self, cap: Capability) -> Result<(), CapabilityDenied> {
        match self.vm_capabilities() {
            Some(set) => set.check(cap),
            None => Ok(()),
        }
    }
}

// ===========================================================================
// Native classification (dispatch-side wiring)
// ===========================================================================

/// Which capability a registered native exercises, by `(class, method)`.
///
/// This is what lets the **registry itself** be a chokepoint: the interpreter
/// dispatches natives by slot, so classifying a triple once at registration
/// time gives every one of these a gate without editing the native that
/// implements it. It is deliberately a coarse, allow-listed table of the
/// entry points that *definitionally* exercise a capability — it is a safety
/// net under the per-call-site gates, not a replacement for them (it can only
/// report [`Scope::Any`], because at registration time there are no arguments).
///
/// `None` means "not capability-relevant", which is the answer for the ~3,100
/// other natives.
pub fn classify_native(class_name: &str, method_name: &str) -> Option<CapabilityKind> {
    match class_name {
        // --- process spawn -------------------------------------------------
        "java/lang/ProcessBuilder" if method_name == "start" => Some(CapabilityKind::ProcessSpawn),
        "java/lang/ProcessImpl" | "java/lang/UNIXProcess" => Some(CapabilityKind::ProcessSpawn),
        "java/lang/Runtime" => match method_name {
            "exec" => Some(CapabilityKind::ProcessSpawn),
            "load" | "load0" | "loadLibrary" | "loadLibrary0" => Some(CapabilityKind::LibraryLoad),
            _ => None,
        },
        "java/lang/System" => match method_name {
            "load" | "loadLibrary" => Some(CapabilityKind::LibraryLoad),
            _ => None,
        },
        // --- native library loading ----------------------------------------
        "jdk/internal/loader/NativeLibraries" | "jdk/internal/loader/RawNativeLibraries" => {
            Some(CapabilityKind::LibraryLoad)
        }
        // --- raw memory ----------------------------------------------------
        "sun/misc/Unsafe" | "jdk/internal/misc/Unsafe" => Some(CapabilityKind::RawMemory),
        // --- foreign (Panama / FFM) ----------------------------------------
        "java/lang/foreign/Linker" | "jdk/internal/foreign/abi/AbstractLinker" => match method_name
        {
            "upcallHandle" => Some(CapabilityKind::ForeignUpcall),
            _ => Some(CapabilityKind::ForeignDowncall),
        },
        "java/lang/foreign/SymbolLookup" => Some(CapabilityKind::LibraryLoad),
        "java/lang/foreign/MemorySegment" => Some(CapabilityKind::RawMemory),
        // --- files ---------------------------------------------------------
        "java/io/FileOutputStream" | "java/io/RandomAccessFile" => Some(CapabilityKind::FileWrite),
        "java/io/FileInputStream" => Some(CapabilityKind::FileRead),
        "java/io/File" => match method_name {
            "delete" | "delete0" | "mkdir" | "mkdir0" | "createNewFile" | "renameTo"
            | "renameTo0" | "setLastModified" | "setReadOnly" | "setWritable" => {
                Some(CapabilityKind::FileWrite)
            }
            _ => None,
        },
        // --- network -------------------------------------------------------
        "java/net/Socket"
        | "java/net/ServerSocket"
        | "java/net/DatagramSocket"
        | "sun/nio/ch/Net"
        | "java/net/PlainSocketImpl"
        | "java/net/SocketImpl" => Some(CapabilityKind::Network),
        _ => None,
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn set(mode: CapabilityMode) -> CapabilitySet {
        CapabilitySet::new(VmId::from_raw(1), mode)
    }

    // -----------------------------------------------------------------------
    // Path normalization + prefix scope matching
    // -----------------------------------------------------------------------

    #[test]
    fn normalize_folds_separators_and_dots() {
        assert_eq!(normalize_path("/data/./sub//file"), "/data/sub/file");
        assert_eq!(normalize_path("/data\\sub\\file"), "/data/sub/file");
        assert_eq!(normalize_path("data/sub/"), "data/sub");
        assert_eq!(normalize_path("/"), "/");
    }

    #[test]
    fn normalize_resolves_interior_parent_segments() {
        // The JDK opens `a/../b` as `b`; so must we, or a legitimate path is
        // rejected. See native-io's `has_escaping_parent_segment` for the same
        // argument on the confinement side.
        assert_eq!(normalize_path("/data/sub/../file"), "/data/file");
        assert_eq!(normalize_path("data/sub/../file"), "data/file");
    }

    #[test]
    fn normalize_clamps_absolute_parent_at_root_and_keeps_relative_escape() {
        assert_eq!(normalize_path("/../etc/passwd"), "/etc/passwd");
        assert_eq!(normalize_path("/data/../../etc"), "/etc");
        // A relative path that climbs above its anchor keeps the marker, so no
        // prefix grant can contain it.
        assert_eq!(normalize_path("../etc/passwd"), "../etc/passwd");
        assert_eq!(normalize_path("data/../../etc"), "../etc");
    }

    #[test]
    fn path_grant_admits_only_what_is_under_it() {
        let grant = Scope::path("/data");
        assert!(grant.admits(&Scope::path("/data")));
        assert!(grant.admits(&Scope::path("/data/file")));
        assert!(grant.admits(&Scope::path("/data/sub/file")));
        // Sibling with a shared textual prefix must NOT match.
        assert!(!grant.admits(&Scope::path("/database/file")));
        assert!(!grant.admits(&Scope::path("/etc/passwd")));
    }

    #[test]
    fn path_traversal_cannot_escape_a_granted_prefix() {
        let grant = Scope::path("/data");
        // The whole point: `..` must be resolved BEFORE the prefix test.
        assert!(!grant.admits(&Scope::path("/data/../etc/passwd")));
        assert!(!grant.admits(&Scope::path("/data/sub/../../etc/passwd")));
        assert!(!grant.admits(&Scope::path("/data/../../etc/passwd")));
        // Backslash spelling of the same escape (Windows-style input on any
        // host) must be rejected identically.
        assert!(!grant.admits(&Scope::path("/data\\..\\etc\\passwd")));
        // A `..` that merely cancels an interior component stays inside.
        assert!(grant.admits(&Scope::path("/data/sub/../file")));
    }

    #[test]
    fn relative_grant_root_rejects_absolute_and_escaping_requests() {
        let grant = Scope::path("work");
        assert!(grant.admits(&Scope::path("work/out.txt")));
        assert!(!grant.admits(&Scope::path("/work/out.txt")));
        assert!(!grant.admits(&Scope::path("../work/out.txt")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_compare_case_insensitively() {
        // NTFS is case-insensitive; a case-sensitive prefix test would let
        // `C:\DATA\x` slip past a `c:\data` grant.
        let grant = Scope::path(r"C:\data");
        assert!(grant.admits(&Scope::path(r"c:\DATA\file")));
        assert!(!grant.admits(&Scope::path(r"C:\data\..\windows\system32")));
    }

    // -----------------------------------------------------------------------
    // Host / port scope matching
    // -----------------------------------------------------------------------

    #[test]
    fn host_glob_matches_suffix_domain_and_subdomains() {
        let grant = Capability::parse_grant("network:*.example.com:443").unwrap();
        assert!(grant.admits(&Capability::network("example.com:443")));
        assert!(grant.admits(&Capability::network("api.example.com:443")));
        assert!(grant.admits(&Capability::network("a.b.example.com:443")));
        // Not a suffix match — `notexample.com` must not be admitted.
        assert!(!grant.admits(&Capability::network("notexample.com:443")));
        // Right host, wrong port.
        assert!(!grant.admits(&Capability::network("api.example.com:8443")));
    }

    #[test]
    fn host_wildcard_and_port_range() {
        let any_host = Capability::parse_grant("network:*:443").unwrap();
        assert!(any_host.admits(&Capability::network("anything.invalid:443")));
        assert!(!any_host.admits(&Capability::network("anything.invalid:80")));

        let range = Capability::parse_grant("network:localhost:8000-8100").unwrap();
        assert!(range.admits(&Capability::network("localhost:8000")));
        assert!(range.admits(&Capability::network("localhost:8100")));
        assert!(!range.admits(&Capability::network("localhost:8101")));
    }

    #[test]
    fn host_case_is_ignored_and_ipv6_literals_parse() {
        let grant = Capability::parse_grant("network:API.Example.COM:443").unwrap();
        assert!(grant.admits(&Capability::network("api.example.com:443")));

        assert_eq!(
            Scope::endpoint_str("[::1]:8080"),
            Scope::Endpoint {
                host: "::1".to_string(),
                port: PortSpec::Exact(8080)
            }
        );
    }

    #[test]
    fn a_request_that_cannot_name_its_port_is_refused_by_a_narrow_grant() {
        // Fail closed: an address the call site could not parse becomes
        // `PortSpec::Any`, and a `:443` grant must not admit it.
        let grant = Capability::parse_grant("network:example.com:443").unwrap();
        let vague = Capability::network("example.com");
        assert!(!grant.admits(&vague));
        // Only an unbounded grant admits it.
        assert!(Capability::parse_grant("network:example.com:*")
            .unwrap()
            .admits(&vague));
    }

    #[test]
    fn name_grants_match_exactly_or_by_prefix() {
        let exact = Capability::parse_grant("library-load:libssl.so.3").unwrap();
        assert!(exact.admits(&Capability::library_load("libssl.so.3")));
        assert!(!exact.admits(&Capability::library_load("libssl.so.1")));

        let prefix = Capability::parse_grant("library-load:libssl*").unwrap();
        assert!(prefix.admits(&Capability::library_load("libssl.so.3")));
        assert!(!prefix.admits(&Capability::library_load("libcrypto.so.3")));
    }

    #[test]
    fn scopes_of_different_shapes_never_admit_each_other() {
        let path = Scope::path("/data");
        let endpoint = Scope::endpoint("example.com", 443);
        let name = Scope::name("libssl");
        assert!(!path.admits(&endpoint));
        assert!(!endpoint.admits(&path));
        assert!(!path.admits(&name));
        // A `Name` grant may cover a `Path` request (documented, for
        // `System.load("/usr/lib/libssl.so")` under `library-load:*ssl*`-style
        // grants) but never the reverse.
        assert!(Scope::name("*").admits(&Scope::path("/usr/lib/libssl.so")));
        assert!(!path.admits(&Scope::name("/data/x")));
    }

    #[test]
    fn an_any_request_is_admitted_only_by_an_any_grant() {
        // A gate that cannot say what it is touching must not slide under a
        // narrow grant.
        let narrow = Capability::FileRead(Scope::path("/data"));
        let vague = Capability::FileRead(Scope::Any);
        assert!(!narrow.admits(&vague));
        assert!(Capability::FileRead(Scope::Any).admits(&vague));
    }

    // -----------------------------------------------------------------------
    // Modes
    // -----------------------------------------------------------------------

    #[test]
    fn permissive_allows_everything_and_still_counts() {
        let caps = set(CapabilityMode::Permissive);
        assert!(caps.check(Capability::file_read("/etc/shadow")).is_ok());
        assert!(caps.check(Capability::process_spawn("/bin/sh")).is_ok());
        assert!(caps.check(Capability::file_read("/etc/shadow")).is_ok());

        let report = caps.audit_report();
        assert_eq!(report.mode, CapabilityMode::Permissive);
        assert_eq!(report.uses.len(), 2, "{report}");
        assert_eq!(report.total_checks(), 3);
    }

    #[test]
    fn audit_allows_everything_but_prices_the_enforce_flip() {
        let mut caps = set(CapabilityMode::Audit);
        caps.grant(Capability::parse_grant("file-read:/data").unwrap());

        // Granted — allowed, and NOT counted against the flip.
        assert!(caps.check(Capability::file_read("/data/a")).is_ok());
        // Ungranted — still allowed (this is Audit), but priced.
        assert!(caps.check(Capability::file_read("/etc/shadow")).is_ok());
        assert!(caps.check(Capability::file_read("/etc/shadow")).is_ok());

        let report = caps.audit_report();
        assert_eq!(report.total_checks(), 3);
        assert_eq!(
            report.total_ungranted(),
            2,
            "an Enforce flip would refuse exactly the two /etc/shadow reads:\n{report}"
        );
        let shadow = report
            .uses
            .iter()
            .find(|u| matches!(u.capability.scope(), Scope::Path(p) if p == "/etc/shadow"))
            .expect("recorded");
        assert_eq!(shadow.count, 2);
        assert_eq!(shadow.ungranted, 2);
    }

    #[test]
    fn enforce_denies_what_is_not_granted_and_allows_what_is() {
        let mut caps = set(CapabilityMode::Enforce);
        caps.grant(Capability::parse_grant("file-read:/data").unwrap())
            .grant(Capability::parse_grant("network:*.example.com:443").unwrap());

        assert!(caps.check(Capability::file_read("/data/a")).is_ok());
        assert!(caps
            .check(Capability::network("api.example.com:443"))
            .is_ok());
        assert!(caps.check(Capability::file_read("/etc/shadow")).is_err());
        assert!(caps.check(Capability::file_write("/data/a")).is_err());
        assert!(caps.check(Capability::process_spawn("/bin/sh")).is_err());
    }

    #[test]
    fn enforce_with_no_grants_denies_everything() {
        let caps = set(CapabilityMode::Enforce);
        for kind in CapabilityKind::ALL {
            let req = Capability::of(kind, Scope::name("anything"));
            assert!(
                caps.check(req).is_err(),
                "{kind:?} must be denied by an empty Enforce set"
            );
        }
    }

    #[test]
    fn mode_parsing_covers_the_operator_spellings() {
        assert_eq!(
            CapabilityMode::parse("PERMISSIVE"),
            Some(CapabilityMode::Permissive)
        );
        assert_eq!(CapabilityMode::parse("audit"), Some(CapabilityMode::Audit));
        assert_eq!(
            CapabilityMode::parse(" Enforce "),
            Some(CapabilityMode::Enforce)
        );
        assert_eq!(CapabilityMode::parse("deny"), Some(CapabilityMode::Enforce));
        assert_eq!(CapabilityMode::parse("nonsense"), None);
        // The default must be permissive — a default-deny flip is a separate,
        // documented migration (docs/security/native-capabilities.md).
        assert_eq!(CapabilityMode::default(), CapabilityMode::Permissive);
    }

    // -----------------------------------------------------------------------
    // Per-VM isolation
    // -----------------------------------------------------------------------

    #[test]
    fn two_vms_with_conflicting_grants_do_not_interfere() {
        let mut a = CapabilitySet::new(VmId::from_raw(0xA), CapabilityMode::Enforce);
        a.grant(Capability::parse_grant("file-read:/a").unwrap());
        let mut b = CapabilitySet::new(VmId::from_raw(0xB), CapabilityMode::Enforce);
        b.grant(Capability::parse_grant("file-read:/b").unwrap());

        assert!(a.check(Capability::file_read("/a/x")).is_ok());
        assert!(a.check(Capability::file_read("/b/x")).is_err());
        assert!(b.check(Capability::file_read("/b/x")).is_ok());
        assert!(b.check(Capability::file_read("/a/x")).is_err());

        // Audit logs are separate too — VM A never sees VM B's traffic.
        assert_eq!(a.audit_report().vm, VmId::from_raw(0xA));
        assert_eq!(b.audit_report().vm, VmId::from_raw(0xB));
        assert_eq!(a.audit_report().total_checks(), 2);
        assert_eq!(b.audit_report().total_checks(), 2);
    }

    #[test]
    fn modes_are_per_vm_too() {
        // The specific cross-VM interference the C2 review names: a policy
        // change in one VM must not alter another's behaviour.
        let strict = CapabilitySet::new(VmId::from_raw(1), CapabilityMode::Enforce);
        let lax = CapabilitySet::new(VmId::from_raw(2), CapabilityMode::Permissive);
        let req = || Capability::process_spawn("/bin/sh");
        assert!(strict.check(req()).is_err());
        assert!(lax.check(req()).is_ok());
    }

    #[test]
    fn installation_is_keyed_by_vm_and_is_reversible() {
        // Use VmIds that no other test installs, so this stays independent of
        // test execution order.
        let vm_x = VmId::from_raw(0x5EC_0001);
        let vm_y = VmId::from_raw(0x5EC_0002);

        let mut x = CapabilitySet::new(vm_x, CapabilityMode::Enforce);
        x.grant(Capability::parse_grant("file-read:/x").unwrap());
        let y = CapabilitySet::new(vm_y, CapabilityMode::Permissive);

        assert!(capabilities_for(vm_x).is_none());
        assert!(install_capabilities(Arc::new(x)).is_none());
        assert!(install_capabilities(Arc::new(y)).is_none());

        let looked_up = capabilities_for(vm_x).expect("installed");
        assert_eq!(looked_up.mode(), CapabilityMode::Enforce);
        assert!(looked_up.check(Capability::file_read("/x/a")).is_ok());
        assert!(looked_up.check(Capability::file_read("/y/a")).is_err());

        // The other VM's policy is untouched by the first VM's enforcement.
        let other = capabilities_for(vm_y).expect("installed");
        assert_eq!(other.mode(), CapabilityMode::Permissive);
        assert!(other.check(Capability::file_read("/x/a")).is_ok());

        // Replacement returns the displaced set; teardown removes the entry.
        let replaced = install_capabilities(Arc::new(CapabilitySet::new(
            vm_x,
            CapabilityMode::Permissive,
        )));
        assert!(replaced.is_some());
        assert!(uninstall_capabilities(vm_x).is_some());
        assert!(uninstall_capabilities(vm_y).is_some());
        assert!(capabilities_for(vm_x).is_none());
        assert!(uninstall_capabilities(vm_x).is_none());
    }

    // -----------------------------------------------------------------------
    // Denial error content
    // -----------------------------------------------------------------------

    #[test]
    fn denial_carries_kind_scope_vm_and_call_site() {
        let caps = CapabilitySet::new(VmId::from_raw(0x1234), CapabilityMode::Enforce);
        // Deliberately one line, with `line!()` immediately after: `check` is
        // `#[track_caller]`, so the recorded site must be the gate's line.
        let denied = caps
            .check(Capability::network("metadata.internal:80"))
            .expect_err("denied");
        let expected_line = line!() - 1;

        assert_eq!(denied.capability, CapabilityKind::Network);
        assert_eq!(
            denied.scope,
            Scope::Endpoint {
                host: "metadata.internal".to_string(),
                port: PortSpec::Exact(80)
            }
        );
        assert_eq!(denied.vm, VmId::from_raw(0x1234));
        assert!(
            denied.site.file.ends_with("capability.rs"),
            "site should name the gate's file, got {}",
            denied.site.file
        );
        assert_eq!(
            denied.site.line, expected_line,
            "#[track_caller] must report the gate line, not capability.rs internals"
        );

        // The message names every field and hands back a paste-able grant.
        let rendered = denied.to_string();
        assert!(rendered.contains("network"), "{rendered}");
        assert!(rendered.contains("metadata.internal:80"), "{rendered}");
        assert!(rendered.contains("vm#0x1234"), "{rendered}");
        assert_eq!(denied.suggested_grant(), "network:metadata.internal:80");
        // And that suggestion actually admits the request it came from.
        let grant = Capability::parse_grant(&denied.suggested_grant()).unwrap();
        assert!(grant.admits(&Capability::network("metadata.internal:80")));
    }

    #[test]
    fn denial_converts_into_a_security_exception() {
        let caps = CapabilitySet::new(VmId::from_raw(7), CapabilityMode::Enforce);
        let denied = caps
            .check(Capability::process_spawn("/bin/sh"))
            .expect_err("denied");
        match RuntimeError::from(denied) {
            RuntimeError::SecurityException { message } => {
                assert!(message.contains("process-spawn"), "{message}");
            }
            other => panic!("expected SecurityException, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Grant-list parsing
    // -----------------------------------------------------------------------

    #[test]
    fn grant_list_parses_and_reports_bad_entries() {
        let mut caps = set(CapabilityMode::Enforce);
        let bad = caps.grant_from_list(
            "file-read:/data; file-write:/tmp ;network:*.example.com:443;\
             process-spawn:*;bogus-kind:/x;file-read",
        );
        assert_eq!(bad, vec!["bogus-kind:/x", "file-read"]);
        assert_eq!(caps.grants().len(), 4);
        assert!(caps.check(Capability::file_read("/data/x")).is_ok());
        assert!(caps.check(Capability::file_write("/tmp/x")).is_ok());
        assert!(caps
            .check(Capability::process_spawn("/bin/anything"))
            .is_ok());
        assert!(caps.check(Capability::file_write("/data/x")).is_err());
    }

    #[test]
    fn every_kind_round_trips_through_the_grant_syntax() {
        for kind in CapabilityKind::ALL {
            let text = format!("{}:*", kind.as_str());
            let parsed =
                Capability::parse_grant(&text).unwrap_or_else(|| panic!("{text} must parse"));
            assert_eq!(parsed.kind(), kind);
            assert_eq!(parsed.scope(), &Scope::Any);
            assert_eq!(CapabilityKind::parse(kind.as_str()), Some(kind));
        }
    }

    // -----------------------------------------------------------------------
    // Audit report
    // -----------------------------------------------------------------------

    #[test]
    fn audit_report_is_sorted_deterministic_and_derives_a_working_grant_set() {
        let caps = set(CapabilityMode::Audit);
        let allow = |r: Result<(), CapabilityDenied>| r.expect("audit mode allows");
        allow(caps.check(Capability::network("db.internal:5432")));
        allow(caps.check(Capability::file_read("/data/b")));
        allow(caps.check(Capability::file_read("/data/a")));
        allow(caps.check(Capability::file_read("/data/a")));
        allow(caps.check(Capability::library_load("libssl.so.3")));

        let report = caps.audit_report();
        assert!(!report.truncated);
        assert_eq!(report.uses.len(), 4);
        assert_eq!(report.total_checks(), 5);
        // Nothing is granted, so an Enforce flip would refuse every check.
        assert_eq!(report.total_ungranted(), 5);

        // Deterministic ordering: kind first (declaration order), then scope.
        let order: Vec<String> = report
            .uses
            .iter()
            .map(|u| u.capability.to_string())
            .collect();
        assert_eq!(
            order,
            vec![
                "file-read:/data/a",
                "file-read:/data/b",
                "network:db.internal:5432",
                "library-load:libssl.so.3",
            ]
        );
        assert_eq!(caps.audit_report(), report, "report must be stable");

        // The derived grant set must actually admit everything the run did —
        // this is the whole promise of the audit mode.
        let mut derived = CapabilitySet::new(VmId::from_raw(99), CapabilityMode::Enforce);
        assert!(derived
            .grant_from_list(&report.suggested_grants())
            .is_empty());
        for use_row in &report.uses {
            assert!(
                derived.check(use_row.capability.clone()).is_ok(),
                "derived grants must admit {}",
                use_row.capability
            );
        }
        // ...and nothing else.
        assert!(derived.check(Capability::file_read("/etc/shadow")).is_err());
    }

    #[test]
    fn audit_records_the_first_call_site_not_the_last() {
        let caps = set(CapabilityMode::Permissive);
        caps.check(Capability::file_read("/data/a"))
            .expect("permissive allows");
        let first = line!() - 1;
        caps.check(Capability::file_read("/data/a"))
            .expect("permissive allows");

        let report = caps.audit_report();
        assert_eq!(report.uses.len(), 1);
        assert_eq!(report.uses[0].count, 2);
        assert_eq!(report.uses[0].first_site.line, first);
    }

    #[test]
    fn reset_audit_clears_records_but_keeps_policy() {
        let mut caps = set(CapabilityMode::Enforce);
        caps.grant(Capability::parse_grant("file-read:/data").unwrap());
        caps.check(Capability::file_read("/data/a"))
            .expect("granted");
        assert_eq!(caps.audit_report().total_checks(), 1);
        caps.reset_audit();
        assert_eq!(caps.audit_report().total_checks(), 0);
        assert!(caps.check(Capability::file_read("/data/a")).is_ok());
        assert!(caps.check(Capability::file_read("/etc/x")).is_err());
    }

    #[test]
    fn audit_map_is_bounded_and_says_so() {
        let caps = set(CapabilityMode::Permissive);
        for i in 0..(MAX_AUDIT_ENTRIES + 32) {
            caps.check(Capability::file_read(&format!("/tmp/f{i}")))
                .expect("permissive allows");
        }
        let report = caps.audit_report();
        assert_eq!(report.uses.len(), MAX_AUDIT_ENTRIES);
        assert!(
            report.truncated,
            "a capped report must never claim to be complete"
        );
    }

    // -----------------------------------------------------------------------
    // Native classification
    // -----------------------------------------------------------------------

    #[test]
    fn classification_covers_the_sensitive_entry_points() {
        assert_eq!(
            classify_native("java/lang/ProcessBuilder", "start"),
            Some(CapabilityKind::ProcessSpawn)
        );
        assert_eq!(
            classify_native("java/lang/Runtime", "exec"),
            Some(CapabilityKind::ProcessSpawn)
        );
        assert_eq!(
            classify_native("java/lang/Runtime", "loadLibrary0"),
            Some(CapabilityKind::LibraryLoad)
        );
        assert_eq!(
            classify_native("java/lang/System", "load"),
            Some(CapabilityKind::LibraryLoad)
        );
        assert_eq!(
            classify_native("jdk/internal/misc/Unsafe", "putLong"),
            Some(CapabilityKind::RawMemory)
        );
        assert_eq!(
            classify_native("java/lang/foreign/Linker", "downcallHandle"),
            Some(CapabilityKind::ForeignDowncall)
        );
        assert_eq!(
            classify_native("java/lang/foreign/Linker", "upcallHandle"),
            Some(CapabilityKind::ForeignUpcall)
        );
        assert_eq!(
            classify_native("sun/nio/ch/Net", "connect0"),
            Some(CapabilityKind::Network)
        );
        assert_eq!(
            classify_native("java/io/File", "delete0"),
            Some(CapabilityKind::FileWrite)
        );
        // Runtime methods that are not capability-relevant stay unclassified,
        // so the dispatch gate does not fire on `Runtime.gc`.
        assert_eq!(classify_native("java/lang/Runtime", "gc"), None);
        assert_eq!(classify_native("java/util/HashMap", "put"), None);
        assert_eq!(classify_native("java/io/File", "getName"), None);
    }
}
