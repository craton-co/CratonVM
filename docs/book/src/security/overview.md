# Security Overview

This chapter sets expectations and the threat model. The companion pages cover
the opt-in [Sandboxing & Hardening](sandboxing.md) knobs and the
[Cryptography](cryptography.md) status.

> **CratonVM is not a certified sandbox and has not had a formal third-party
> security audit. Do not use it to run untrusted Java code in
> security-sensitive environments.** Report security issues privately per the
> repository's `SECURITY.md`, not as public issues.

## Posture: JDK-faithful by default, hardening opt-in

CratonVM aims to be a **JDK-faithful general-purpose JVM by default**. With no
extra configuration, its behavior matches a standard JVM: file I/O is
unconfined, outbound sockets are permitted, and a missing security policy means
"allow everything" (the no-`SecurityManager` compatibility path). Faithfulness
is a correctness requirement for running real applications, so hardening is
layered on top as **opt-in profiles** rather than baked into the default.

What CratonVM **does** give you, opt-in:

- Defense-in-depth knobs to confine a workload's filesystem, bound its memory
  use against malicious archives and requests, and restrict where it can
  connect. See [Sandboxing & Hardening](sandboxing.md).

What is **always on**, even with no configuration:

- One egress default: well-known cloud-metadata link-local addresses (such as
  the AWS instance metadata endpoint) are blocked on the native connect path.

## What CratonVM is *not*

- **Not a certified sandbox.** The hardening profiles are defense-in-depth, not
  a trust boundary. The crypto cores use audited Rust crates where noted, but
  the VM as a whole is unaudited.
- **Not a substitute for OS-level isolation.** For genuinely adversarial code,
  run CratonVM inside a container / VM / seccomp profile / separate user
  account. The in-process knobs reduce blast radius; they do not replace kernel
  boundaries.
- **Not a complete `SecurityManager`.** The Java `SecurityManager` path is
  partial: the default (no policy ⇒ allow-all) is JDK-faithful, and
  `CRATONVM_REQUIRE_POLICY` flips it to fail-closed, but the
  permission-enforcement surface is not a full `java.security.Policy`
  implementation. Sensitive native gates (process spawning, the Panama host-call
  gate) consult the same policy singleton, but coverage is not exhaustive.

## Known security-relevant correctness item

Under the **moving** garbage collector there is a documented residual gap: a JIT
worker's *published* GC-root snapshot can be stale relative to its current JIT
spill slots at a stop-the-world safepoint, which could leave a peer collector
blind to those roots. The complete fix is a cross-thread stop-the-world JIT root
scan (precise oop-map / shadow-stack work) and is in progress.

In practice the JIT-active execution path forces the **non-moving** young sweep
with selective promotion, so this has not been observed to corrupt the heap —
but it is an open correctness item, noted here for transparency.

## Recommended deployment posture

For anything touching real user data or network traffic:

1. **Terminate TLS in front of the VM** (a reverse proxy such as nginx/Envoy, or
   a service-mesh sidecar) — CratonVM does not provide a full JSSE stack.
2. **Source long-lived secrets from a dedicated KMS** (cloud HSM, Vault) rather
   than relying on `KeyStore` round-trips through CratonVM.
3. **Run adversarial workloads under OS-level isolation**, and enable the
   in-process hardening profile (`CRATONVM_CONFINE_IO`, plus the network knobs)
   as additional defense in depth.

See [Cryptography](cryptography.md) for the per-algorithm picture and
[Sandboxing & Hardening](sandboxing.md) for the full set of knobs.
