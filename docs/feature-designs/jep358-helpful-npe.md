# JEP 358 — Helpful NullPointerException Messages

> **Increment 1 landed (2026-06-18).** Steps 1–5 of this doc are implemented;
> step 6 (JIT-NPE parity) is intentionally skipped (deopt-gated). What shipped:
> - **Action half + null-expression half at the interpreter null-receiver
>   invoke site** (`vm/src/runtime/interpreter.rs`, `execute_invoke_kind` ~ the
>   `Object(None)` receiver arm). The old half-message
>   `Cannot invoke <method> on null` is replaced with the HotSpot shape
>   `Cannot invoke "Owner.name(params)" because "<expr>" is null` (action-only
>   when the expression can't be classified).
> - **Bounded backward bytecode analysis** in a new in-`exceptions.rs` module
>   `helpful_npe`: a linear operand-stack simulation from bci 0 to the trapping
>   bci (`last_instr_pc`, always known in the interpreter → deopt-independent)
>   that locates the receiver-slot producer and classifies it. Covered
>   producers: `aload` local/param (incl. `aload_0` → `this`), `getfield`
>   (recurses on its receiver, depth-capped at 4), `getstatic`, `aaload`
>   (`arr[...]`), `aconst_null`. Conservative: any unmodeled stack effect or a
>   nested invoke makes it bail to the action-only message (never fabricates a
>   wrong expression).
> - **`getExtendedNPEMessage` un-stubbed** (`native-builtins/src/lib.rs`): the
>   message is computed *eagerly* at the throw site and stored in
>   `Throwable.detailMessage`; the native now surfaces that String (null when
>   absent), matching `getMessage()`/the JDK accessor shape.
> - **Tests:** `vm/src/runtime/exceptions.rs` → `mod helpful_npe_tests`
>   (rt.jar-free, pure syntactic logic): getfield-of-`this`, getfield-of-local,
>   bare-local, getstatic, action-half shape, and external-name formatting.
>
> **Not yet done (next increments):** route the field/array/`arraylength`/
> `athrow`/`monitor` null-deref opcodes through an equivalent action helper
> (currently only the invoke site emits the JEP-358 shape; getfield still uses
> the older ad-hoc `pop_object_ref_ctx_with` message); real
> `LocalVariableTable` name resolution (step 4 — currently always `<localN>`);
> the `-XX:±ShowCodeDetailsInExceptionMessages` opt-out flag (step 5b); and the
> deopt-gated JIT parity (step 6).

Status: design / not started (foundation partially present). M. Synthesize
HotSpot-style "Cannot invoke `String.length()` because `<expr>` is null"
messages by analyzing the bytecode at the NPE throw site.

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

## Current state (cited)

The foundation is partly there but inconsistent and not JEP-358-formatted.

- **`RuntimeError::NullPointerException { message: Option<String> }`** is the
  carrier; most throw sites pass `message: None`. Examples:
  `interpreter.rs:14998`, `:16992`, `:17285`, `:18959`, `:18963` (the
  JIT-fallback and generic deref helpers).
- **A few sites already build an ad-hoc message** — but not in JEP 358 format
  and only for a subset:
  - `invokevirtual` null receiver: `interpreter.rs:11572`
    `Some(format!("Cannot invoke {method_name} on null"))`. This is close to
    JEP 358's verb but lacks the owner class + descriptor (`"String.length()"`)
    and the `because "<expr>" is null` clause.
  - `athrow` of null: `interpreter.rs:8857` `"cannot throw null"`.
  - getfield on a non-heap receiver: `interpreter.rs:7965` (a `[BADRECV]`
    diagnostic, not a user message).
- **`getExtendedNPEMessage` is stubbed to null.**
  `native-builtins/src/lib.rs:5018`:
  ```
  registry.register("java/lang/NullPointerException", "getExtendedNPEMessage",
      "()Ljava/lang/String;", |_ctx, _args| Ok(Some(Value::Object(None))));
  ```
  So even when a message exists, the JDK's extended-message accessor returns
  null, and `Throwable.getMessage()` for a no-message NPE shows nothing.
- **The null-deref opcodes** that must be covered all live in the interpreter
  dispatch (`vm/src/runtime/interpreter.rs`): `getfield`/`putfield` (receiver
  null), `invokevirtual`/`invokeinterface`/`invokespecial` (receiver null),
  `arraylength`, `iaload`/`aaload`/`bastore`/... (array null), `athrow` (null),
  `monitorenter`/`monitorexit` (null). Each throws `RuntimeError::
  NullPointerException` today; most with `message: None`.

Net: the exception carrier supports a message, one opcode builds a half-message,
and the extended-message native is a null stub. The analysis to *synthesize* the
JEP 358 string from the bytecode + operand provenance does not exist.

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

## Implementation steps (ordered)

1. **Action half for all null-deref opcodes.** Centralize NPE creation in one
   helper that takes `(opcode, cp_operand, frame)` and builds the action string;
   route the `message: None` sites through it. Immediate, high value.
2. **`getExtendedNPEMessage` native** returns the stored/synthesized message
   instead of null (`lib.rs:5018`).
3. **Bounded backward expression analysis** producing the `because "<expr>"`
   clause; start with `aload`/`getfield`/`getstatic`/`aaload`, add `invoke` and
   array index forms.
4. **LocalVariableTable name resolution** (use real local names when the
   attribute is present; `reader/` already parses it) — otherwise `<localN>`.
5. **`-XX:±ShowCodeDetailsInExceptionMessages` flag** to match HotSpot's opt-out
   (and the `VmConfig` plumbing).
6. **JIT parity**: the JIT-thrown NPE paths (`interpreter.rs:16992`, `:17285`)
   currently can't know the bci precisely (the JIT signals NPE out-of-band).
   Either thread the trapping bci through the JIT NPE signal, or fall back to the
   action-only message for JIT-originated NPEs. Full parity is gated on
   `real-frame-deopt.md` (which makes the trapping bci available).

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

## Effort

M. The action half + the `getExtendedNPEMessage` un-stub (steps 1–2) is S and
delivers most of the user-visible value. The backward expression analysis
(steps 3–4) is M. Full JIT-NPE parity (step 6) is gated on `real-frame-deopt.md`.
