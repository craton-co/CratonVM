# Proposal (w25-c): mint unit-test VM identities from the real counter

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 51
> of 54).** Not built (no `test_vm_identity`). Test hygiene, zero run-time
> cost. **Gate:** the eight sites use the helper; a source ratchet forbids new
> private identity counters in `vm/src` tests. **Size:** XS.

**Status:** PROPOSAL (for triage). Filed by gc-common w25-c, 2026-09-25.

## Why

Real `SharedVm` identities come from `NEXT_VM_IDENTITY`
(`vm/src/vm/vm_init.rs:9`), a counter starting at 1. `release_vm_native_state`
(`vm/src/vm/vm_init.rs`, run by `Drop for SharedVm`) forgets that identity's
rows in about twenty process-wide tables. A unit test in the `vm` binary that
invents its own identity must therefore keep clear of every value the counter
will ever hand out while the binary runs, and today each module picks its own
way of doing so:

| module | how it picks | base |
|---|---|---|
| `runtime/jvmti.rs` `scoped_test_vm` | private counter + base | `0x7000_0000` |
| `runtime/interpreter/jvmti_events.rs` `scoped_vm` | private counter + base | `0x7100_0000` |
| `runtime/interpreter.rs` OSR gate test | literal | `0x7200_0000` |
| `runtime/instrument.rs` `unique_vm` | private counter + base (w25-c; was `1..`, i.e. real VMs) | `0x7300_0000` |
| `runtime/instrument.rs` load-time offer tests | literals | `0x10ad_0001..5` |
| `runtime/interpreter.rs` multianewarray test | literals (local map) | `0x10F_1A00` |
| `runtime/interpreter/lambda.rs` | literals | `usize::MAX - 11/12` |
| `vm/src/jit/helpers.rs` `class_init_memo` tests | literals, **inside the real range** | `7`, `8`, `101`, `202` |

Two of these were (or are) inside the real range: `instrument.rs` until w25-c,
and the JIT memo tests (`handoff-w25c-jit-class-init-memo-tests-use-real-vm-identities.md`).
Each new module has to rediscover the rule, and nothing checks the bases do not
overlap.

## Proposal

One `#[cfg(test)] pub(crate) fn test_vm_identity() -> usize` beside
`NEXT_VM_IDENTITY` that simply does `NEXT_VM_IDENTITY.fetch_add(1, Relaxed)`.
An identity drawn from the real counter is unique against every real VM and
every other test by construction, so no base, no table of bases and no
collision is possible; it is never recycled, so a real VM's teardown can never
reach it. The eight sites above would switch to it (the literal-identity tests
that assert per-VM separation keep working, since each call is distinct).

A source ratchet could then forbid new `AtomicUsize` identity counters and
`vm_identity`-shaped literals in `vm/src` test modules.

## What it would cost

Nothing at run time (test-only). The only behavioural difference: test
identities become small numbers again, which is harmless once they come from
the same counter as real ones.

## What would retire it

The helper exists and the sites in the table use it, or triage rejects it.
