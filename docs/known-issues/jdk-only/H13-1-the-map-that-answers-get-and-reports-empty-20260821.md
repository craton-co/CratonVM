
---

## RETRACTION of the "INDEPENDENT REPRODUCTION" above (lane H0, 2026-08-21)

**The four discriminators in my check above — and in this record's §1–§3 — are
ORDER ARTIFACTS, not properties of the treatments.** Measured, three probes,
each run twice with its cases swapped: `regression-suite/probes/ChmOrderConfound*.java`.

* `Math.abs(1)` between the puts **does not fix anything**; the failure follows
  whichever case runs **first**.
* `Integer` keys are **not** immune; they fail when they go first.
* The `ConcurrentHashMap`-vs-`Map` door disagreement is **not** about the door;
  whichever read runs first is the correct one.

There is **one** phenomenon: `CRATONVM_ENFORCE_NATIVE_SHADOW` yields to real
bytecode **exactly once per process** (`H16-3`), so exactly one map — or one
read — differs, and order decides which. **This record's "third mechanism" is
the instrument, not the VM.**

**What still stands:** the primary symptom (`size()=1` with an empty `keySet()`
under an armed CHM) is a real observed divergence, and §4's traced consequence —
`CryptoPermissions.isEmpty()` → `JceSecurity.<clinit>` throws → the JCE is dead
for the process — is unaffected.

Full analysis, including how the probe design produced the error and the
polarity asymmetry it leaves open, in
`H0-8-the-third-mechanism-was-four-order-artifacts-20260821.md`.
