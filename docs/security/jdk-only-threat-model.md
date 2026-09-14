# JDK-only mode — threat model

| | |
|---|---|
| **Status** | Advisory. Scoped to what `--jdk-only` changes about the VM's trust posture. |
| **Normative source** | [`../feature-designs/jdk-only-mode.md`](../feature-designs/jdk-only-mode.md) |
| **Related** | [`../../SECURITY.md`](../../SECURITY.md) (policy and disclosure) · [`../SECURITY_HARDENING.md`](../SECURITY_HARDENING.md) (the opt-in defence-in-depth surface) · [`../jdk-only-migration.md`](../jdk-only-migration.md) |

> **Read this first.** `--jdk-only` is a **correctness and provenance** control.
> It is not an isolation boundary and does not create one. It narrows *which
> class implementations may execute*; it does not constrain what those
> implementations are permitted to do. Every trusted surface CratonVM had before
> the flag it still has after.

---

## 1. What the mode actually enforces

Three invariants, all about provenance:

1. **No fabricated non-array class.** `ClassOrigin::CompatibilityStub` is never
   created. A class either came from real bytes (boot image, classpath, a user
   loader, or a legitimate generation service) or it does not exist.
2. **No `NativeKind::SyntheticStub` registered or invoked.** Native code is
   limited to reviewed bridges at genuine VM/OS boundaries and reviewed,
   parity-proven intrinsics.
3. **Real bytecode is authoritative.** A registered native cannot silently
   shadow a concrete JDK method body except as a reviewed intrinsic.

Everything below follows from those three and nothing else.

---

## 2. Risks this reduces

### 2.1 Classpath confusion

**Today (compatible mode).** When a class is absent, `ClassManager::load_class`
can fabricate an empty stand-in for classes under configured non-JDK prefixes
(`org/jboss/`, `io/quarkus/`, `io/smallrye/`, …). The intent is benign — let
enterprise bytecode link against types the VM handles natively or that are
genuinely optional. The consequence is that "class resolved" no longer implies
"class was found".

**Under `--jdk-only`.** Resolution succeeds only when real bytes were located.
A missing dependency produces `ClassNotFoundException` / `NoClassDefFoundError`
at the point of failure, naming the requester. An operator reading a strict run
can trust that every loaded type traces to a file they can name.

**Residual risk.** Strict mode says nothing about *which* real bytes won when
several were available. Ordinary classpath-ordering and shadowing questions are
unchanged; a malicious jar earlier on `-cp` is exactly as effective as before.

### 2.2 Privileged-package spoofing

**Today.** The class loader already carries privileged-package guards with
documented exemptions so genuine JDK-generated reflection and serialization
accessors can be defined (`classloading/src/loaders.rs`). Those exemptions are
**prefix-based**, and a prefix rule cannot distinguish "the JDK generated this
accessor" from "something asked for a class under that prefix".

**Under `--jdk-only`.** Authorisation becomes **origin-based**. A class under a
privileged package is admissible because its `ClassOrigin` is `BootImage` or
`ReflectionAccessor` — a fact about how it came to exist — rather than because
its name matched a string. Fabrication under `java/`, `jdk/`, `sun/` is refused
categorically.

**Residual risk.** A user class loader that supplies real bytes for a privileged
package is a separate question, governed by the existing package guards and
unchanged by this mode.

### 2.3 False-positive capability discovery

**Today.** Frameworks probe for optional features with `Class.forName` or an
`isPresent()` helper. If the probe is satisfied by a fabricated class, the
framework concludes a capability exists and then exercises it. The failure
surfaces later, in unrelated code, in a state the framework did not design for.
The class-manager source already documents this exact hazard and gates
reflective probes against the stub fallback.

**Under `--jdk-only`.** A probe for an absent class returns the honest answer.
Feature detection reflects reality, so the framework takes the path it would
take on HotSpot.

**Why this is a security property and not only a correctness one.** A framework
that believes an integration is present may enable an authentication provider,
a serialization filter, a TLS backend or an audit sink that is not actually
there. Failing the probe honestly is what keeps a security-relevant branch from
being selected on false evidence.

### 2.4 Reduced review surface

Compatibility stubs are code paths that no JDK specification describes and no
upstream test suite exercises. Removing them shrinks the amount of
security-relevant behaviour that exists only in this repository and can only be
reviewed here. This is a real benefit, and a modest one — it reduces *quantity*
of bespoke code, not the *privilege* of what remains.

---

## 3. What the mode does **not** secure

This section is the point of the document. Each item below is a fully trusted
surface **before and after** `--jdk-only`, with no change in privilege.

| Surface | Why the mode does not constrain it |
|---|---|
| **JNI** | Loading a native library and binding its symbols is part of the JVM execution model. Strict mode makes binding *more* load-bearing, since a missing bridge must now be a real bridge rather than a stub. Native code runs in-process with full process privilege. |
| **`Unsafe`** | Arbitrary memory read/write, allocation and fence operations. Unaffected. Available to any code that can obtain the instance. |
| **Class definition** | `Lookup.defineClass`, `defineHiddenClass`, proxy and accessor generation are **explicitly allowed** and carry their own origins. The mode validates *provenance categories*, not the bytes. Arbitrary valid class bytes still define arbitrary classes. |
| **Reflection** | Access checks, setAccessible, method handles. Unchanged. Strict mode requires reflection to be *more* complete, not more restricted. |
| **OS bindings** | File I/O, process spawning and control, sockets, DNS, memory mapping, clocks, entropy. Reviewed bridges are still full-privilege syscalls. |
| **Application code** | Runs with the privileges of the VM process. Strict mode does not confine it, meter it, or audit it. |
| **Bytecode verification** | Verifier completeness is an open item (`ROADMAP.md`); the pre-Java-7 split-verifier corpus is not fully covered. `--jdk-only` neither improves nor depends on this, and `--noverify` remains as unsafe as it was. |
| **Deserialization** | Java serialization hazards are a property of the real JDK code. Running *more* real JDK bytecode does not reduce them. |
| **The JDK image itself** | The mode requires a real runtime image and trusts it entirely. A tampered `java.home` is trusted the same way HotSpot trusts its own image. |
| **The JIT** | Compiled code must reach the same dispatch decisions as the interpreter. That is a correctness invariant enforced by tests, not a security boundary. |

### 3.1 The specific false impression to avoid

The phrasing that causes trouble is any variant of *"JDK-only mode restricts the
VM to safe, JDK-only code."* It is wrong in both halves:

- **"restricts"** — the mode restricts *provenance of class implementations*.
  It does not restrict *capability*. A reviewed bridge to `ProcessHandle` can
  still enumerate and kill processes. That is the whole reason the bridge
  exists.
- **"safe"** — real JDK code is not safer than our code by construction. It is
  *more specified*, *more tested upstream*, and *more predictable*. Those are
  the actual benefits and they are worth having. They are not confinement.

Do not describe this mode with isolation vocabulary. CratonVM's opt-in
defence-in-depth controls — egress policy, filesystem confinement, decompression
and body caps — are a **separate, orthogonal** surface documented in
[`../SECURITY_HARDENING.md`](../SECURITY_HARDENING.md), and that document is
itself explicit that those controls are useful *inside* an OS-level boundary and
do not constitute an in-process trust boundary. Enabling `--jdk-only` changes
none of that, in either direction. The two can be combined; neither substitutes
for the other, and neither substitutes for OS-level or container-level
isolation.

---

## 4. Risks the mode introduces

Being strict has its own failure modes.

| Risk | Impact | Mitigation |
|---|---|---|
| **Availability.** Applications that currently proceed through a compatibility substitution now fail. | An optional-dependency probe that used to return "absent" via an honest path now aborts the application if the class was load-bearing. | `--real-jdk` remains the documented fallback and the default. Strict rollout stays opt-in through preview. See [`../jdk-only-migration.md`](../jdk-only-migration.md). |
| **Diagnostic leakage.** Violation reports name classes, methods, descriptors, modules and paths. | Class names can disclose internal application structure; absolute paths can disclose deployment layout. | Absolute paths are redacted unless `--explain-jdk-only` is passed. Reports are process-local and written only where the operator asks. Review before attaching to a public issue. |
| **Telemetry scope creep.** | Counters that grow into classpath contents, arguments or environment values would export more than intended. | Telemetry stays disabled by default, process-local unless explicitly exported, aggregate-only, and free of classpath contents, source paths, arguments and environment values. Versioned so CI artifacts stay comparable. |
| **False assurance in CI.** A strict job that falls back to `--real-jdk` on failure reports green while proving nothing. | Silent loss of the entire signal. | Every strict job passes `--jdk-only`, asserts the mode it ran in, and never retries under the fallback. |
| **Bridge expansion.** Closing a strict-mode gap can mean writing a *new* native bridge. | Each new bridge is new full-privilege in-process code — a net increase in bespoke privileged surface, even as stub count falls. | Every promotion goes through [`../jdk-only-native-review.md`](../jdk-only-native-review.md), which requires the exception/synchronization/visibility/side-effect analysis and forbids assumed-slot field access. |

---

## 5. Reporting

Security issues go through [`../../SECURITY.md`](../../SECURITY.md) and GitHub
Security Advisories — **not** the public issue tracker and **not** the JDK-only
issue template.

File a JDK-only issue when strict mode *fails*. File a security advisory when
strict mode *succeeds and should not have*: a fabricated class that was admitted,
a `SyntheticStub` that was invoked, or a native that shadowed real bytecode
outside a reviewed intrinsic. Those are invariant breaches, and an invariant that
silently does not hold is worse than one that was never claimed.
