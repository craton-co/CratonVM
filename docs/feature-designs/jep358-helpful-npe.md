# JEP 358 — helpful NullPointerException messages

**Status:** Shipped for the CLI (default on). Partial for embedders and for
exceptions thrown from compiled frames — see below.

## What it does today

`vm/src/runtime/exceptions.rs` (`pub mod helpful_npe`) reconstructs the null
expression backwards from the trapping instruction and renders HotSpot's
`… because "<expr>" is null` form, falling back to the action-only message when
it cannot. The pieces: `block_start_for` / `simulate_to` simulate the operand
stack from the block leader, `CpResolver` resolves field and method refs,
`LocalVariableTable` supplies local names, and `combine_opt` assembles the
message. Reconstruction is bounded at `MAX_EXPR_DEPTH = 4`.

The HotSpot switch has an equivalent:
`VmConfig::show_code_details_in_exception_messages` (`vm/src/config.rs`)
defaults to `true` and is pushed into
`vm/src/runtime/env_cache.rs`. `CRATONVM_HELPFUL_NPE_OPCODES` overrides the
opcode set.

`getExtendedNPEMessage` is registered as a native in
`native-builtins/src/lib.rs`. It surfaces the already-synthesized
`detailMessage` rather than computing lazily as HotSpot does — a legitimate
shape, but not the same one.

## What is not built yet

- **An embedder gets the legacy messages.** The backing `SHOW_CODE_DETAILS`
  static defaults to a `-1` sentinel meaning "legacy strings", and only
  `vm-cli` ever calls the setter. A host driving the VM through
  `libcratonvm` that never wires the flag therefore gets the old ad-hoc
  messages for the non-invoke opcodes. Invoke-site messages are on
  unconditionally.
- **Compiled frames give the action-only message.** The JIT path sets
  `helpful_npe::jit_action::*` codes in `vm/src/jit/helpers.rs`, rendered by
  `jit_action_message`. There is no expression clause from a compiled frame
  unless deopt supplies a bci.
- **No standing parity check.** Nothing in CI re-verifies NPE message parity
  against HotSpot.

## Goal

When a NullPointerException is thrown at a bytecode that dereferences null,
produce a JEP 358-style message naming **what** was being done and **which
expression** was null — e.g.:

```
Cannot invoke "String.length()" because "<local1>" is null
Cannot read field "x" because "this.next" is null
Cannot store to int array because "arr" is null
Cannot read the array length because "a" is null
```

and surface it through both the exception's `getMessage()` and the JDK's
`getExtendedNPEMessage()` path.

## Design

JEP 358's message has two halves: the **action** (from the trapping opcode) and
the **null expression** (from a lightweight backward dataflow over the bytecode
that produced the null operand).

### 1. The action half (cheap — from the opcode + constant pool)

At each null-deref opcode the interpreter already knows the opcode and its
constant-pool operand. Map opcode → action string:

| Opcode | Action |
|---|---|
| `invokevirtual`/`interface`/`special` | `Cannot invoke "Owner.name(Descriptor)"` |
| `getfield` | `Cannot read field "name"` |
| `putfield` | `Cannot assign field "name"` |
| `arraylength` | `Cannot read the array length` |
| `*aload` | `Cannot load from <T> array` |
| `*astore` | `Cannot store to <T> array` |
| `monitorenter`/`exit` | `Cannot enter synchronized block` |
| `athrow` | `Cannot throw exception` |

The owner/name/descriptor come from the CP entry the opcode references
(`vm/src/runtime` already resolves these for dispatch). This half is implementable
immediately and covers the bulk of the value.

### 2. The null-expression half (the JEP 358 analysis)

Reconstruct the source-ish expression for the null operand by a **bounded
backward walk** of the bytecode in the *current frame's method*, starting at the
trapping bci, mirroring HotSpot's `BytecodeUtils::do_null_pointer_exception_
message`:

- The trapping opcode consumes the null from a known operand-stack depth. Walk
  backward to find the bytecode that *pushed* that stack slot.
- Classify the producer:
  - `aload_N` / `aload N` → `"<localN>"` (or the local-variable-table name if
    the `LocalVariableTable` attribute is present — `reader/` parses it).
  - `getfield f` → `"<recv>.f"` (recurse one level on the receiver, bounded).
  - `getstatic Owner.f` → `"Owner.f"`.
  - `aaload` → `"<arr>[<idx>]"`.
  - `invoke... m()` → `"the return value of Owner.m(...)"`.
  - `aconst_null` → `"null"` (degenerate).
- **Bound the recursion depth** (HotSpot caps it) and bail to no expression
  ("…is null") when the producer can't be classified — the action half is still
  useful.

This needs a small bytecode-cursor + abstract-stack-height tracker scoped to the
current method's `code[]` (already on the frame). It does *not* need full
dataflow — JEP 358 is a syntactic reconstruction, deliberately approximate.

### 3. Message synthesis + the `getExtendedNPEMessage` path

- Combine: `"{action} because {expr} is null"` (or just `"{action}"` when the
  expr is unknown). Store it on the NPE.
- Two delivery modes matching the JDK:
  - **Eager** (default in the JDK since 15): set `message` when the NPE is
    constructed at the throw site, so `getMessage()` returns it.
  - **`getExtendedNPEMessage()`**: replace the `lib.rs:5018` null stub with a
    native that returns the synthesized string. To match the JDK precisely the
    message is computed lazily from the bci recorded on the exception (the JDK
    stores the bci + method and computes on demand) — but eager computation at
    throw is simpler and acceptable for a first cut. Gate computation off when
    `-XX:-ShowCodeDetailsInExceptionMessages` (a `VmConfig` flag) so behavior
    matches HotSpot's opt-out.

## Risks

- **Wrong expression text** is worse than none — an inaccurate `because "..."`
  misleads. Keep the analysis conservative: emit the expr clause only when the
  producer is unambiguously classified, else action-only.
- **Backward-walk cost** on every NPE: bound the walk; NPEs are not hot, but a
  pathological method shouldn't make NPE-throwing O(method size). Cap iterations.
- **JIT-thrown NPEs** lack a precise bci today; don't fabricate an expression
  from the wrong PC. Action-only for JIT NPEs until deopt provides the bci.
- **Compliance drift**: the exact JEP 358 wording is tested by some suites;
  match the JDK strings precisely (verb, quoting, "the return value of", array
  element type spelling).

