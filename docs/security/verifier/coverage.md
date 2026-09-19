# Bytecode verifier — JVMS coverage, gaps, and residual attacker capability

Scope: `classloading/src/{verifier,bytecode_verifier,verify_insn,verify_frame,vtype}.rs`.

This document is the honest inventory. It states, rule by rule, what CratonVM's
verifier enforces, where the enforcement lives, which test pins it, and — in
the last two sections — exactly what an attacker with control over a `.class`
file can still do today.

Terminology used throughout:

* **Pass 2** — structural class checks (`verifier::verify_class_structure`):
  access flags, final class/method constraints, `Code` attribute presence.
* **Pass 3 / structural** — JVMS §4.9.1 static constraints over a method body
  (`verifier::verify_method_structural`): decode, branch and handler bounds,
  local-index and `max_locals` conformance. **Consults no class hierarchy.**
* **Pass 3 / type-state** — JVMS §4.10 operand-stack and local type checking.
  Two algorithms: the *split verifier* (`StackMapTable`-driven linear walk,
  §4.10.1) and *type inference* (worklist dataflow to fixpoint, §4.10.2).

---

## 1. Verifier selection

| Class file major | `StackMapTable` | Algorithm | Where |
| --- | --- | --- | --- |
| ≥ 51 (Java 7+) | present | split verifier, **strict** for untrusted classes | `bytecode_verifier.rs:252` onward |
| ≥ 51 | absent, method has branches or handlers | **rejected** | `bytecode_verifier.rs:238` |
| ≥ 51 | absent, straight-line method | linear walk from the initial frame | `bytecode_verifier.rs:360` |
| ≤ 50 | absent, branches or handlers | type inference (§4.10.2) | `bytecode_verifier.rs:263` → `verify_by_inference` |
| ≤ 50 | present, frames cover every merge point | split verifier (lenient) | `bytecode_verifier.rs:365` |
| ≤ 50 | present, **partial** frames | **failover to type inference** | `bytecode_verifier.rs:335` and `:576` |
| any, method uses `jsr`/`ret` | — | structural only, then a version-gated accept/reject | `verifier.rs:333` |

**The pre-Java-7 failover is new and closes a type-confusion hole.** Previously
a class file declaring major 50 and shipping a *token* `StackMapTable` skipped
the worklist (the dispatch tested `stack_map_table.is_none()`) and then took the
lenient linear walk, whose strict merge checks are gated on
`requires_stack_map` — i.e. off for major ≤ 50. A branch to a target with no
declared frame was therefore typed by whatever *fell through* into it. Reaching
that target along the branch with a shallower operand stack is a run-time
underflow the verifier had signed off on. JVMS §4.10 already defines the remedy
for this exact version range (failover to §4.10.2 inference), and that is what
the two `pre_java7_partial_frames` sites now do: any handler entry or branch
target without a declared frame hands the whole method to the worklist, which
merges at every edge and needs no declared frames at all.

### Type inference (JVMS §4.10.2) — implementation status

**Implemented**, in two mirrored copies:
`bytecode_verifier::verify_by_inference` (`bytecode_verifier.rs:736`, the main
path) and `verifier::verify_pre_java7_inference` (`verifier.rs:962`, the path
taken by the non-`jsr` methods of a `jsr`-bearing class).

Both are worklist dataflow to fixpoint over the instruction graph, merging
frame states at every branch target (`merge_inference_frame`) and at every
exception-handler entry whose protected range covers the current pc. The lattice
is `vtype::VType`: `Top`, `Int`, `Float`, `Long`, `Double`, `Null`, `ObjectRef`,
`ArrayRef`, `UninitializedThis`, `Uninitialized(offset)`, `ReturnAddress(pc)`,
with category-2 slot pairing, array covariance (`vtype.rs:370`) and
least-upper-bound merges (`vtype.rs:328`).

The one **subset that is not modelled** is subroutines (§4.10.2.5) — see §4
below. That subset does not fall through to acceptance by the inference engine;
it is diverted before the walk starts.

---

## 2. Rule-by-rule coverage

`file:line` is the enforcement site. Line numbers drift; the function names do
not.

### JVMS §4.9.1 — static constraints on the `Code` array

| Rule | Enforced | Where | Test |
| --- | --- | --- | --- |
| `code_length ≥ 1` | yes | `verifier.rs` `verify_method_structural` | `verifier.rs::structural_rejects_empty_code_array` (the reader also rejects it in `attribute.rs::validate_attribute_shape`) |
| `code_length ≤ 65535` | yes | `reader/verified_code.rs::VerifiedCode::decode` | reader-side |
| Every byte decodes to a legal instruction | yes | `verified_code::decode` via `verify_method_structural` | corpus `branch_into_the_middle_of_an_instruction_rejected` |
| Branch/switch targets inside the code array | yes | `verified_code::checked_target` + `verify_method_structural` | corpus `branch_target_outside_the_code_array_rejected` |
| Branch/switch targets land on an instruction boundary | yes | `verified_code::decode` + `verify_method_structural` | corpus `branch_into_the_middle_of_an_instruction_rejected` / `..._accepted` |
| `tableswitch` / `lookupswitch` padding and operand decode | yes | `reader/instruction.rs::decode` | reader-side |
| `tableswitch` `low ≤ high`, `lookupswitch` key ordering | **NO** — see §3 | — | — |
| `wide` is a prefix, never standalone | yes | rejected at decode (`instruction.rs:694`); defence in depth in `verify_method_structural` and `verify_insn.rs::Instruction::Wide` | — |
| Local index `< max_locals` for every local-addressing opcode | yes | `verify_method_structural` (`local_operand`) **and** `verify_frame::local_load`/`local_store` | `verifier.rs::structural_rejects_local_index_past_max_locals`; corpus `local_index_past_max_locals_rejected` / `..._accepted` |
| Category-2 local needs slots `n` and `n+1`, both `< max_locals` | yes | `local_operand` width 2; `verify_frame::local_store_wide` | `verifier.rs::structural_rejects_cat2_local_straddling_max_locals`; `verify_frame.rs::cat2_store_at_last_slot_rejected` |
| `max_locals` covers the method's own arguments | yes | `verify_method_structural` (`argument_slots`) | `verifier.rs::structural_rejects_max_locals_smaller_than_the_argument_slots`; corpus `max_locals_smaller_than_arguments_rejected` / `..._accepted` |
| `ret` index `< max_locals` | yes | `verify_method_structural` | — |
| `ret` reachable from a `jsr` prologue | yes | `verifier.rs::covered_ret_sites` | — |
| Handler `0 ≤ start_pc < end_pc ≤ code_length` | yes | `reader/attribute.rs::validate_exception_range`, then `verify_method_structural` | `verifier.rs::structural_rejects_inverted_handler_range` / `..._empty_handler_range`; corpus `inverted_handler_range_rejected` (reader-level — see below) |
| Handler `handler_pc < code_length` | yes | `reader/attribute.rs::validate_exception_range`, then `verify_method_structural` | `verifier.rs::structural_rejects_handler_pc_past_the_code_array`; corpus `handler_pc_past_the_code_array_rejected` (reader-level — see below) |
| Handler `start_pc` / `end_pc` / `handler_pc` on instruction boundaries | yes | `verify_method_structural` | corpus `handler_pc_inside_an_instruction_rejected` / `well_formed_handler_accepted` |
| Handler `catch_type` is a `CONSTANT_Class` or 0 | yes | `reader/attribute.rs::validate_catch_type`, then `bytecode_verifier::catch_type_of` | `verifier.rs::rejects_a_handler_whose_catch_type_is_not_a_class` / `..._is_out_of_range` / `accepts_a_handler_whose_catch_type_is_a_class` / `accepts_a_catch_all_handler`; corpus `handler_with_a_non_class_catch_type_rejected` (reader-level — see below) |
| Exception table ordering (first-match semantics) | n/a — ordering is a *dispatch* rule, not a validity rule; JVMS imposes no ordering constraint | — | — |

**The structural scan used to run only for `jsr`-bearing classes.** Ordinary
classes went straight to `bytecode_verifier::verify_method`, which validated
the *code array* (through `verified_code`) but never looked at the exception
table. A handler whose `handler_pc` landed mid-instruction was accepted, because
the type-state walk only consults `handler_pc` as a map key and simply never
matched it — while the interpreter, which does dispatch there, began decoding at
a mid-instruction offset. `verify_method` now runs the scan first
(`bytecode_verifier.rs:190`).

**Three exception-table rules are enforced twice, at two layers.** JVMS §4.7.3
states `start_pc < end_pc`, `handler_pc < code_length` and "`catch_type` is 0 or
a `CONSTANT_Class`" as *format* constraints on the `Code` attribute, and
`cratonvm_reader` checks all three while decoding it — so a class file carrying
one of those shapes is a `ClassReaderError` and never reaches Pass 3 — on the
eager and lazy decode paths alike (`decode_attribute` re-runs both checks, so a
`Code` nested inside another attribute cannot slip past). The verifier keeps its
own copy of each check regardless, because a `CodeAttribute` built in memory
rather than decoded from bytes — the `LazyAttribute::new_decoded` form used by
synthetic stubs, cached class data and tests — reaches the verifier without ever
passing through the reader. The practical consequence for this table is that a
**corpus** case for one of these
three rules can only ever observe the reader's verdict, which is why the three
corpus cases assert there (`reject_at_parse`) while the verifier's half is
pinned by the in-memory `verifier.rs` unit tests cited alongside them. The
boundary-landing rule in the row between them has no reader-side counterpart —
the reader does not decode instructions — so it stays a pure corpus case.

### JVMS §4.10.1 / §4.10.2 — type checking

| Rule | Enforced | Where | Test |
| --- | --- | --- | --- |
| Operand stack never underflows | yes | `verify_frame::pop` | corpus `stack_underflow_rejected` / `balanced_stack_accepted` |
| Operand stack depth never exceeds `max_stack` | yes | `verify_frame::push` | corpus `stack_overflow_rejected` / `stack_within_max_stack_accepted` |
| Category-2 values occupy two operand-stack slots and are never split | yes | `verify_insn::check_no_split`, `is_cat2_upper_half`, `pop_n_slots` (`pop`, `pop2`, `dup*`, `swap`) | pre-existing `verify_insn.rs` tests |
| Category-2 local pairs are never split | yes | `verify_frame::local_store` (invalidates a base at `n-1`), `local_load_wide` (checks the `Top` upper half) | `verify_frame.rs::storing_over_the_upper_half_invalidates_the_cat2_base`; corpus `split_category2_local_rejected` / `intact_category2_local_accepted` |
| Frame merge at every branch target (inference path) | yes | `merge_inference_frame` / `merge_frame_into` | corpus `type_confused_merge_rejected` / `consistent_merge_accepted` |
| Frame merge at every exception-handler entry (inference path) | yes | `verify_by_inference` handler loop | corpus `well_formed_handler_accepted` |
| Declared frame checked on the fall-through edge (split verifier) | yes | `bytecode_verifier.rs:369` | — |
| Declared frame checked on the branch edge (split verifier) | yes | `bytecode_verifier.rs:585` | — |
| Every branch target has a declared frame (strict mode) | yes, for untrusted major ≥ 51 | `bytecode_verifier.rs:548` | corpus `java8_branch_to_a_frameless_target_rejected` / `java8_branch_with_a_declared_frame_accepted` |
| Java 7+ method with branches/handlers must ship a `StackMapTable` | yes | `bytecode_verifier.rs:229` | corpus `java8_branch_without_a_stack_map_table_rejected` |
| Unreachable code rejected (strict mode, major ≥ 51) | yes | `bytecode_verifier.rs:441` | — |
| Type lattice: `Top`, `null`, `uninitialized(offset)`, `uninitializedThis`, array subtyping | yes | `vtype.rs::is_assignable_to` / `merge` / `array_is_assignable` | `vtype.rs` tests |
| Uninitialized value not assignable where an initialized reference is expected | yes | `vtype.rs:302` (the `_ => false` arm) | `vtype.rs` tests |
| `invokespecial <init>` receiver must be uninitialized | yes | `verify_insn.rs:990` | `verify_insn.rs::invokespecial_init_on_initialized_ref_rejected`; corpus `init_called_twice_rejected` / `init_called_once_accepted` |
| `<init>` owner must be the current class or its **direct** superclass (`uninitializedThis`) | yes | `verify_insn.rs:998` | `verify_insn.rs::invokespecial_init_on_uninitialized_this_rejects_indirect_superclass` |
| `new`-site type must equal the `<init>` owner (`uninitialized(offset)`) | yes, on the `verifier.rs` paths | `verifier::check_new_init_owner_match` | `verifier.rs` owner-match tests |
| A constructor may not `return` with `this` uninitialized | yes | `verify_insn.rs::Instruction::Return` | `verify_insn.rs::constructor_return_before_super_init_rejected` / `..._accepted`; corpus `constructor_returning_uninitialized_this_rejected` / `constructor_calling_super_accepted` |
| `athrow` operand assignable to `Throwable` | yes | `verify_insn.rs:1162` | `verify_insn.rs` athrow tests |
| `areturn` value assignable to the declared return type | yes | `verify_insn.rs:809` | `verify_insn.rs` areturn tests |
| `aastore` value is a reference, array element type is a reference | yes | `verify_insn.rs:287` | — |
| `multianewarray` dimensions ≥ 1 and ≤ the descriptor's bracket count | yes | `verify_insn.rs:1127` | — |
| Uninitialized value dead across a backward branch (§4.10.2.4) | **NO** — see §3 | — | — |
| `jsr`/`ret` subroutine inlining (§4.10.2.5) | **NO** — see §4 | — | — |

### JVMS §4.4 / §4.9.1 — constant-pool cross-checks

| Rule | Enforced | Where | Test |
| --- | --- | --- | --- |
| Index in range for every opcode operand | yes (all resolvers return `None` for an out-of-range index and the callers reject) | `verify_insn.rs` resolvers | corpus `new_with_an_out_of_range_operand_rejected` |
| `new` operand is a `CONSTANT_Class` naming a non-array type | yes | `verify_insn.rs::Instruction::New` | corpus `new_with_a_non_class_operand_rejected` / `..._accepted`; `verify_insn.rs::new_of_an_array_type_rejected` |
| `checkcast` / `instanceof` / `anewarray` / `multianewarray` operand is a `CONSTANT_Class` | yes | `verify_insn.rs`, via `get_class_name{,_arc}` (tag-checked) | `verify_insn.rs::instanceof_with_a_non_class_cp_entry_rejected` / `..._accepted` |
| `getfield`/`putfield`/`getstatic`/`putstatic` operand is a `CONSTANT_Fieldref` | yes | `resolve_field_type` | — |
| `Fieldref.class_index` is a `CONSTANT_Class` | yes | `resolve_field_type` | — |
| `Fieldref` `NameAndType` carries a well-formed **field** descriptor | yes | `resolve_field_type` + `vtype::is_valid_field_descriptor` | `verify_insn.rs::getstatic_with_a_malformed_field_descriptor_rejected` / `..._accepted`; corpus `malformed_field_descriptor_rejected` / `..._accepted` |
| `invokevirtual` operand is a `CONSTANT_Methodref` | yes | `resolve_method_name_and_type` | — |
| `invokeinterface` operand is a `CONSTANT_InterfaceMethodref` | yes | `resolve_imethod_name_and_type` | — |
| `invokestatic` / `invokespecial` operand is `Methodref` or `InterfaceMethodref` | yes | `resolve_method_or_imethod_name_and_type` | — |
| `invokedynamic` operand is a `CONSTANT_InvokeDynamic` | yes | `resolve_invokedynamic_type` | — |
| Every `invoke*` `NameAndType` carries a well-formed **method** descriptor and a non-empty name | yes | `checked_name_and_type` + `vtype::is_valid_method_descriptor` | `verify_insn.rs::invokestatic_with_a_malformed_method_descriptor_rejected`; corpus `malformed_method_descriptor_rejected` / `..._accepted` |
| `ldc` loads a category-1 constant of a legal tag; `ldc2_w` a category-2 one | yes | `verify_ldc` / `verify_ldc2w` | `verify_insn.rs` ldc tests |
| `StackMapTable` `Object` verification type resolves to a `CONSTANT_Class` | yes | `vtype::from_verification_type_info` | — |
| `invokespecial` of `<init>` names `<init>`; `invoke*` does not name `<init>`/`<clinit>` | **partial** — the `<init>` receiver rule is enforced, but `invokevirtual/static/interface` naming `<init>` is not rejected | — | — |
| Method name / field name / class name character legality (JVMS §4.2.2) | **NO** — only emptiness and, for class names in descriptors, the absence of `.`/`[`/`;` | — | — |

### JVMS §5.4.5 / Pass 2 — class-level structural rules

| Rule | Enforced | Where | Test |
| --- | --- | --- | --- |
| `FINAL` + `ABSTRACT`, `INTERFACE` without `ABSTRACT`, `INTERFACE` + `FINAL`, `ANNOTATION` without `INTERFACE` | yes | `verify_class_access_flags` | `verifier.rs` flag tests |
| Abstract method flag combinations; interface methods `PUBLIC` or `PRIVATE` | yes | `verify_method_access_flags` | `verifier.rs` tests |
| Cannot extend a `FINAL` class | yes | `verify_final_class_constraint` | `verifier.rs` tests |
| Cannot override a `FINAL` method (package-private scoped by runtime package) | yes | `verify_final_method_constraint` | `verifier.rs` tests |
| Non-abstract non-native methods have `Code`; abstract/native do not | yes | `verify_code_attribute_presence` | `verifier.rs` tests |
| Concrete class implements every inherited abstract method | **NO — deliberately disabled** (`verifier.rs:1846`, `:1854`) | — | — |
| §4.10.1.8 protected-member access (receiver assignable to the current class) | **NO** — see §3 | — | — |

---

## 3. Remaining gaps

Ordered by how much an attacker gains.

1. **Pass 3 type-state is deferred entirely for user-defined-loader classes.**
   `class_manager.rs::define_class_with_options` sets
   `defer_loader_sensitive_pass3 = loader_aware_resolution() && loader is
   UserDefined`, and `loader_aware_resolution()` defaults **on**. Every
   Spring / WildFly / H2 / Elasticsearch application class is defined by a
   user-defined loader, so for those classes **no operand-stack or local
   type-state check is made at all**. `vm_util.rs` deliberately does not repeat
   Pass 3 at link time either.
   The deferral exists for a real reason — the hierarchy adapter available at
   that point cannot always keep two loaders' same-named classes apart, so its
   *assignability verdicts* can be wrong, and a wrong verdict is a spurious
   `VerifyError` on bytecode HotSpot accepts. It is a statement about the
   hierarchy, not about the rest of the verifier.
   **Partially closed:** the deferred path now runs
   `verifier::verify_class_structural_bytecode` (the hierarchy-independent
   §4.9.1 scan) and rejects on failure. What remains deferred is exactly the
   type-state verdict. Closing it fully requires a loader-faithful hierarchy
   adapter at define time.

2. **Subroutines (`jsr` / `ret`, JVMS §4.10.2.5) are not type-state verified.**
   See §4.

3. **§4.10.2.4 — an `uninitialized(offset)` value may survive a backward
   branch.** JVMS requires that no uninitialized value be live across a
   backward branch (it would alias two distinct allocations at the same
   `new` offset). The worklist does not check this. Consequence: a crafted
   loop can make two live objects share the `Uninitialized(offset)` identity,
   so `invokespecial <init>` on one initializes *both* in the verifier's model.
   The receiver still has the right *shape* (the `new` site's class), so this is
   an initialization-order violation rather than a type confusion.

4. **§4.10.1.8 protected-member access is not checked by the verifier.**
   Access control is enforced at *resolution* time by
   `access_control.rs` (JVMS §5.4.4), which covers `public`/`protected`/
   package-private/`private` visibility. The additional verifier-time rule —
   for a `protected` member of a superclass in another package, the receiver
   type must be assignable to the currently-verified class — is not enforced.
   Consequence: a class in package `p` can invoke `q.Super`'s protected member
   on a receiver typed `q.Super` rather than on itself. Resolution-time access
   control still requires that the caller be a subclass, so the escalation is
   narrower than a full bypass.

5. **`tableswitch` `low ≤ high` and `lookupswitch` key ordering are not
   checked.** The decoder reads the operands and every target is
   bounds-checked, so this cannot reach an out-of-range jump; a mis-ordered
   `lookupswitch` degrades a run-time binary search to a wrong-but-in-range
   arm.

6. **Name legality (JVMS §4.2.2) is not validated** beyond emptiness and the
   descriptor-internal `.`/`[`/`;` rules. A method named `foo;bar` loads.

7. **Concrete-class abstract-method implementation is not enforced**
   (`verify_abstract_method_implementation` is compiled but not called —
   `verifier.rs:1846`). HotSpot also defers this to invocation time
   (`AbstractMethodError`), so this is a compatibility choice, not a hole.

8. **The `new`-site owner-match (§4.10.1.9) runs only on the `verifier.rs`
   paths**, not inside `bytecode_verifier::verify_method`. The type-state check
   still requires the receiver to be `Uninitialized(offset)` and initializes it
   to the *constructor's* owner, so a mismatch produces a wrongly-typed
   reference rather than an uninitialized one.

9. **Two near-duplicate implementations of both the split verifier and the
   inference worklist** exist (`bytecode_verifier.rs` and `verifier.rs`). They
   are kept in sync by hand. Every fix above had to be applied twice; a future
   fix applied once would leave the `jsr`-bearing-class path behind.

---

## 4. Configurations that weaken verification

| Configuration | Default | Effect | Observable as |
| --- | --- | --- | --- |
| `--noverify` / `-Xverify:none` (`VmConfig::skip_verification`) | **off** | Pass 2 and Pass 3 are skipped entirely at define time and link time. The class reaches the interpreter and JIT unverified. | `type_maps::verification_status(id) == Skipped` (set by `class_manager.rs:5070`) |
| `-Xverify:remote` (`XverifyMode::Remote`) | **on** | Bootstrap-loaded classes under `java/`, `jdk/`, `sun/`, `com/sun/` skip Pass 3 (`vm_util::verifier_skip_eligible`). Both the loader identity *and* the name prefix must match, so a forged `java/lang/Evil` from an application loader is still verified. | `verification_status == Skipped` |
| `-Xverify:all` (`XverifyMode::All` → `ClassManager::strict_verification`) | off | Strengthens, never weakens, in three places at once: forces strict mode for *every* class including the trusted JDK image (`verify_bytecode_strict`); withdraws the `CRATONVM_LOADER_AWARE_RESOLUTION` Pass-3 deferral two rows below; and stops `verifier_skip_eligible` skipping link-time Pass 2 for bootstrap classes. | `ClassManager::strict_verification()`; a bootstrap-trusted class with a frameless branch target is rejected (`bytecode_verifier::tests::xverify_all_makes_a_trusted_class_reject_what_remote_accepts`) |
| `DefineClassOptions::skip_verification` | per call site | Trusted runtime-generated bytecode (ByteBuddy, CGLIB, JDK proxies, `Unsafe.defineClass`, `Lookup.defineClass`) skips both passes. `proxy_gen` deliberately sets `false`. | `verification_status == Skipped`; `ClassManager::class_skip_bytecode_verification(id)` |
| `CRATONVM_LOADER_AWARE_RESOLUTION` | **on** | When on, user-defined-loader classes skip the Pass-3 *type-state verdict* (gap 1 above). Structural bytecode verification still runs. `-Xverify:all` withdraws this deferral. | `debug!` at `class_manager.rs:5078`; per-method `FastPathVeto::IncompleteWalk` in the published type maps |
| `CRATONVM_ALLOW_JSR_RET` | off | Forces structural-only acceptance of `jsr`/`ret` methods at **any** class-file version, including major ≥ 51 where the opcodes are illegal. | `warn!` at `verifier.rs` (jsr acceptance site, `via_escape_hatch = true`); `FastPathVeto::Subroutine` |
| Class-file major ≤ 50 containing `jsr`/`ret` | n/a | Accepted after the structural scan **without** subroutine type-state verification, matching HotSpot's *load* decision. The operand stack and locals inside the subroutine body are unverified. | `warn!` at the jsr acceptance site; `FastPathVeto::Subroutine`, zero oop-map rows |
| Bootstrap-trusted classes (`class_is_bootstrap_trusted`) | n/a | Lenient mode: frameless branch targets and unreachable code are tolerated instead of rejected. Requires bootstrap loader identity **and** a JDK package prefix. | `FastPathVeto::IncompleteWalk` where the walk skipped a region |
| CDS-cached bytes | n/a | Verification skipped (verified at archive-creation time). | `verification_status == Skipped` |
| Synthetic stub classes | n/a | No bytecode to verify. | `verification_status == Skipped` |

Every entry in this table is *observable*: `type_maps::verification_status`
distinguishes `Unknown` (not verified yet) from `Skipped` (never will be), and
`MethodTypeMaps::fast_path_veto()` names the reason a method's proof is
incomplete. Consumers that see either must scan conservatively and must not take
the unchecked interpreter fast path.

---

## 5. What an attacker can still do with a malformed class today

Assume the attacker controls the full byte content of a `.class` file and can
get it loaded. Ranked by severity.

1. **Skip type-state verification entirely by using a custom class loader.**
   With the default `CRATONVM_LOADER_AWARE_RESOLUTION=1`, a class defined
   through a user-defined `ClassLoader.defineClass` gets Pass 2 plus the
   structural §4.9.1 scan, and **no §4.10 type-state check**. Within the
   structural envelope — every branch in range and on a boundary, every local
   index below `max_locals`, every handler well-formed, operand-stack depth
   *not* checked — the attacker has free rein over types: store an `int` into a
   local and read it back as a reference, call a method on a receiver of the
   wrong class, return an unrelated type. This is a full memory-safety escape
   and it is the single most important open item. It is *not* a new regression;
   it predates this work and is documented here for the first time.

2. **Skip everything with `--noverify` / `-Xverify:none`, or by reaching a
   `skip_verification: true` define path.** Both are explicit operator or
   embedder choices, both are recorded as `VerificationStatus::Skipped`, and
   both are outside the verifier's control.

3. **Run unverified operand-stack and local type-state inside a `jsr`
   subroutine** by shipping a class file with major ≤ 50 that contains
   `jsr`/`ret`. The structural scan still applies (targets in range, on
   boundaries, `ret` covered by a reachable prologue, handler ranges valid), so
   this is not arbitrary — but inside the subroutine body the operand stack is
   unchecked. HotSpot loads the same class and applies §4.10.2.5 inlining;
   CratonVM loads it and does not. Logged as a `warn!` on every occurrence, and
   the affected methods publish zero oop-map rows with
   `FastPathVeto::Subroutine`, so the GC scans them conservatively and the
   unchecked interpreter fast path is denied.

4. **Alias two allocations through one `Uninitialized(offset)`** with a
   backward branch (gap 3). The resulting reference has the correct class, so
   this yields an object whose constructor ran twice or not at all, not a
   type confusion.

5. **Invoke a `protected` superclass member on a foreign receiver** of the
   declaring type (gap 4), within what resolution-time access control permits.

6. **Load a class with names that are illegal per §4.2.2** (gap 6). No memory
   effect; it can confuse tooling and log output.

What an attacker **cannot** do any more, that they could before this work:

* reach the interpreter through an exception handler whose `handler_pc` is
  out of range or mid-instruction (any class, any loader);
* under-declare `max_locals` so that argument slots or a `wide` local operand
  index past the end of the runtime frame;
* split a `long`/`double` local pair and read the torn value back as a wide
  value;
* panic the verifier with `wide lstore 65535` (`index + 1` overflow) or with an
  unterminated `L…` descriptor in any `NameAndType` (an out-of-range slice);
* get a `new` / `instanceof` past the verifier with a constant-pool operand
  that is not a `CONSTANT_Class`, or a `new` of an array type;
* disable the operand type check on a field access by supplying a malformed
  field descriptor (which used to degrade to `Top`, and everything is
  assignable to `Top`);
* have a constructor `return` without ever calling `this()` / `super()`;
* exploit a partial `StackMapTable` on a major-50 class to reach a frameless
  branch target typed from the fall-through edge.

---

## 6. Error contract

Every rejection is `cratonvm_types::error::LinkageError::VerifyError { class_name,
method_name, message }`, surfaced to callers as
`VmError::Linkage(LinkageError::VerifyError { .. })`. Messages name the rule and,
where the walk knows it, the bytecode offset — `verify_instruction` failures are
wrapped with `"at bytecode offset {pc}: …"` by both walkers, and the structural
scan embeds `at offset {pc}` / `handler_pc={n}` directly.

There is no `panic!` / `unwrap` / `expect` on attacker-controlled input in the
verifier: every constant-pool lookup returns `Option` and is converted to a
`VerifyError`, every descriptor slice bound comes from
`vtype::field_descriptor_len` (which never reports a length past the end of the
input), and every index arithmetic that can overflow uses `checked_add`.

---

## 7. Test inventory

| Location | What it covers |
| --- | --- |
| `classloading/tests/verifier_corpus/` | Matched valid/invalid **class byte arrays**, built in Rust (`builder.rs`), parsed by `cratonvm_reader::read_class`, verified through `verifier::verify_class_bytecode`. 26 cases: stack under/overflow, local index, `max_locals` vs arguments, split category-2, out-of-range branch, mid-instruction branch, four handler shapes, type-confused merge, uninitialized-this escape, double `<init>`, bad CP tag (three shapes), malformed field and method descriptors, three `StackMapTable` shapes, and a composite valid class. |
| `classloading/src/verifier.rs` (tests) | `verify_class_structural_bytecode` unit coverage: handler shapes, local index, `max_locals`, branch targets, empty code, abstract/native skip. |
| `classloading/src/verify_frame.rs` (tests) | `max_locals` bound, `pad_locals_to` semantics, category-2 pair invalidation and split detection, `u16::MAX` index overflow, `uninitializedThis` tracking. |
| `classloading/src/verify_insn.rs` (tests) | CP tag checks for `new` / `instanceof`, descriptor well-formedness for field and method refs, category-2 local round-trip and split, `<init>`-before-`return`. |
| `classloading/src/vtype.rs` (tests) | Field and method descriptor validators, the unterminated-`L` regression, 255-dimension bound. |

Run: `cargo test -p cratonvm-classloading verifier`.
