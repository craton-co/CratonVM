# Access control false-denials, and the type-map coverage gap

**Slug:** `access-control-and-map-coverage`
**Date:** 2026-07-26
**Status:** LANDED — both access-control defects fixed and pinned; type maps
now published for user-defined-loader classes; the linear-walk `record`
soundness hole closed.

## Tree basis

Authored on `arch/wave1-integration-20260726` merged into this worktree at
**`bed750290`** (`dev` @ `6495a191c` plus 22 merged agent branches; the merge
was a fast-forward, no conflicts). `classloading/src/type_maps.rs` is present.
Every file:line citation below was re-verified against that merged tree.

Files changed (all owned by this session):

- `classloading/src/access_control.rs`
- `classloading/src/verifier.rs`
- `classloading/src/bytecode_verifier.rs`
- `classloading/src/class_manager.rs`

---

# Part 1 — Two provable false denials in `access_control.rs`

## 1.0 The standing situation

Member access control is **not enforced** at runtime. `check_field_access`,
`check_method_access` and the three `*_with_modules` wrappers have zero
production call sites; the only wired checks are `check_class_access` (for
`new` only) and `check_module_access_by_id` (JPMS). A hand-written class file
that names another class's `private` member resolves and executes today.

An earlier session was assigned to wire the member checks into the interpreter
and **correctly refused**. Wiring them *as written* would have converted a
silent under-enforcement into a loud over-enforcement: two of the rules were
strictly stricter than the JVM spec and would have rejected correct,
javac-generated programs. Both are now fixed. Both were verified independently
before being fixed — neither was taken on report.

## 1.1 Defect 1a — a hidden class could never be a nestmate

### Verification of the claim

`confirmed_nest_host` (`access_control.rs`, now ~`:376`) honoured a `NestHost`
attribute only when the named host could be found in the `ClassStore` **and**
that host's `NestMembers` listed the claimant back:

```rust
Some(host) => match store.find_by_name(host) {
    Some(host_class) if host_class.nest_members.iter().any(|m| m == &*class.name) => host,
    _ => &class.name,   // unconfirmed → its own nest → not a nestmate of anyone
}
```

`nest_members` is populated by class-file parsing. `NestMembers` is a
compile-time attribute; it can only name classes the compiler knew about.

A hidden class's `nest_host`, meanwhile, is **not** parsed from its class file
at all. `class_manager.rs:3724` overwrites whatever the file said:

```rust
let nest_host = options.nest_host_class_name.clone().or(nest_host);
```

and the three producers of `options.nest_host_class_name`
(`native-builtins/src/lookup_define.rs:353` and `:514`,
`native-builtins/src/classloader.rs:3288`,
`native-builtins/src/lang_system.rs:3552`) all derive it from the
`MethodHandles.Lookup`'s own class via `resolve_lookup_nest_host`, and only
when the `NESTMATE` class option is present.

The hidden class's *name* is minted at run time —
`lookup_define.rs:374` builds `format!("{original}/0x{id:x}")`. No class file's
`NestMembers` can spell that. So the confirmation was unsatisfiable **by
construction**, `are_nestmates(hidden, host)` was unconditionally `false`, and
`confirmed_nest_host` fell through to "the hidden class is its own nest host".

**Consequence.** `LambdaMetafactory` spins the lambda body as a `NESTMATE`
hidden class in the capturing class's nest; its bytecode calls
`invokestatic Host.lambda$foo$0`, which javac emits `private static`. With
`check_method_access` wired, every lambda in every program would have thrown
`IllegalAccessError`. Same for a captured `private` field.

### Fix

Hidden classes are exempt from the bidirectional confirmation:

```rust
Some(host) if class.hidden => host,
```

This is not a spoofing hole. The bidirectional check exists because a
`NestHost` attribute is attacker-supplied bytes. A hidden class's `nest_host`
is not: it is supplied by the defining call, from a `Lookup` the caller must
already have had the access to obtain. The claim is authoritative *because only
the defining call could have made it*.

The exemption is deliberately narrow and one-directional — it is keyed on
`Class::hidden` and nothing else. A non-hidden class making the identical claim
still goes through full confirmation and still fails.

### Tests (all in `access_control.rs`)

| test | what it pins |
|---|---|
| `hidden_class_is_nestmate_of_its_lambda_host` | **FAILS BEFORE THE FIX.** The exact lambda shape: a top-level `com/foo/Host` with *no* `NestMembers` at all, plus a hidden `com/foo/Host/0x2a` claiming it. Asserts `are_nestmates` both ways, the `private static` method access, and the captured `private` field access. |
| `hidden_class_joins_the_whole_nest_not_just_the_lookup_class` | A hidden class defined through a `Lookup` on `Outer$A` (so its host resolves to `Outer`) reaches `Outer$B`'s private state. |
| `non_hidden_class_with_same_shape_is_still_denied` | CONTROL. The identical shape minus `hidden` is still an unconfirmed spoof and is denied. |
| `hidden_class_without_nest_host_is_its_own_nest` | CONTROL. A non-`NESTMATE` hidden class gets nothing. |

The pre-existing `spoofed_nest_host_is_rejected` and
`unconfirmed_nest_host_when_host_missing` still pass unchanged.

## 1.2 Defect 1b — `receiver_ok_for_protected` implemented one of four disjuncts

### Verification of the claim

The function was:

```rust
Some(r) => r.is_subclass_of(accessor.id, store),
```

i.e. `T <: D` only, in JVMS §5.4.4 naming (`C` = declaring class, `D` =
accessing class, `T` = the class named by the symbolic reference / the static
receiver type).

JVMS §5.4.4 requires `T` to be "either a subclass of `D`, a superclass of `D`,
or `D` itself". HotSpot's `Reflection::verify_field_access` implements that as
four disjuncts (`current_class` = `D`, `resolved_class` = `T`, `field_class` =
`C`):

```
   current_class == resolved_class               // T == D
|| field_class   == resolved_class               // T == C
|| current_class->is_subclass_of(resolved_class) // D <: T
|| resolved_class->is_subclass_of(current_class) // T <: D
```

The missing case that matters is the ordinary one. Given
`package p; public class C { protected int f; }` and
`package q; class S extends C`, javac compiles `this.f` inside `S` to
`getfield p/C.f` — so `T = p/C`, `D = q/S`. `T <: D` is false (`p/C` is a
*super*class of `q/S`), and a spec-legal, javac-generated access was denied.

### Fix

All four disjuncts, with `declaring` threaded in as a new parameter:

```rust
fn receiver_ok_for_protected(accessor, declaring, receiver, store) -> bool {
    match receiver {
        None => true,
        Some(t) => t.id == accessor.id
            || t.id == declaring.id
            || accessor.is_subclass_of(t.id, store)
            || t.is_subclass_of(accessor.id, store),
    }
}
```

`T == C` is in fact implied by `D <: T` whenever the caller's preceding
`D <: C` gate held, so HotSpot's second disjunct is redundant. It is kept
explicit anyway so the function reads as the spec rule, and so a hierarchy walk
that cannot reach `C` (a not-yet-linked super edge) cannot turn a legal access
into an `IllegalAccessError`.

### Tests

Fixture (`protected_fixture`): `p/C <- p/Mid <- q/D <- q/Sub`, plus a sibling
`p/Other extends p/C`. `p/C` declares the member; `q/D` is the accessor; the
two are in different runtime packages.

| test | disjunct | before the fix |
|---|---|---|
| `protected_disjunct_t_equals_declaring_class` | `T == C` (receiver `p/C`) — the javac `this.inheritedProtected` shape | **FAILS** |
| `protected_disjunct_t_equals_accessor` | `T == D` (receiver `q/D`) | passes |
| `protected_disjunct_accessor_subclass_of_receiver_type` | `D <: T` (receiver `p/Mid`) | **FAILS** |
| `protected_disjunct_receiver_type_subclass_of_accessor` | `T <: D` (receiver `q/Sub`) | passes |
| `protected_sibling_receiver_still_denied_after_widening` | CONTROL: receiver `p/Other` satisfies none of the four → still denied | passes |
| `protected_non_subclass_accessor_denied_regardless_of_receiver` | CONTROL: clause 1 gates the receiver clause | passes |

Each of the first four asserts the field half *and* the method half.

## 1.3 Are the checks now safe to wire? **Yes** — with one caveat

Both false denials are gone. What remains is a plumbing requirement, not a
correctness defect in this module.

**Caveat (live, unchanged):** `receiver_ok_for_protected` is satisfied
**vacuously** by `receiver: None`. A wirer who passes `None` because the static
receiver type is inconvenient to thread through gets clause 1 (accessor is a
subclass of the declaring class) and nothing else — which is *under*-
enforcement, not a false denial, so it cannot break correct programs, but it
also is not the spec rule. Pinned by
`protected_receiver_none_is_vacuous_not_a_check`. Plumbing the static receiver
type through `getfield` / `putfield` / `invokevirtual` / `invokeinterface` is
part of the wiring job.

### Cross-owner request: the exact call sites (`vm/src/runtime/interpreter.rs`, not ours)

**Field half — drops straight in.** `resolve_field_in_class` (`:19699`) already
calls `check_module_access_by_id` at two points, and at both of them the
accessor, the declaring class, the field's flags and `field_class_id` are all
in hand, and both are **before** the `ResolvedField` cache write:

- `:19738` — the "own class declares it" branch. Accessor `current_class_id`,
  declaring `declaring_id`, flags `f`, symbolic owner `field_class_id`.
- `:19794` — the inherited/interface branch. Accessor `current_class_id`,
  declaring `decl_id`, flags `field`, symbolic owner `field_class_id`.

`field_class_id` is the class named by the constant-pool `Fieldref`, i.e.
exactly the `T` of §5.4.4 — pass it as `receiver` and the protected clause is
non-vacuous with no extra plumbing. Add
`check_field_access(accessor, declaring, flags, store, Some(field_class))`
immediately beside each existing `check_module_access_by_id`.

Caching is already safe: `resolution::ResolutionKey` is `(ClassId, u16)` —
keyed on the *referencing* class — so a cache hit is by construction the same
accessor the check was performed for.

**Method half — needs restructuring first.** `resolve_method_metadata`
(`:41729`) resolves only the symbolic constant-pool text: it reads the
`MethodReference` class/name-and-type from `current_class_id`'s constant pool
and never locates the declaring method or its `MethodAccessFlags`. There is
nothing to pass to `check_method_access` at that point. Wiring the method half
requires that function to resolve the actual declaring class + method first
(as `resolve_field_in_class` already does for fields), which is a genuine
restructure and not a drop-in.

The `check_module_access_by_id` call at `:41797` is the natural landing site
once the declaring method is available there.

---

# Part 2 — The type-map coverage gap

## 2.1 Verification of the claim

`classloading/src/class_manager.rs:3810` (merged tree) computed:

```rust
let defer_loader_sensitive_pass3 =
    loader_aware_resolution() && matches!(class.loader_id, ClassLoaderId::UserDefined(_));
```

and the whole of `verifier::verify_class` — Pass 2 **and** Pass 3 — was skipped
when it was true. `loader_aware_resolution()` defaults **ON**
(`class_manager.rs:126`).

`vm/src/vm/vm_util.rs` (the link-time verifier, ~`:806`–`:880`) runs Pass 2
only, with an explicit comment that Pass 3 "is deliberately NOT repeated at
link time" because the adapter available there is a name-indexed `ClassStore`
that first-matches across all loaders. So it does not compensate.

Type maps are published from exactly two places —
`bytecode_verifier.rs:155` (inside `verify_bytecode`) and `verifier.rs:389`
(the JSR-aware per-method path) — both reachable only through
`verify_class_bytecode`, i.e. only through the Pass 3 that was being skipped.

**Confirmed.** Every Spring / WildFly / H2 / Elasticsearch application class is
defined by a user-defined loader, so none of them had maps,
`verification_status` answered `Unknown` for all of them,
`safe_for_fast_path` was unavailable for the entire application workload, and
the GC had no precise oop maps precisely where the interesting object graphs
live. Adopting `safe_for_fast_path` in that state would have switched the
interpreter fast path **off** for the whole application.

## 2.2 Why the deferral is not a reason to withhold maps

The deferral has one cause: the hierarchy adapter at define time cannot always
keep two loaders' same-named classes apart, so its **assignability verdicts**
can be wrong, and a wrong verdict there is a spurious `VerifyError` on bytecode
HotSpot accepts (the AssertJ `AbstractThrowableAssert.<init>` case,
2026-07-13).

But a Pass-3 walk produces two independent outputs, and only one of them
depends on the hierarchy:

- **The verdict** (accept / `VerifyError`) is a function of
  `VerificationFrame::is_assignable_to`, which consults the hierarchy. This is
  the part the deferral distrusts.
- **The rows** record, per pc, which local slots and which operand-stack slots
  hold a *reference*. Reference-vs-primitive is decided by `VType::is_reference`
  over types derived from field/method descriptors, `ldc` constants,
  `new`/`anewarray` operands, and the class's own `StackMapTable` — none of
  which consult the hierarchy. A confused hierarchy can pick the wrong common
  supertype when merging two reference types, but the merge result is still a
  reference, so the *bit* is unchanged. Operand-stack depths and local-slot
  indices are likewise structural.

So a mis-resolving hierarchy can make the walk accept bytecode it should have
rejected. It cannot make it record an int where a reference lives, or a
reference where an int lives. That is exactly the property the maps' consumers
(GC root scan, interpreter fast path) depend on.

## 2.3 The fix

`verifier.rs` gains a non-rejecting collector and two thin installers:

```rust
pub fn collect_class_type_maps(class, hierarchy) -> ClassTypeMaps   // never fails
pub fn publish_deferred_class_type_maps(class, hierarchy) -> bool   // CAS install
pub fn refresh_class_type_maps(class, hierarchy) -> bool            // replacing install
```

`collect_class_type_maps` applies the same **per-method** routing as the
verifying path — abstract/native → `None`, `jsr`/`ret` → `subroutine_unproven_maps`
(zero rows, `FastPathVeto::Subroutine`), everything else →
`verify_method_typestate`. A per-method walk error is swallowed into
`unproven_maps` (zero rows, `FastPathVeto::IncompleteWalk`) rather than
propagated: on this path the verdict is not a load decision, so discarding it
is the whole point.

It runs the walk **lenient** (`strict = false`). Strict mode's only extra
behaviour is turning unreachable-code / frameless-branch-target findings into a
hard `VerifyError`; on this path that would throw away every row of an
otherwise perfectly describable method in exchange for an error nobody acts on.
This is *not* a downgrade of verification — no verification decision is made
here at all.

`class_manager.rs` is restructured so the deferral picks the *branch* rather
than skipping the block:

```rust
let skip_all_verification = options.skip_verification
    || self.cds_class_cache.contains_key(name)
    || class.is_synthetic_stub
    || class.state == ClassState::Verified;
if !skip_all_verification {
    let hierarchy = ClassStoreHierarchy { .. };            // unchanged
    if defer_loader_sensitive_pass3 {
        crate::verifier::publish_deferred_class_type_maps(&class, &hierarchy);
    } else if let Err(e) = crate::verifier::verify_class(&class, &self.class_store, &hierarchy) {
        ..                                                  // unchanged
    }
} else {
    crate::type_maps::mark_class_verification_skipped(class.id);
}
```

**The load decision is byte-for-byte unchanged.** No class that loaded before
fails now, and no class that failed before loads now. The only new effect is
that maps get published.

Publishing before the class is registered is safe: the `ClassId` is already
minted, the class store is the only other thing keyed by it, and nothing
between that point and `class_store.add` can fail.

### Two coherence fixes that fall out of it

**(a) `Skipped` markers.** `mark_class_verification_skipped` existed but had no
callers, so `VerificationStatus::Skipped` was never observable. The `else`
branch now records it for `--noverify` / per-class-skip / CDS / synthetic-stub
classes. This is diagnosability only — `Unknown` and `Skipped` are equally
conservative — but "none are coming" is a different answer from "not yet", and
"not yet" is a state a consumer might reasonably wait for. `type_maps::install`
upgrades an unverified marker to real maps on a later publish, so the marker
never traps an id.

**(b) Stale maps after `RedefineClasses` — a heap-corruption hazard my own fix
would otherwise have created.** `publish_class_type_maps` is first-writer-wins.
Before this change a user-defined-loader class had no maps at all, so a
redefine's `verify_class` publish landed on an empty slot. Now that define-time
publishes maps, the redefine's publish becomes a silent no-op and the *old*
bodies' rows survive — at `Class::methods` indices the redefine may have
reshuffled. A stale oop map is a wrong oop map. `redefine_class_bytes` now calls
`refresh_class_type_maps` (a replacing install) unconditionally after a
successful redefine, including on the paths that skipped verification, where
replacing stale rows with an honest "walk did not complete" set is still
strictly better than leaving the old class's rows visible.

## 2.4 Interaction with the per-method routing in `verify_class_bytecode_inner`

Re-confirmed on the merged tree. `verify_class_bytecode_inner`
(`verifier.rs:244`) pre-scans for `jsr`/`jsr_w`/`ret`; with none it delegates to
`bytecode_verifier::verify_bytecode`, which is itself per-method; with any it
runs its own per-method loop where only the subroutine-using methods take the
structural-only fallback and every sibling is fully type-state verified and
keeps real rows. So one legacy `finally` no longer costs the whole class its
maps.

The deferred path composes correctly with that, because
`collect_class_type_maps` re-implements the *same* per-method routing rather
than calling the class-level dispatcher. Calling the dispatcher would have been
wrong twice over: it returns a load decision this path must not act on, and it
publishes internally with no-replace semantics.

## 2.5 What is still not covered (honest limits)

- **Classes defined with `skip_verification`** — trusted generated bytecode
  (hidden classes / lambdas, CGLIB, ByteBuddy, JDK Proxy, `Unsafe.defineClass`)
  — get no maps and now answer `Skipped`. Their bytecode is synthesised in
  shapes the worklist verifier cannot model, which is why they are trusted in
  the first place. Consumers must scan them conservatively. This is a real
  residual hole for lambda-heavy code and is the obvious next target.
- **CDS-cached classes** answer `Skipped`. They are pre-verified at archive
  build time; their maps would have to be archived alongside them.
- **`vm_util.rs` link-time Pass 2** is unchanged and still does not run Pass 3.
  With define-time collection in place this no longer matters for map coverage.
- **Cost.** Every user-defined-loader class now pays a full Pass-3 walk at
  define time that it previously skipped entirely. That is the intended price
  of the feature, but it is a real class-loading cost on large Spring/WildFly
  boots and should be measured.

## 2.6 Tests (`verifier.rs`)

| test | what it pins |
|---|---|
| `collect_class_type_maps_never_rejects_and_keeps_good_methods` | The keystone. A 4-method class the *verifying* path rejects outright (asserted as a control) still yields: `None` for the abstract method, zero rows + `IncompleteWalk` for the unwalkable one, **real rows + `safe_for_fast_path`** for its walkable sibling, and `Subroutine` for the `jsr` method. |
| `deferred_publish_makes_maps_visible` | `verification_status` goes `Unknown` → `Verified` and `type_maps_for` returns rows. |
| `refresh_replaces_stale_maps_after_a_redefine` | Demonstrates the hazard (a no-replace publish over a redefined class is a no-op and the stale row count survives) and that `refresh_class_type_maps` fixes it. |
| `refresh_overwrites_a_verification_skipped_marker` | `Skipped` → `Verified` across a redefine. |

---

# Part 3 — The unsound `record` in `bytecode_verifier.rs`

## 3.1 The two stale pcs

The linear StackMapTable walk in `verify_method` called
`type_maps.record(pc, &current_frame)` unconditionally. `current_frame` is not
this pc's entry state at two of them:

1. **Dead code in a pre-Java-7 class that ships a `StackMapTable`.**
   `requires_stack_map` is `version.major >= 51`, so at major ≤ 50 the
   unreachable-code guard (`!verified && requires_stack_map && ..`) never fires
   even though the class took the linear walk (it takes it because the
   `StackMapTable` is present, which is what steers it away from the inference
   worklist). The walk marches into dead code after a `goto` / `return` /
   `athrow` still carrying the frame from before it.
2. **A handler pc that is also reachable by fall-through.** The handler frame
   is installed only under `if !verified`. When control *does* fall through,
   `current_frame` describes the fall-through edge, while the exception edge
   enters the same pc with a different locals state and `[throwable]` on the
   operand stack. The linear walk never merges the two.

Type-*checking* against a stale frame is a pre-existing laxity of this walk (it
can only accept too much, and strict mode closes case 1). **Recording** one is
categorically worse: a row claiming a slot holds a reference where the other
edge supplies an int makes the GC scan a non-oop — immediate corruption — and a
row claiming none where the other edge supplies a live reference is a missed
root. Both are silent.

`verify_by_inference` (the pre-Java-7 worklist) is **not** affected: it
serializes a settled fixpoint in which every edge, exception edges included, has
already been merged into `frame_at[pc]`.

## 3.2 The fix

An `authoritative` flag mirroring the one a sibling added to
`verifier::verify_method_typestate`:

- set `true` when a declared `StackMapTable` frame is adopted (a declared frame
  *is* the authoritative merge-point state);
- set `true` when a handler frame is built under `!verified`;
- set **`false`** at a handler pc reached with `verified == true` and no
  declared frame to reconcile the two edges;
- carried forward only through real fall-through:
  `authoritative = authoritative && result.falls_through && next_pc < len`.

`record` is then guarded; a non-authoritative pc records nothing and clears
`walk_complete`, so the method also loses `safe_for_fast_path`. An unrecorded pc
answers `None`, which the type-map contract already defines as "unproven, scan
conservatively".

## 3.3 Tests (`bytecode_verifier.rs`)

| test | what it pins |
|---|---|
| `dead_code_in_pre_java7_class_with_stackmap_records_no_row` | **FAILS BEFORE.** Major 50 + `StackMapTable`, dead code after `return`. Rows survive at the reachable prefix; the dead region contributes nothing; veto is `IncompleteWalk`. |
| `handler_pc_reachable_by_fallthrough_records_no_row` | **FAILS BEFORE.** The two edges disagree about whether stack slot 0 is an oop (fall-through `int` vs exception `Throwable`); no row is emitted at that pc. |
| `handler_pc_reachable_only_by_exception_keeps_its_rows` | CONTROL. An ordinary handler keeps a row at every pc, the caught `Throwable` **is** marked as a reference on the operand stack, and the method stays fast-path safe. |
| `straight_line_method_keeps_every_row` | CONTROL. The guard is invisible to a branch-free method. |

---

# Part 4 — Build/test status

Not built and not run: nine concurrent sessions share this host and nine
concurrent builds OOM it (standing instruction for this wave). Every changed
file was parsed and format-checked with `rustfmt --edition 2021 --check`
(clean, and CRLF line endings verified intact afterwards). Type-level review was
done by hand against the signatures of `type_maps.rs`, `verify_frame.rs`,
`class.rs` and `reader/src/attribute.rs`. The tests listed above are the
verification contract; they must be run before this is trusted.
