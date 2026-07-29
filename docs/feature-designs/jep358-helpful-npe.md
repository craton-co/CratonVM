# JEP 358 — Helpful NullPointerException Messages

> **Increment 6 landed (2026-07-28) — merge-point blocks.** The last standing
> divergence-log entry against this feature is closed: the expression
> reconstruction no longer bails inside a basic block whose *incoming* operand
> stack is non-empty.
>
> `simulate_to` walks straight-line from the trapping instruction's block leader
> with an empty simulated stack. That models a leader correctly only when the
> real stack is empty there — which a control-flow **merge** point is not: the
> join of a ternary, of a `&&`/`||` short-circuit, or of a `switch` arm carries
> the predecessors' value. The leader is then typically the `astore` of
> `T x = cond ? a : b;`, whose very first pop underflowed and abandoned the whole
> reconstruction, so `x.deref()` fell back to the action-only message. That is a
> valid HotSpot shape, which is why increment 5's 41-case probe (all
> straight-line) never caught it — but HotSpot itself prints the full
> `because "x" is null` clause there, and the shape is everywhere in real code.
>
> - **Underflow is now information, not failure.** `pop_slot` yields an
>   `UNKNOWN_SLOT` once the block's own simulated suffix is exhausted, meaning
>   "this operand predates the block". Entries stay correctly aligned relative to
>   the **top** of the stack — which is how every caller indexes them — so an
>   operand that genuinely reaches below the block boundary still reports an
>   unknown producer and still yields the action-only message. The change can
>   only turn a bail into "correct" or "still bails"; it can never rename an
>   operand.
> - **Arithmetic / conversion / comparison opcodes are modelled.** They carry no
>   nameable expression (`describe_producer` returns `None` for them) but sit in
>   the prefix of the trapping statement's own block constantly — `a[i + 1]`,
>   `sink += a[0]` — where an unmodelled opcode used to abandon the walk. The
>   category-ambiguous stack shufflers (`dup2`, `dup_x1`, `pop2`, `swap`) are
>   deliberately left unmodelled: their entry count depends on operand
>   categories, and a mis-shaped stack would name the *wrong* value.
> - **Verified** against JDK 25 `getExtendedNPEMessage` with a 40-case fixture
>   (`vm/tests/resources/cratonvm/DiffNpeMessage.java`, wired into
>   `differential.rs::diff_npe_messages`) covering every null-deref opcode plus
>   13 merge-block shapes, byte-identical in the interpreter and after JIT
>   warm-up. Three unit tests in `exceptions.rs` pin the merge-point, the
>   arithmetic-prefix and the still-unnamed-operand behaviours.
> **Increment 6b — the opt-out flag now matches HotSpot.** With
> `-XX:-ShowCodeDetailsInExceptionMessages`, HotSpot's `getMessage()` is `null`;
> CratonVM returned the pre-JEP-358 diagnostic text instead, and the invoke site
> (unconditionally on since increment 1) returned the full JEP-358 string — so
> the flag whose purpose is HotSpot parity was the one place that diverged.
>
> The gate is now three-valued rather than two. `env_cache::helpful_npe_opcodes()`
> means "handle this the HotSpot way" and is true when the flag is on **or**
> explicitly off; only the `-1` sentinel (an embedder or the in-process Rust test
> harness that never wires the flag) is false, which is what preserves the legacy
> diagnostic strings for exactly the callers increment 3 wanted them for. The new
> `helpful_npe_suppressed()` is true only for an explicit opt-out
> (`-XX:-…` or `CRATONVM_HELPFUL_NPE_OPCODES=0`); `helpful_npe_invoke_message`
> and `helpful_npe_opcode_message_parts` then return the empty string, which
> `throw_runtime_error`'s NPE arm maps back to a `None` message. The empty-string
> marker exists because the throw sites must hand a `String` to
> `pop_object_ref_ctx_with` and cannot pass `None` themselves; no Rust-side path
> produces a deliberately empty NPE message, and a message user code supplied
> (`Objects.requireNonNull(x, "charset")`) is untouched — as on HotSpot, whose
> flag only affects the VM's *implicit* NPEs. `jit_npe_message_gated` checks the
> suppression first, since the gate it used to consult is now true in that state.
>
> - Step 6's remaining bci-gated item is unchanged: a JIT NPE that deopts
>   without a precise trapping bci still cannot name a field/invoke operand.

> **Increment 5 landed (2026-06-19) — differential compliance pass + default
> flip to HotSpot parity.** A 41-case lambda-free probe (`scratch/npeprobe/`)
> covering every null-deref opcode and every expression-reconstruction shape was
> diffed against JDK 25 `getExtendedNPEMessage`, **both with and without debug
> info**; CratonVM is now **byte-identical** to HotSpot on all 41 in both modes.
> The interpreter path (steps 1–5) is complete; the default is flipped on.
> Six compliance gaps were found and fixed (all in `exceptions.rs::helpful_npe`
> + the `CpResolver`), none of which the hand-written store-free unit tests had
> exercised:
> - **Owner rendering.** Invoke owner uses descriptor-with-dots (`[I`,
>   `java.lang.Integer`), *not* the `int[]` external form; only `java.lang.String`
>   → `String` and `java.lang.Object` → `Object` are shortened (every other
>   `java.lang` type stays qualified). New `render_owner`; `action_invoke` now
>   builds via `render_method`.
> - **Parameter rendering.** Params use the external form (`Object[]`, `int`,
>   `java.lang.CharSequence`) but with `String`/`Object` shortened (`render_param`
>   → `shorten_jlang`).
> - **Static-method slot 0.** In a `static` method slot 0 is `<local0>`, not
>   `this`; the `CpResolver` gained `is_static_method()` (looked up from the
>   trapping method's access flags) and `render_local` honors it.
> - **`byte/boolean array`.** `baload`/`bastore` (shared by `byte[]` and
>   `boolean[]`) spell the element type `byte/boolean`, not `byte`.
> - **`aaload` index reconstruction.** `arr[idx]` now reconstructs the real
>   index sub-expression (`a[0]`, `a[i]`, `a[Owner.f]`) instead of the
>   fabricated `a[...]`; falls back to action-only if the index can't be named.
> - **Invoke-result + checkcast producers.** A directly-null invoke result reads
>   `the return value of "Owner.m(params)"`; nested it renders inline
>   (`Owner.m().field`). A `checkcast` is transparent (`((String) o)` → `o`).
>   This needed `simulate_to`/`apply_stack_effect` to (a) thread the resolver so
>   it can size invoke args from the descriptor, and (b) model **stores**
>   (`astore`/`istore`/…) — the single most important fix, since every
>   `Type x = null; … x.deref()` emits a store the linear simulation previously
>   bailed on, which is why locals never got a `because` clause before.
> - **No "because the receiver is null".** `combine` now emits action-only when
>   the expression is unknown (HotSpot never prints that phrase). The expression
>   reconstruction returns a `Producer { text, is_invoke }` instead of a bare
>   `String` so the top-level `the return value of "…"` phrasing and quoting are
>   correct.
>
> The linear-from-bci-0 simulation stays conservative: any branch or unmodeled
> opcode before the trap bails to the (valid) action-only HotSpot shape, so it is
> never *wrong*, only sometimes less detailed than HotSpot on complex methods.
>
> **Default flip.** `VmConfig.show_code_details_in_exception_messages` now
> defaults `true` (matching HotSpot). The `vm-cli` flag is tri-state
> (`Option<bool>`): absent → the on default; the JDK `-XX:+`/`-XX:-` spellings
> rewrite to `=true`/`=false`. The `env_cache` `-1` sentinel (set() never called)
> still reads off, so the in-process Rust test harness and un-wired embedders
> keep the legacy strings; only CLI app runs get the on default. Opt out per-run
> with `-XX:-ShowCodeDetailsInExceptionMessages` (or `CRATONVM_HELPFUL_NPE_OPCODES=0`).
> Step 6 (JIT-NPE field/invoke names) remains gated on `real-frame-deopt.md`.
> A pre-existing, *separate* bug surfaced during probing: some intrinsified
> receivers (`String.charAt`/`substring`, certain lambda/`Callable` dispatch)
> skip the null check entirely (no NPE thrown) — orthogonal to message
> construction, tracked separately.

> **Increment 4 follow-up landed — inline-codegen null-check path.** Step 6's
> *first* "Still not done" item (the x64 inline null-check stubs carried no
> action code) is now done; the deopt-overlapping half (field/invoke names) is
> still gated on `real-frame-deopt.md`. What shipped:
> - **Single-source action vocabulary.** The JEP-358 operation-kind codes moved
>   to `cratonvm_jit_api::npe_action` (the one crate both the JIT and VM crates
>   depend on; the JIT crate cannot depend on the VM crate). The VM's
>   `helpful_npe::jit_action` module now re-exports it. The vocabulary is
>   **extended to every primitive element kind** — `ALOAD_/ASTORE_` ×
>   {INT, LONG, FLOAT, DOUBLE, BYTE, CHAR, SHORT, OBJECT} plus `ARRAY_LENGTH`
>   (codes `0..=17`, append-only/stable since the JIT bakes them into RWX code).
>   `jit_action_message` maps the 10 new codes to their verbatim HotSpot
>   `BytecodeUtils` strings ("Cannot load from long array", "Cannot store to
>   char array", …).
> - **New `jit_npe_with_action(code)` helper** (`vm/src/jit/helpers.rs`, ABI
>   field 41 in `JitRuntimeHelpers`, appended at the end so prior golden offsets
>   are stable). Sets `JIT_PENDING_NPE` *with* the passed action code +
>   `JIT_DEOPT_PENDING`, then the stub loads the `i64::MIN` sentinel.
> - **Per-action inline stubs (`jit/src/x64.rs`).** `null_check_store_stubs`
>   became `Vec<(action, patch_offset)>`; `emit_null_check_array_load`/`_store`
>   take the action, derived from the trapping opcode (`array_opcode_npe_action`
>   reads `code[bc_pc]`); `arraylength` passes `ARRAY_LENGTH`; the two
>   non-opcode intrinsic null-checks (`Arrays.fill`, the per-element intrinsic)
>   pass `NONE`. `emit_null_check_store_stubs` now groups sites by action and
>   emits **one cold stub per distinct code**, each calling
>   `jit_npe_with_action(code)` — replacing the single shared stub that called
>   `jit_bastore(0)` and therefore fabricated `ASTORE_BYTE` ("Cannot store to
>   byte array") for *every* inline array opcode (the bug this fixes). The hot
>   path (`TEST RAX,RAX; JZ`) is byte-for-byte unchanged; only the cold
>   out-of-line stubs differ (≤ ~18 per method, bounded by distinct element
>   kinds touched).
> - **Default unchanged / gating.** The action code is always recorded but is
>   surfaced *only* through the existing `jit_npe_message_gated`
>   (`-XX:±ShowCodeDetailsInExceptionMessages` / `CRATONVM_HELPFUL_NPE_OPCODES`,
>   default off). With the gate off the inline path emits the same unmessaged
>   NPE as before — so there is **no observable default-behavior change**, and
>   no separate codegen gate is needed (the codegen has no hot-path cost and is
>   strictly more correct). Verified in a clean worktree at HEAD + only these
>   changes: `bt18 = 68332206` (correct, == HotSpot) at 28s wall (within the
>   ~20–33s baseline) → no JIT throughput or correctness regression — expected,
>   since `bt18` never dereferences a null array and so never reaches a per-action
>   stub.
> - **Tests.** `jit-api` golden-ABI tests bumped to 42 fields; the x64
>   `test_inline_{arraylength,iaload}_null_throws_npe` tests now also assert the
>   threaded action code, and a new `test_inline_castore_null_threads_char_action`
>   proves a `char[]` *store* threads `ASTORE_CHAR` (both direction and element
>   type precise); `exceptions.rs` `jit_action_messages_match_hotspot` gained the
>   10 new element-kind strings.
> - **Still not done (unchanged from below):** field/invoke JIT NPEs need the
>   precise trapping bci (→ field name / method owner+name), which
>   `real-frame-deopt.md` provides — so they stay action-only-or-unmessaged. The
>   differential compliance pass vs HotSpot's `getExtendedNPEMessage` on
>   JIT-compiled methods and the default-flip (Increment 3) remain. A
>   pre-existing nuance: HotSpot spells `baload`/`bastore` null as
>   "byte/boolean array"; both the interpreter (`ArrayElemKind::Byte`) and this
>   JIT path spell it "byte" — consistent across both paths, tracked as a
>   separate uniform-fix follow-up, not introduced here.

> **Increment 4 landed.** Step 6 (partial, deopt-independent): action-only
> JEP-358 messages for **JIT-originated** NPEs, via the doc's offered fallback —
> "thread the operation kind through the JIT NPE signal" — since the precise
> trapping bci needs `real-frame-deopt.md` (unstarted) and the JIT NPE signal
> carried no context.
> - **Signal widened.** `JIT_PENDING_NPE` (a bare `bool`) gains a companion
>   `JIT_PENDING_NPE_ACTION: Cell<u8>` (`vm/src/jit/helpers.rs`) carrying a
>   `helpful_npe::jit_action` code, set in lockstep via the new
>   `set_jit_pending_npe_action(code)` and reset to `0` by every bare
>   `set_jit_pending_npe()` so a stale code can never leak onto an unrelated NPE.
>   `take_jit_pending_npe_action()` / `stash_jit_pending_npe_action()` mirror the
>   existing take/stash (OSR re-stash preserves the action).
> - **Annotated helpers.** The array/length helpers that *know* their operation —
>   `jit_baload`/`jit_bastore` (byte), `jit_iaload`/`jit_iastore` (int),
>   `jit_aaload`/`jit_aastore` (object), `jit_arraylength` — now record their
>   action. `jit_getfield` stays unmessaged (the helper has only a resolved slot
>   index, not the field name HotSpot needs, so a message would diverge — no
>   fabricated text).
> - **Message mapping (`exceptions.rs`).** `helpful_npe::jit_action_message`
>   maps each code to its **verbatim** HotSpot `BytecodeUtils` action-only string
>   ("Cannot read the array length", "Cannot load from int array", "Cannot store
>   to object array", …) — action-only is a valid HotSpot shape when the null
>   expression can't be reconstructed (which is exactly the no-bci JIT case).
>   `jit_npe_message_gated` returns `None` (today's unmessaged NPE) unless the
>   `-XX:±ShowCodeDetailsInExceptionMessages` / `CRATONVM_HELPFUL_NPE_OPCODES`
>   gate is on, so the default path is byte-for-byte unchanged.
> - **Drains updated.** The four interpreter JIT-NPE drains
>   (`interpreter.rs`: `execute_jit_call` early, the OSR bail, and the two
>   void-return store drains) take the action and attach the gated message; the
>   OSR re-stash preserves it.
> - **Tests:** `helpful_npe_tests::jit_action_messages_match_hotspot` (exact
>   strings + `NONE`/unknown → no message) and
>   `jit_npe_message_gated_is_none_when_gate_off`; the 8 `jit_*_null_sets_pending_npe`
>   helper regressions and all 15 `helpful_npe` tests stay green.
> - **Still not done:** ~~the **inline-codegen** null-check path (x64 deopt
>   stubs) does not yet carry an action~~ — **DONE** (see the "Increment 4
>   follow-up" note at the very top: the inline stubs now thread the per-opcode
>   action via `jit_npe_with_action`). Field/invoke JIT NPEs still stay
>   unmessaged (no name/owner available without the precise bci that
>   `real-frame-deopt.md` provides).

> **Increment 3 landed (2026-06-18).** Step 5b: the real HotSpot opt-out flag
> `-XX:±ShowCodeDetailsInExceptionMessages` now drives the non-invoke opcode
> routing, replacing the interim `CRATONVM_HELPFUL_NPE_OPCODES`-only gate.
> - New `VmConfig.show_code_details_in_exception_messages` (default `false`,
>   pending the message-string compliance soak; HotSpot's own default is `true`).
> - `vm-cli` parses `-XX:+/-ShowCodeDetailsInExceptionMessages` (rewritten to the
>   clap `--XX:ShowCodeDetailsInExceptionMessages` toggle) and publishes it to
>   `env_cache::set_show_code_details_in_exception_messages` before `Vm::new`.
> - `env_cache::helpful_npe_opcodes()` now returns: the explicit
>   `CRATONVM_HELPFUL_NPE_OPCODES` env var if set (developer override, wins),
>   else the `-XX` flag value, else the built-in default (off). The increment-1
>   invoke-site message remains unconditionally on.
> - Remaining: step 6 (JIT-NPE parity, deopt-gated) and flipping the default to
>   HotSpot parity (`true`) after the differential compliance run. Embedding
>   (`libcratonvm`) does not yet wire the flag from its `VmConfig` — follow-up.

> **Increment 2 landed (2026-06-18).** Extends the JEP-358 message shape from
> the increment-1 invoke site to the remaining interpreter null-deref opcodes,
> and replaces the synthetic `<localN>` spelling with real source names when a
> `LocalVariableTable` is present. What shipped in increment 2:
> - **Action halves for every non-invoke null-deref opcode** in a set of small
>   `helpful_npe::action_*` builders (`exceptions.rs`): `action_read_field`
>   (`Cannot read field "x"`), `action_assign_field` (`Cannot assign field
>   "x"`), `action_array_length` (`Cannot read the array length`),
>   `action_array_load` / `action_array_store` (`Cannot load from <T> array` /
>   `Cannot store to <T> array`, with `<T>` ∈ {int,long,float,double,byte,
>   char,short,boolean,object} via the new `ArrayElemKind` enum), `action_monitor`
>   (`Cannot enter synchronized block`), `action_throw` (`Cannot throw
>   exception`). All match the HotSpot `BytecodeUtils` wording.
> - **Generalized expression analysis.** `null_expr_for_invoke_receiver` now
>   delegates to a new `null_expr_at_depth(code, trap_bci, depth_below_top,
>   resolver)` that reconstructs the null operand at an arbitrary operand-stack
>   depth (0 = top-of-stack for getfield/arraylength/monitor/athrow, 1 = the
>   putfield receiver and the `*aload` array, 2 = the `*astore` array). Reuses
>   the same increment-1 backward `simulate_to` walk + `describe_producer`.
> - **`combine_opt`** emits the action-only string (no fabricated `because`
>   clause) when the operand can't be classified — matching HotSpot, which
>   omits the clause rather than printing "the receiver". (The invoke site keeps
>   the increment-1 `combine`, whose `None` arm reads "because the receiver is
>   null", for backward compatibility with its shipped tests.)
> - **Interpreter routing** (`vm/src/runtime/interpreter.rs`): `getfield`
>   (~8003), `putfield` (~8250), the `*aload` family (~7040), `aastore`
>   (~7110), the `iastore`/`fastore`/`bastore`/`castore`/`sastore` family
>   (~7165), `lastore` (~7205), `dastore` (~7235), `arraylength` (~8690),
>   `athrow`-null (~9010), `monitorenter` (~9235) and `monitorexit` (~9320) now
>   build the JEP-358 message via the shared `helpful_npe_opcode_message` /
>   `helpful_npe_opcode_message_parts` glue (the `_parts` form is `thread`-free
>   so it can run inside a `pop_object_ref_ctx_with` closure).
> - **Real `LocalVariableTable` names.** `CpPoolResolver` now carries the
>   trapping method's name+descriptor and implements `local_name(slot, bci)` by
>   scanning the method's `Code.LocalVariableTable` attribute (already parsed by
>   `cratonvm_reader`) for an entry whose `[start_pc, start_pc+length)` live
>   range covers `bci` — rendering the real source name (e.g. `items`) in place
>   of `<localN>`. Falls back to `<localN>` / `this` when no LVT is present.
> - **Default-off flag.** All increment-2 opcode routing is gated behind
>   `CRATONVM_HELPFUL_NPE_OPCODES` (`env_cache::helpful_npe_opcodes()`, default
>   OFF). With the flag unset the default path keeps the exact prior ad-hoc
>   null-NPE strings, so no message string regresses; setting it to anything
>   non-empty / non-`0` switches the covered opcodes to the JEP-358 shape. The
>   increment-1 invoke-site message is unconditionally on and unaffected.
> - **Tests:** `exceptions.rs` `mod helpful_npe_tests` gained
>   `increment2_action_shapes`, `combine_opt_action_only_when_unknown`,
>   `getfield_read_receiver_is_field_of_this`, `arraylength_of_local`,
>   `array_store_to_local`, `monitorenter_of_local`, and
>   `lvt_name_resolves_when_present` (asserts the LVT name renders when present
>   and falls back to `<local3>` when absent).
>
> **Still not done:** step 6 JIT-NPE parity (deopt-gated) and the
> `-XX:±ShowCodeDetailsInExceptionMessages` HotSpot opt-out spelling (the
> increment-2 `CRATONVM_HELPFUL_NPE_OPCODES` env flag is the interim knob).

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
   - **Done (helper path):** the array/length JIT helpers record their action
     (Increment 4).
   - **Done (inline-codegen path):** the x64 inline null-check stubs now thread
     the per-opcode action via `jit_npe_with_action` (Increment 4 follow-up; see
     top note). Covers every array element kind + `arraylength`, action-only.
   - **Remaining (bci-gated):** field/invoke names need the precise trapping bci
     from `real-frame-deopt.md`; until then those JIT NPEs stay
     action-only-or-unmessaged.

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
