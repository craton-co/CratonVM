# JDK-only mode — normative design and cross-crate API contract

**Status:** Partial — `--jdk-only` exists, boots, and enforces the core
refusals; the hard-coded dispatch lists it was meant to retire are still there,
bypassed rather than removed.

## What it does today

**`--jdk-only` is a policy, not a JDK mode.** `JdkMode` is still exactly
`{Real, Synthetic}` (`vm/src/config.rs`); strictness is a second, orthogonal
enum `CompatibilityMode::{Compatible, JdkOnly}` (`types/src/compat.rs`) carried
as an `ExecutionPolicy`. `--jdk-only` sets `JdkMode::Real` **and**
`CompatibilityMode::JdkOnly`, and conflicts with `--synthetic-jdk`. Both the
launcher and the embedded entry point default to `Compatible`.

The single decision point is `resolve_dispatch` in `vm/src/vm/vm_exec.rs`.
Companion CLI switches: `--jdk-only-report`, `--dump-class-origins`,
`--trace-jdk-only`, `--explain-jdk-only`.

What strict mode enforces today, over and above plain real-JDK mode:

- refuses `NativeKind::SyntheticStub` **at registration and at invocation**
  (`native-api/src/registry.rs`, `resolve_dispatch`);
- refuses compatibility-class fabrication
  (`classloading/src/class_manager.rs`);
- refuses the canonical-interface substitution map
  (`Set`→`HashSet`, `Map`→`HashMap`, …) in `vm/src/runtime/interpreter.rs`;
- refuses unapproved JIT direct-native ladders
  (`jit/src/lib.rs::direct_native_helper`);
- **records** `NativeShadowsBytecode` observations rather than rejecting them.

Deletions that did land, with anti-regression tests: the four copies of the
forced-native `java/lang/String` lists, and the `ThreadPoolExecutor.execute`
receiver-shape sites. Where those lists went is
`drop_real_layout_synthetic` in `native-api/src/registry.rs` — still
name-based, but centralised at **registration** rather than replicated across
dispatch paths.

## What is not built yet

- **The two large hard-coded lists survive.**
  `force_native_over_real_jdk_bytecode` (~55 `(class, method, descriptor)`
  branches, `vm/src/runtime/interpreter/native_override.rs`) and
  `check_override` (~250 disjuncts, `vm/src/vm/vm_exec.rs`) are untouched. Of
  `check_override`'s disjuncts only `method.is_abstract()` survives §7 of this
  contract; the rest are class-name exceptions. They are dead under
  `--jdk-only` and load-bearing under `Compatible`, which is exactly why
  deleting them is a `Compatible`-mode change, not a jdk-only one.
- **`compat_native_wins: true` is still hard-coded** at both
  `vm/src/runtime/interpreter.rs` and
  `native_override.rs::resolve_step1_native`, so §7's "concrete bytecode wins"
  is enforced by the strict arm rather than by the dispatch rule itself.
- **The `redefine_immune_*` predicate family** (~8 families) is intact and not
  policy-gated.
- **`JIT_COMPATIBILITY_MODE`**, the process-global latched `AtomicU8` in
  `jit/src/lib.rs`, is **gone** (2026-09-12). The JIT takes the policy as a
  per-compilation `jdk_only` argument from each VM's own config, so a
  `--jdk-only` VM no longer makes later VMs in the same process over-strict.
  See `jit-compatibility-and-despec-state-per-vm-FIXED.md`.

**Rename, do not purge.** The end state is two modes reached by renaming:
`--jdk-only` becomes `--real-jdk`, and today's `--real-jdk` (`Compatible`)
becomes `--synthetic-jdk`. **No synthetic method used by either surviving mode
may be removed.** Strict mode declines to *admit* a native — at registration,
by `NativeKind` — and that is the whole mechanism. Deleting the Rust function
is a different, larger change that breaks the other mode. Sort every candidate
into *policy artefacts* (hard-coded name lists, per-path copies of one
decision, `matches!` chains — delete) and *implementations* (the natives
themselves — keep, and tag `SyntheticStub` if they must not run under strict
policy).

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

## 3. Contract: `types` crate

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

## 4. Contract: `native-api` crate

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

## 5. Contract: `classloading` crate

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

## 6. Contract: `vm/src/config.rs`

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

## 7. Contract: `vm/src/vm/vm_exec.rs` + interpreter

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

## 8. Contract: `vm/src/vm/vm_init.rs`

- Propagate `config.compatibility_mode` into the registry
  (`set_compatibility_mode`) **before** any `register_*` pass, and into the
  `ClassManager`.
- Under `JdkOnly`, require a real JDK image and fail with a `MissingBootClass` /
  `InvalidConfiguration` error that names `--jdk-only`, the searched paths and
  the accepted JDK layout. Reuse the existing `require_real_jdk` machinery.
- Emit the census on request; keep the existing manual re-registration order
  (later entries overwrite earlier ones) — do not reorder it this wave.
- Under `JdkOnly`, a `--java-home` that does not EXIST must carry the same
  framing as one that exists but is not an image. Both branches now name
  `--jdk-only`, the accepted layouts and the available fixes; before 2026-08-19
  only the second did, and the first — the branch a typo takes — failed during
  argument parsing with a generic message. See
  `known-issues/jdk-only/G82-1-the-run-that-closed-a-p0-row-20260819.md` §4 N1.
- Do not edit `native-builtins/src/lib.rs`; the stub reclassification is a
  separate wave with its own subsystem-per-PR discipline.

  **The count in this clause was `157` and was wrong — it is the ROOT of a
  stale figure that propagated.** Measured 2026-08-19: the registry holds
  **1330** `SyntheticStub` registrations in the default mode, and
  `stub_ratchet.rs`'s end-state gate (`strict_mode_refuses_nothing`) reports
  **1328** still refused at VM init. The P0 *Residual synthetic native set* row
  and that test's own doc comment (`549`) both inherited a number from here
  while the tree moved. Anyone re-freezing the ratchet or planning the wave
  should re-derive the count rather than cite this clause. See
  `known-issues/jdk-only/G83-1-the-ratchet-was-already-red-20260819.md` §3a.

---

## 9. Contract: `vm-cli/src/main.rs`

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

## 10. Enforcement posture

Enforcement is **measurement first, deletion later.** Where enforcing is not
yet safe, `--jdk-only` may be diagnostic-only: record the violation, keep
going, and count it. Class fabrication (§5) and stub registration (§4) are the
two that enforce hard. `Compatible` mode must be unchanged by any of it; that
is checked by the existing regression suite and by the stub ratchet.

## 11. Acceptance criteria

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
