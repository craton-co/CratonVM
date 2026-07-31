# JDK-only mode — normative design and cross-crate API contract

Status: **in implementation (wave 1)**. Owner: orchestrated multi-agent delivery,
started 2026-07-31.

Source of the plan: `deep-research-report` (JDK-only mode research), which audited
the repository and produced the blocker inventory this document implements.

> **This file is the interface contract.** It is owned by the orchestrator.
> Implementation agents **read** it and **must not edit** it. Every item below
> that says MUST is a compile-level commitment other agents are coding against
> without being able to build. Deviating from a signature breaks other people's
> code.

---

## 1. Normative semantics

`--jdk-only` means: **real class bytes are authoritative.**

1. A real JDK runtime image is **required**. No silent fallback.
2. No non-array class may be fabricated. `ClassOrigin::CompatibilityStub` must
   never be created.
3. No `NativeKind::SyntheticStub` may be **registered** or **invoked**.
4. Concrete Java bytecode wins over any registered native, *except* for a
   reviewed `NativeKind::Intrinsic`.
5. `ACC_NATIVE` methods bind to a `NativeKind::Bridge` (or reviewed intrinsic).
   Absence is a structured `MissingNative` error, never a stub.
6. Arrays, hidden classes, lambdas, proxies and reflection accessors are
   **allowed** and carry their own distinct origin — they are not compatibility
   stubs.
7. Failures are structured, actionable errors naming class, method, descriptor,
   class origin, attempted native kind, JDK feature version and the
   `--real-jdk` fallback.

`--real-jdk` (default) keeps today's behaviour exactly. `--synthetic-jdk` is
unchanged and conflicts with `--jdk-only`.

Strictness is a **runtime policy**, not a build feature. The existing build-time
exclusion of the full `synthetic-jdk` library is retained.

### Terminology (do not say "synthetic" unqualified)

| Category | JDK-only disposition |
|---|---|
| class-file `ACC_SYNTHETIC` / `Synthetic` attribute | allowed |
| `NativeKind::Intrinsic` | allowed after parity review |
| `NativeKind::Bridge` | allowed after review |
| `NativeKind::SyntheticStub` | **forbidden** |
| fabricated `Class` with no real bytes | **forbidden** |
| VM-created array class | allowed |
| hidden / lambda / proxy / reflection accessor | allowed, distinct origin |
| enterprise fallback stub (`org/jboss`, `io/quarkus`, `io/smallrye`, …) | **forbidden** |

---

## 2. Crate layering

```
types  ──────────────►  (no deps on the crates below)
  ▲        ▲
  │        │
native-api │            classloading
  ▲        ▲                 ▲
  └────────┴─────────────────┴──────  vm  ──────  vm-cli
```

Therefore the **shared policy token lives in `types`**. `NativeKind` stays in
`native-api`; `ClassOrigin` stays in `classloading`. Neither crate may reference
the other's enum. Predicates live next to their own type.

**There are no process globals for this feature.** Memory-scoped state is
per-registry / per-class-manager / per-VmConfig, set at VM init. (A process
global would break multi-VM-in-one-process runs; see the repo's history of
process-global native caches leaking across VMs.)

---

## 3. Contract: `types` crate  (owner: agent F)

New module `types/src/compat.rs`, re-exported from `types/src/lib.rs` as
`pub mod compat;`.

```rust
// types/src/compat.rs

/// Which compatibility substitutions the VM permits. Orthogonal to `JdkMode`
/// (which selects *which class library*); this selects *which substitutions*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompatibilityMode {
    /// Existing real-JDK behaviour: bridges, intrinsics AND compatibility shims.
    #[default]
    Compatible,
    /// Real JDK bytes are authoritative. No fabricated compatibility classes,
    /// no `SyntheticStub` native registered or invoked.
    JdkOnly,
}

impl CompatibilityMode {
    /// Stable machine-greppable spelling: `"compatible"` / `"jdk-only"`.
    pub fn as_str(self) -> &'static str;
    pub fn is_jdk_only(self) -> bool;
}

/// The policy object shared by init, class loading and dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionPolicy {
    pub compatibility_mode: CompatibilityMode,
    /// `true` when booting a real JDK image (`JdkMode::Real`). `JdkMode` itself
    /// lives in `vm`, which `types` cannot see, hence a bool.
    pub real_jdk: bool,
}

impl ExecutionPolicy {
    pub fn compatible(real_jdk: bool) -> Self;
    pub fn jdk_only() -> Self;               // real_jdk = true
    pub fn is_jdk_only(&self) -> bool;
}

impl Default for ExecutionPolicy { /* compatible(true) */ }
```

New in `types/src/error.rs` (same file, same owner):

```rust
/// A JDK-only policy violation. All fields are owned/plain so `types` needs no
/// dependency on `native-api` or `classloading`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JdkOnlyViolation {
    CompatibilityClassRequested {
        class: String,
        initiating_loader: Option<String>,
        requester: Option<String>,     // "owner/Class.method(Desc)"
        reason: String,
    },
    SyntheticNativeRegistered {
        class: String, method: String, descriptor: String,
        registered_by: Option<String>,
    },
    SyntheticNativeInvocation {
        class: String, method: String, descriptor: String,
        call_site: Option<String>,
    },
    MissingNative {
        class: String, method: String, descriptor: String,
        module: Option<String>,
    },
    NativeShadowsBytecode {
        class: String, method: String, descriptor: String,
        native_kind: &'static str,     // NativeKind::as_str()
    },
    MissingBootClass { class: String, searched_image: String },
    MissingImplementation { class: String, method: String, descriptor: String },
}

impl JdkOnlyViolation {
    /// Stable kind tag for counters/JSON: e.g. `"compatibility-class-requested"`.
    pub fn kind(&self) -> &'static str;
    /// One-line form for logs.
    pub fn summary(&self) -> String;
    /// Multi-line operator-facing report, ending with the `--real-jdk`
    /// fallback hint. Absolute paths must be redacted unless `verbose`.
    pub fn render(&self, jdk_feature: Option<u32>, verbose: bool) -> String;
    /// JSON object body (no trailing comma, no outer braces omitted) matching
    /// the report schema. Hand-rolled to match existing dump style.
    pub fn to_json(&self) -> String;
}

impl std::fmt::Display for JdkOnlyViolation { /* = summary() */ }
```

`VmError` (wherever it lives in `types/src/error.rs`) gains:

```rust
InvalidConfiguration(String),
JdkOnly(JdkOnlyViolation),
```
only if those variants do not already exist; reuse an existing equivalent
rather than duplicating.

---

## 4. Contract: `native-api` crate  (owner: agent D)

`native-api/src/registry.rs`:

```rust
impl NativeKind {
    /// Whether this kind may be registered/invoked under `mode`.
    /// `SyntheticStub` is the only kind rejected under `JdkOnly`.
    pub fn allowed_in(self, mode: CompatibilityMode) -> bool;
}

/// One row of the schema-version-2 native census.
#[derive(Debug, Clone)]
pub struct NativeCensusEntry {
    pub class: String,
    pub name: String,
    pub descriptor: String,
    pub kind: NativeKind,
    /// Registration site, when recorded (`"native-builtins/src/lib.rs:1234"`).
    pub registered_by: Option<String>,
    /// Kind of the entry this registration overwrote, if any.
    pub overwrote: Option<NativeKind>,
    /// Times this slot was dispatched through any path this run.
    pub invocations: u64,
}

impl NativeMethodRegistry {
    /// VM-scoped strict policy. Set once at VM init, before registration.
    pub fn set_compatibility_mode(&mut self, mode: CompatibilityMode);
    pub fn compatibility_mode(&self) -> CompatibilityMode;

    /// Registrations refused because of `JdkOnly`, in registration order.
    pub fn refused_registrations(&self) -> &[JdkOnlyViolation];

    /// Schema-v2 census; order follows registration order, callers sort.
    pub fn census(&self) -> Vec<NativeCensusEntry>;

    /// Total dispatches recorded per kind this run (index by
    /// `NativeKind as usize` is NOT stable — use these accessors).
    pub fn invocations_of_kind(&self, kind: NativeKind) -> u64;

    /// Called by every dispatch path immediately before invoking `id`.
    /// Cheap: one relaxed increment. Must be safe to call on the hot path.
    pub fn record_invocation(&self, id: NativeMethodId);
}
```

Rules for D:
- `register()` under `JdkOnly` **must not** insert a `SyntheticStub`; it records
  a `SyntheticNativeRegistered` violation and returns. The existing
  `drop_synthetic_stubs` (`CRATONVM_NO_STUBS`) path stays and keeps working;
  `JdkOnly` is a stricter superset that also records provenance.
- Provenance capture must be **cheap**; use `#[track_caller]` +
  `core::panic::Location` rather than a string built at every registration.
- Invocation counters must not require `&mut self` (dispatch holds `&`). Use
  `AtomicU64` / `Cell` consistent with the existing slot layout, and keep the
  hot path allocation-free.
- Do **not** change `find_with_kind`'s existing signature or semantics.

---

## 5. Contract: `classloading` crate  (owner: agent C)

New file `classloading/src/class_origin.rs`, `pub mod class_origin;` in
`classloading/src/lib.rs`.

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassOrigin {
    BootImage { module: Option<Arc<str>>, source: Arc<str> },
    ApplicationClassPath { source: Arc<str> },
    UserDefined { loader: ClassLoaderId, source: Option<Arc<str>> },
    VmArray,
    HiddenClass { host: Option<ClassId> },
    GeneratedLambda { host: Option<ClassId> },
    GeneratedProxy { interfaces: Arc<[ClassId]> },
    ReflectionAccessor { host: Option<ClassId> },
    VmInternal,
    CompatibilityStub { reason: Arc<str> },
}

impl ClassOrigin {
    /// Stable lowercase tag for census/JSON, e.g. `"boot-image"`,
    /// `"compatibility-stub"`, `"vm-array"`, `"generated-lambda"`.
    pub fn as_str(&self) -> &'static str;
    /// Only `CompatibilityStub` is rejected under `JdkOnly`.
    pub fn allowed_in(&self, mode: CompatibilityMode) -> bool;
    pub fn is_compatibility_stub(&self) -> bool;
}

impl Default for ClassOrigin { /* VmInternal */ }

/// One row of the `--dump-class-origins` census.
#[derive(Debug, Clone)]
pub struct ClassOriginEntry {
    pub name: String,
    pub origin: String,            // ClassOrigin::as_str()
    pub reason: Option<String>,    // CompatibilityStub reason
    pub requested_by: Option<String>,
    pub real_bytes_found: bool,
    pub loader_id: u32,
}
```

`classloading/src/class.rs`:

```rust
pub struct Class {
    // ...
    /// Provenance of this class. Authoritative.
    pub origin: ClassOrigin,
    /// DERIVED from `origin` — kept as a field for the ~160 existing readers
    /// across 17 files. MUST stay in sync: it is `true` iff
    /// `origin.is_compatibility_stub()`.
    pub is_synthetic_stub: bool,
    // ...
}

impl Class {
    /// Set both `origin` and the derived `is_synthetic_stub` together.
    /// Every write to either field MUST go through this.
    pub fn set_origin(&mut self, origin: ClassOrigin);
}
```

**C must not delete `is_synthetic_stub`.** 160 read sites across 17 files are
owned by other agents this wave; removing the field breaks all of them. Convert
it to a pure derived mirror now, delete it in a later wave.

`classloading/src/class_manager.rs`:

```rust
impl ClassManager {
    pub fn set_compatibility_mode(&mut self, mode: CompatibilityMode);
    pub fn compatibility_mode(&self) -> CompatibilityMode;

    /// Every class this manager holds, for `--dump-class-origins`.
    pub fn dump_class_origins(&self) -> Vec<ClassOriginEntry>;

    /// Class-origin violations recorded this run (JdkOnly refusals, plus
    /// would-be refusals recorded in Compatible mode for the census).
    pub fn origin_violations(&self) -> &[JdkOnlyViolation];
}
```

Rules for C:
- Under `JdkOnly`, every path that today fabricates a class (`ensure_synthetic_class`,
  the enterprise-prefix fallback in `load_class`, the `Function$Identity`
  stand-in, `ProcessHandle`/`ProcessHandle$Info` native-backed stubs) must
  instead return the specification-appropriate `ClassNotFoundException` /
  `NoClassDefFoundError` and record a `CompatibilityClassRequested` violation.
- Under `Compatible`, behaviour is **byte-for-byte unchanged**, but the origin
  is still recorded so the census is meaningful before enforcement lands.
- Array classes get `VmArray`, never `CompatibilityStub`, in **both** modes.
- The in-place "upgrade a stub to real" path must call `set_origin`.
- Do not touch any other file in `classloading/`.

---

## 6. Contract: `vm/src/config.rs`  (owner: agent A)

```rust
pub use cratonvm_types::compat::{CompatibilityMode, ExecutionPolicy};

pub struct VmConfig {
    pub jdk_mode: JdkMode,
    /// Which compatibility substitutions are permitted. Defaults to
    /// `Compatible` for BOTH launcher and embedded configs — strict mode is
    /// never inferred from a build feature or an unrelated env var.
    pub compatibility_mode: CompatibilityMode,
    // ...existing fields unchanged...
}

impl VmConfig {
    pub fn is_jdk_only(&self) -> bool;
    pub fn execution_policy(&self) -> ExecutionPolicy;
    /// `JdkOnly` + `JdkMode::Synthetic` is a configuration error.
    pub fn validate_compatibility(&self) -> Result<(), VmError>;
}
```

`JdkMode` is unchanged. Do not overload it with strictness.

---

## 7. Contract: `vm/src/vm/vm_exec.rs` + interpreter  (owner: agent E)

```rust
pub enum DispatchDecision<'a> {
    Bytecode(&'a Method),
    NativeBridge(NativeCallback),
    Intrinsic(NativeCallback),
    Reject(JdkOnlyViolation),
}

/// THE single native-vs-bytecode decision point. Interpreter, JIT, reflection,
/// JNI and method handles must all route through this.
pub fn resolve_dispatch<'a>(
    policy: ExecutionPolicy,
    class: &Class,
    method: &'a Method,
    native: Option<(NativeCallback, NativeKind)>,
) -> DispatchDecision<'a>;
```

Order (normative):
1. `method.is_native()` → bridge/intrinsic if registered; `SyntheticStub` under
   `JdkOnly` → `Reject(SyntheticNativeInvocation)`; nothing registered →
   `Reject(MissingNative)`; under `Compatible` any kind is accepted.
2. registered `Intrinsic` → `Intrinsic`.
3. `method.code().is_some()` → `Bytecode`. **Concrete bytecode beats a
   registered `Bridge` or `SyntheticStub`.**
4. otherwise → `Reject(MissingImplementation)`.

Wave-1 scope for E: **land the resolver and route the main interpreter path
through it, with `Compatible` behaviour preserved bit-for-bit.** The hard-coded
class-name exception lists (`ThreadPoolExecutor.execute` receiver-shape special
case, the forced-native `String` method list) are wave-2 removals — leave them
in place but funnel them through `resolve_dispatch` and mark each with
`// JDK-ONLY-WAVE2:` so wave 2 can find them mechanically.

---

## 8. Contract: `vm/src/vm/vm_init.rs`  (owner: agent G)

- Propagate `config.compatibility_mode` into the registry
  (`set_compatibility_mode`) **before** any `register_*` pass, and into the
  `ClassManager`.
- Under `JdkOnly`, require a real JDK image and fail with a `MissingBootClass` /
  `InvalidConfiguration` error that names `--jdk-only`, the searched paths and
  the accepted JDK layout. Reuse the existing `require_real_jdk` machinery.
- Emit the census on request; keep the existing manual re-registration order
  (later entries overwrite earlier ones) — do not reorder it this wave.
- Do not edit `native-builtins/src/lib.rs`; the 157-stub reclassification is a
  separate wave with its own subsystem-per-PR discipline.

---

## 9. Contract: `vm-cli/src/main.rs`  (owner: agent B)

```text
--jdk-only                    Real JDK, reject compatibility stubs and fabricated classes.
--real-jdk                    Real JDK with current compatibility behaviour (default).
--synthetic-jdk               Standalone synthetic library; conflicts with both.
--jdk-only-report <FILE>      Write the JSON violation/counter report.
--dump-class-origins <FILE>   Write the class-origin census.
--trace-jdk-only              Log every violation as it happens.
--explain-jdk-only            Print the long-form explanation for each violation.
```

`--jdk-only` conflicts with `--synthetic-jdk`. `--jdk-only` implies
`JdkMode::Real` + `CompatibilityMode::JdkOnly`. Bare `--real-jdk` and the
no-flag default both stay `JdkMode::Real` + `CompatibilityMode::Compatible`.

`CRATONVM_REAL=-stubs` keeps working as a native-registry filter, and prints a
one-time note recommending `--jdk-only` because the env token cannot express the
class-loading or dispatch half of the contract.

Report JSON (schema_version 1):

```json
{
  "schema_version": 1,
  "mode": "jdk-only",
  "jdk_feature": 25,
  "violations": [],
  "counts": {
    "boot_image_classes": 312, "application_classes": 18,
    "generated_classes": 4,   "compatibility_classes": 0,
    "bridge_invocations": 1082, "intrinsic_invocations": 4301,
    "synthetic_stub_invocations": 0
  }
}
```

Native census JSON is bumped to `"schema_version": 2` with `registered_by`,
`overwrote`, `invocations` and a `real_declaring_method` object per entry.
Absolute paths are redacted unless `--explain-jdk-only` is passed.

---

## 10. Wave-1 enforcement posture

Wave 1 is **measurement, not deletion.** `--jdk-only` may be diagnostic-only
where enforcement is not yet safe: record the violation, keep going, and count
it. Only class fabrication (§5) and stub registration (§4) enforce in wave 1.
Compatible mode must be unchanged; that is checked by the existing regression
suite and by the stub ratchet.

## 11. Acceptance criteria (feature-level, not wave-1)

- `--jdk-only` cannot start without a valid real JDK runtime image.
- Final native registry contains zero `SyntheticStub` entries.
- Zero `ClassOrigin::CompatibilityStub` classes for non-array JDK, application
  or dependency classes.
- Every strict-mode native dispatch is an `ACC_NATIVE` bridge, a reviewed VM
  service, or a semantics-preserving intrinsic.
- Strict runs emit actionable errors with class, method, descriptor, origin,
  attempted native kind, JDK version and fallback instructions.
- Strict corpus shows no new HotSpot divergence; CI asserts a zero-stub census
  as a **blocking** gate, not an advisory warning.

**Central principle, enforced mechanically:** real class bytes are
authoritative; generated classes must have a legitimate generation origin;
native code may cross VM boundaries or provide proven intrinsics; compatibility
substitutions are never permitted in JDK-only mode.
