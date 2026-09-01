# `--jdk-only`: total scope, and eight parallel lanes — RETIRED 2026-09-01

**This page has retired to `jdk-only/HANDOFF-20260828-SCOPE.md` in the internal
tree.** A stub is left here because thirteen records in this directory cite it
by name, and a citation that leads nowhere is how a reader concludes the
reasoning behind live code is obsolete.

## If you are starting work

**You want [`../../contributing/jdk-only-lane-operations.md`](../../contributing/jdk-only-lane-operations.md),
not the retired page.** It is permanent, and it holds everything the scope page
was consulted for in practice: the method, probe hygiene, worktrees and shared
files, `owns_slot` and the identity/field-slot traps, the instrument traps, the
landing protocol and gate set, telling your red from theirs, and what "done"
means for a lane — including the four preconditions for retiring a shadow.

The retired page's known-red lists are dated by construction. **Re-derive rather
than trust one**; that is why they did not move to the permanent page.

## What it was, and why it could retire

It scoped the eight-lane `--jdk-only` campaign of 2026-08-28/29 — L1 `Unsafe`,
L2 strings, L3 `java.util`, L4 `java.io`/`java.nio`, L5 reflection, L6
concurrency, L7 definition-of-done, L8 the long tail — and then outlived them,
because it named three blockers of its own and held itself open until each was
discharged:

| blocker | discharged |
| --- | --- |
| **Phase 2 unadjudicated** | 2026-08-30. All 270 classes of the shadow surface armed individually: 34 load-bearing, 236 not — and **the corpus cannot decide a retirement**, because arming those 236 together fails 54 of 118 vectors and breaks 35 of 78 probe families with **zero dial leaks**. One triple of 1477 survived the evidence and is retired. [`phase-2-adjudicated-the-corpus-cannot-decide-a-retirement-20260830.md`](phase-2-adjudicated-the-corpus-cannot-decide-a-retirement-20260830.md) |
| **§4's two owned items** | 2026-08-29/30. The FFM carrier settled as a contract — the carrier is this VM's own allocation shape, so its class name is not comparable — then confirmed from the opposite side when retirement proved unable to produce the JDK's name either. `KeyStore.getInstance("JCEKS")` implemented, with the PKCS12-writes-JKS interop defect its own registration guard caught. |
| **§5 still being the operating page** | Rehomed to the contributing page above, which is linked from `INDEX.md` and `docs/jdk-only-migration.md`. |

**What is not closed is the surface itself.** 1477 native-won triples are
*adjudicated*, not retired: the Phase 2 record states what a retirement wave
needs, and §7 of the operations page carries the same four preconditions where
someone about to attempt one will actually read them.
