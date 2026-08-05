# L6 — Ratchet the unadjudicated `Bridge` registrations

**Owns:** `native-builtins/tests/`, `scripts/`, `regression-suite/`
**Gated on:** nothing. No production source changes — safe beside every other lane.
**Effort:** S
**Evidence:** [`native-kind-is-ambient-and-defaults-to-syntheticstub.md`](../../known-issues/jdk-only/native-kind-is-ambient-and-defaults-to-syntheticstub.md)

## Goal

Schema 3's `image_declaring_method` made "is this registration adjudicated?" a
machine-readable fact. Measured on JDK 25:

| what the image says about the target | rows | share |
|---|---:|---:|
| `ACC_NATIVE` — a genuine bridge (§1.5) | 760 | 7% |
| concrete bytecode — a **shadow** | 4,796 | 44% |
| abstract method — intercepts every implementor | 1,321 | 12% |
| class present, method **not declared** — dead or misdescribed | 2,489 | 23% |
| class absent from the image | 1,478 | 14% |

**10,084 of 10,844 `Bridge` registrations have no `ACC_NATIVE` target.** Nothing
stops that number rising. Pin it.

## Design

Model on `native-builtins/tests/stub_ratchet.rs`: an exact baseline with
`SLACK = 0`, plus a vacuity floor so an empty census cannot pass.

The obstacle: this needs a **real JDK image at test time**, which a unit test
does not have. Two options, pick one and say which in the test's doc comment:

* **Regression-suite gate** — run the census in `regression-suite/`, where a
  JDK is available, and compare against a committed baseline JSON. Preferred.
* **Committed census artefact** — commit the schema-3 census for a pinned JDK
  and have a unit test assert over it. Cheaper, but it is a snapshot that rots,
  and a rotting baseline is worse than none. If you take this route, add a
  freshness assertion on the JDK version in the artefact.

## Steps

1. Emit the four counts above as a machine-readable block (extend
   `scripts/jdk-only-adjudicate.py`, or add them to the census JSON itself).
2. Commit the baseline with the JDK feature version beside it.
3. Assert: `bridge_without_acc_native <= BASELINE` exactly, `SLACK = 0`; and
   `total_rows >= 8000` as a vacuity floor — the same reasoning as
   `essential_registry_is_populated`, and label it a collapse detector, not a
   measurement, so nobody cites it as a fact.
4. **Verify by injection.** Add a deliberate `Bridge` registration for a method
   with concrete bytecode; the gate must fail. Revert. A guard never shown to
   fail is decoration — see the README's failure modes for three that shipped
   inert.

## Why this is worth doing early

It is the only lane that makes the other `NativeKind` work *measurable*. L5's
migrations are individually small and collectively invisible without a number
that moves. It also costs nothing to run beside every other lane, since it
touches no production source.

## Done when

The gate exists, fails on an injected unadjudicated `Bridge`, passes on the
current tree, and the baseline is committed with its JDK version.
