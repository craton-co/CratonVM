# Method and field resolution

Status: partial. Written for the C2 review's P0 *"Centralize method and field
resolution"*.

Acceptance criterion under review:

> No direct metadata-table bypass remains; a repository check rejects new
> bypasses.

**The second clause is met; the first is not yet.** The repository check exists
(`vm/src/runtime/resolve/guard.rs`) and fails the build on a bypass it does not
already know about. 30 known bypasses remain, each with a row naming why and
which migration step retires it. This document is the ordered plan behind those
rows.

---

## 0. The facts everything else follows from

**Fact 1 — there is no member access check on the bytecode path.**

`classloading/src/access_control.rs` implements JVMS §5.4.4 in full. Its own
module docs record the finding:

| function | production call sites |
|---|---|
| `check_class_access` | `vm/src/runtime/interpreter.rs` — `new` only |
| `check_module_access_by_id` | field + method resolution (JPMS only) |
| `check_field_access` | **none** |
| `check_method_access` | **none** |

So a hand-written class file that names another class's `private` or
package-private member resolves and executes. This pass does **not** change
that — see §4 — but it does make the omission visible at the call site instead
of implicit, by requiring every resolution to name an `AccessPolicy`.

**Fact 2 — there are two access-control implementations, and the live one is
not the JVMS one.**

`native-builtins/src/lang_class.rs:525` has its own `check_field_access`,
reached from `Field.get*` / `Field.set*`. It is the only member access check
that runs. It and the JVMS implementation do not agree: this one consults
`setAccessible`, the JVMS one consults nestmates and runtime packages. The two
answer different questions for different callers, which is defensible, but
nothing today states which one a given call site gets.

**Fact 3 — `ClassId`s are allocated per VM.**

`docs/architecture/per-vm-state.md` Fact 1: `ClassStore::next_id` returns
`self.classes.len()` and each `SharedVm` owns its own store, so `ClassId(7)`
names a different class in every VM. Every resolution cache is keyed on a bare
`ClassId`. Four of them were found aliasing across VMs in that pass; a fifth
(`GLOBAL_VTABLE_MANAGER`, item V1) still writes VM B's vtables into VM A's
index.

**Fact 4 — `None` already means two things on a live path.**

`NativeContextImpl::link_resolver_get_method` (`vm/src/vm/vm_exec.rs:6776`)
returns `None` for both "cache cold" and "cached: this member does not exist",
and its comment at `:6791` documents the caller having to guess and eat a
redundant walk. That is the ambiguous-`None` hazard, live, in the reflection
path.

---

## 1. Inventory

Every site that answers "what member does this symbolic reference name", or
reads a metadata table to do so. Counts are the exact non-comment,
non-declaration occurrence counts the guard enforces.

### 1.1 By consumer

| consumer | entry point | cache | VM-keyed? | access control |
|---|---|---|---|---|
| interpreter — CP method ref | `interpreter::invoke::resolve_method_metadata` | `ResolutionCache`, key `(ClassId, cp_index)` | by containment (`SharedVm.classes`) | JPMS module only |
| interpreter — CP field ref | `interpreter::field_access::resolve_field_ref` | same | by containment | JPMS module only |
| interpreter — inline-cache miss | `JvmThread::invoke_cache` → `SharedResolutionState::promoted_invokes`, key `(caller, cp_index, is_special, receiver)` | per-VM by containment | by containment | inherits the CP entry's (i.e. module only) |
| interpreter — `invokedynamic` / condy | `ResolutionCache::{get,put}_call_site` / `_condy` | same | by containment | none |
| JIT — call-site resolution | `vm/src/jit/helpers.rs`, bare `find_method_recursive` | **none** | n/a | `check_class_access` for `new` only |
| JNI `Get{Method,Field}ID` | `vm/src/native/jni.rs` | `LinkResolver`, key `(ClassId, name, descriptor)` | by containment | none (JNI does not mandate it) |
| reflection / method handles | `native-builtins/src/lang_class.rs` via the four `NativeContextImpl::link_resolver_*` bridges | `LinkResolver` | by containment | the second implementation (Fact 2) |
| virtual dispatch | `runtime::vtable::VtableManager`, key `ClassId` | **process-global `OnceLock`** | **NO** — per-vm-state V1 | none |

The only structurally cross-VM item is the last row. Everything else is per-VM
by containment inside `SharedVm.classes` — which is correct, and is why the
resolver's VM check is cheap to satisfy rather than a rewrite.

### 1.2 By site (what the guard counts)

30 `(file, needle)` pairs, 118 occurrences. Grouped by disposition:

| disposition | rows | occurrences |
|---|---|---|
| not a resolution (cache lifecycle, GC roots, teardown) | 6 | 6 |
| unit tests of the primitives | 3 | 10 |
| migration step 1 — reflective (JNI + reflection natives) | 5 | 16 |
| migration step 2 — JIT call-site resolution | 2 | 6 |
| migration step 3 — interpreter hot paths | 12 | 68 |
| migration step 4 — boot-time field offsets, native helper walks | 2 | 12 |
| migration step 5 — the second access-control implementation | — | tracked separately |

The single largest cluster is 27 `find_method_recursive(` calls in
`vm/src/runtime/interpreter/invoke.rs`.

---

## 2. The API

`vm/src/runtime/resolve` — `MemberResolver`, plus the types that make the three
required properties checkable.

### 2.1 VM identity is required, not optional

```rust
pub struct VmScoped<T> { vm: VmId, value: T }
```

`VmId` is the one VM-identity notion (`native-api/src/capability.rs`;
per-vm-state.md says explicitly not to introduce a second). Every input to and
output from `MemberResolver` is `VmScoped`, and the payload cannot be read
without naming a matching `VmId`.

The honest strength of that claim, stated in the type's own docs rather than
left to be discovered: `VmId::from_raw` is public (the boot path needs it), so
this is not a type-level proof that a tag cannot be fabricated. What it is: a
cross-VM read cannot happen *by accident*, because there is no accessor that
takes "the current VM" and every one requires writing down an identity. All
four aliasing caches per-vm-state.md found were accidents of exactly that
shape — a plain map probe with a plain key — and none of them would compile
against this type.

`MemberResolver::new` takes `&SharedVm` and reads `vm_identity` from it. There
is no constructor that does not.

### 2.2 Failure is structured

```rust
pub enum ResolveError {
    NoSuchMethod { class_name, method_name, descriptor },
    NoSuchField  { class_name, field_name },
    IllegalAccess { message },
    NoClassDefFound { class_name },
    IncompatibleClassChange { message },
    ForeignVm { expected: VmId, found: VmId },
    ForeignVtableManager { vm: VmId },
    Pending(ObjectRef),
    Internal { message },
}
```

`ResolveErrorKind` is the discriminant for tests and telemetry.
`From`/`Into` conversions to and from `LinkageError`, `VmError` and
`MethodCallFailed` preserve the kind, so a `NoSuchMethodError` still reaches a
Java `catch` as itself.

"Not cached" is deliberately **not** in that enum. It is a different type:

```rust
pub enum CacheProbe<T> { Hit(T), Miss }
```

A cached `ResolvedMember::NotFound` is a `Hit`. Collapsing to `Option` is
possible but is spelled `into_hit()`, so discarding the distinction is a
greppable act rather than the default.

### 2.3 Access control is applied once, by one implementation

```rust
pub enum AccessPolicy { ModuleOnly, Full }
pub enum MemberFlags { Field(..), Method(..), OwnerOnly }

fn check_member_access(&self, cm, accessor, declaring, member, receiver, policy)
    -> Result<AccessGrant, ResolveError>
```

* It is the only call into `classloading::access_control` from
  `vm/src/runtime/` — the guard enforces that.
* It bumps a process counter, `access_checks_run()`, so "exactly once" is a
  property a test can assert as a delta.
* It returns a `#[must_use] AccessGrant` carrying the policy and the *accessor*
  it was performed for. JVMS §5.4.4 resolution is defined per (referencing
  class, symbolic reference), so a grant is not transferable; keeping the
  accessor on the token is what makes that checkable later.
* `MemberFlags::OwnerOnly` names the constant-pool method path's real
  situation: the module check happens against the owner class *before* the
  hierarchy walk, so there are no method flags yet. Pairing it with
  `AccessPolicy::Full` is rejected as `Internal` rather than silently degrading.

### 2.4 Entry points

| method | replaces |
|---|---|
| `method_ref(caller, cp_index)` | `resolve_method_metadata` at every call site |
| `field_ref(caller, cp_index)` | `resolve_field_ref` at every call site |
| `probe_method_ref` / `probe_field_ref` | direct `.resolution_cache.read().get_*` |
| `declared_method(cm, owner, name, desc)` | the JNI `GetMethodID` walk-then-index block |
| `declared_field(cm, owner, name, desc)` | the JNI `GetFieldID` block |
| `probe_declared` | the four `NativeContextImpl::link_resolver_*` bridges |
| `locate_field(cm, accessor, owner, ..)` | the interpreter's own-fields-then-walk field search |
| `vtable_manager()` | `runtime::vtable::global_vtable_manager()` |

`cm: &ClassManager` is a parameter on the methods that need one, never acquired
internally. That is deliberate: the L10 class-manager lock has a documented
ABBA hazard against `resolution_cache` (the H2 `TestScript` deadlock), and a
resolver that took its own guard would move lock acquisition to a place the
call site cannot see. Call sites keep their guards and their lock order
unchanged.

`vtable_manager()` decides ownership with `Arc::ptr_eq` against
`SharedVm.classes.vtable_manager` — the same allocation this VM handed the
installer — so it detects per-vm-state V1 exactly, without needing the
captureless classloading install hook to learn a `VmId`. It returns
`ForeignVtableManager` rather than a foreign table. **No caller uses it yet**:
this pass adds the detector, not the fallback, because installing a fallback
would change dispatch.

---

## 3. Migrated in this pass

Three sites, all inside `vm/src/runtime/`:

1. **`interpreter/field_access.rs::resolve_field_in_class`** → `locate_field`.
   The own-fields scan, the superclass/superinterface walk, the two copies of
   the JPMS check and both `CRATON_FIELD_TRACE` diagnostics moved into the
   resolver. The function keeps its `cm` guard, and keeps the cache write
   *outside* it per the ABBA rule. The trace moved with the search because only
   the search knows which of its two paths produced the answer.
2. **`interpreter/invoke.rs::resolve_method_metadata`** — its JPMS check now
   goes through `check_member_access(.., MemberFlags::OwnerOnly,
   AccessPolicy::ModuleOnly)`.
3. **`runtime/vtable.rs`** — reached through `MemberResolver::vtable_manager`,
   which adds the per-VM ownership check. `global_vtable_manager()` itself is
   unchanged and still has its existing callers.

Two module declarations in `interpreter.rs` went from private to `pub(crate)`
(`invoke`, `field_access`) and two functions from `pub(super)` to `pub(crate)`
(`resolve_method_metadata`, `resolve_field_ref`), so the resolver can delegate
to the cores. Both are marked "this is the core, not the entry point"; the
guard is what stops a new caller taking the shortcut.

Nothing else changed. No resolution answers differently, no access check was
added or removed, and no public item was renamed or removed.

---

## 4. Migration order

Each step is one reviewable commit. The numbering is the order the risk argues
for, not the order the code appears in.

**Step 1 — reflective resolution.** `vm/src/native/jni.rs` (`GetMethodID`,
`GetFieldID`) and the four `NativeContextImpl::link_resolver_*` bridges in
`vm/src/vm/vm_exec.rs`, then their `native-builtins/src/lang_class.rs`
consumers. `declared_method` / `declared_field` / `probe_declared` already
implement exactly what these do; the move is mechanical. Do this first because
it is the step that retires Fact 4 — `probe_declared` returns `CacheProbe`, so
the "cache cold vs cached-absent" guess disappears rather than being
documented again.

*Cross-owner:* `vm/src/native/`, `vm/src/vm/`, `native-builtins/` — all outside
this pass's edit scope.

**Step 2 — JIT call-site resolution.** `vm/src/jit/helpers.rs` — five
`find_method_recursive` calls and one `check_class_access`. The JIT resolves
call-site targets with an uncached bare walk today, so a JIT-resolved target
and an interpreter-resolved target agree only by construction. Route both
through `method_ref` and they agree by definition.

*Cross-owner:* `vm/src/jit/`.

**Step 3 — the interpreter.** Split in three because it is the hot path.

* **3a — `interpreter/invoke.rs`** (50 occurrences across six needles). The
  27 `find_method_recursive` calls are the largest single cluster in the tree.
  `resolve_field_ref` → `field_ref` is a one-line change per site, but each
  also changes the error type at the site, so they land together with a full
  suite behind them. When 3a lands, `resolve_method_metadata` can go back to
  private.
* **3b — `interpreter.rs` and `interpreter/field_access.rs`** (9). The opcode
  handlers and the field core's own cache probe/writes.
* **3c — `interpreter/constants.rs` and `invokedynamic.rs`** (9). These need a
  `MemberResolver` entry point that does not exist yet: `ResolvedCallSite` and
  condy are a third and fourth member kind alongside method and field. This
  pass did not invent one, because a single consumer is not enough evidence for
  a shape.

**Step 4 — the stragglers.** `vm/src/vm/vm_util.rs` (three boot-time
`FileDescriptor` field offsets) and `vm/src/vm/vm_exec.rs` (nine
`NativeContext` helper walks). Low risk and low value; last on purpose.

*Cross-owner:* `vm/src/vm/`.

**Step 5 — one access-control implementation.** Fold
`native-builtins/src/lang_class.rs::check_field_access` onto
`check_member_access` with `AccessPolicy::Full`. **This one is
behaviour-affecting** and must not be done casually: the two implementations
answer different questions (`setAccessible` versus nestmates/packages), so the
fold needs a policy that covers both, not a substitution.

`the_access_control_implementations_are_the_two_known_ones` in the guard pins
the count at two until then, so a third cannot appear while step 5 is pending.

**Not a step — wiring `AccessPolicy::Full` into the bytecode path.** That is
its own project and it is genuinely blocked, not merely unscheduled. The
cross-package protected clause of JVMS §5.4.4 is satisfied **vacuously** by
`receiver: None`, and `classloading::access_control` pins that with
`protected_receiver_none_is_vacuous_not_a_check`. Passing `None` from
`getfield` / `invokevirtual` does not skip a refinement, it turns the clause
off. Plumbing the static receiver type through the opcode handlers is the bulk
of the work, and it is not a resolution-centralisation change.

---

## 5. The repository check

`vm/src/runtime/resolve/guard.rs`, modelled on
`types/tests/flag_declaration_guard.rs`.

### 5.1 What it looks for

Seven literals: `find_method_recursive(`, `find_field_recursive(`,
`.resolution_cache`, `.link_resolver`, `access_control::check_`,
`resolve_method_metadata(`, `resolve_field_ref(`.

### 5.2 Precision rules

1. **Whole-line comments are skipped.** This tree has hundreds of doc comments
   discussing these functions by name. The comment test is copied from the flag
   guard *including* its warning: a bare `starts_with('*')` is wrong, because
   `*CELL.get_or_init(…)` is a deref, not a block-comment continuation.
2. **A declaration is not a call.** A line containing `fn <needle>` is the
   definition, so a resolution core does not need an allowlist row to declare
   itself.
3. **The field needles carry their leading dot.** `.resolution_cache` matches
   `shared.classes.resolution_cache` and does not match
   `shared.classes.initiating_resolution_cache` — a different table (the JVMS
   §5.4.3 initiating-loader cache) that is not a member-resolution bypass.

Two directories are excluded by rule rather than by allowlist row, because they
own the names: `classloading/` (which defines all of them) and
`vm/src/runtime/resolve/` (the sanctioned wrapper, and the guard's own file).

**Known blind spot**, stated rather than papered over: an aliased import
(`use … find_method_recursive as walk;`) or a re-export under another name is
invisible to a literal scan — the same caveat the flag guard carries for
`format!`-assembled flag names.

### 5.3 Why the allowlist carries counts

Each row is `(file, needle, exact count, reason)`. Presence-only permission
would let a file that already has one bypass grow ten more silently, and with
27 occurrences of one needle in one file, "the file is on the list" is not a
useful unit of permission.

An exact count also fails when a bypass is *removed*, which is deliberate: the
row is then stale and must be tightened, exactly as the flag guard prunes an
exemption whose read site is gone. Migrating a site is a two-line diff — the
migration, and the number.

Every reason must classify itself as `migration step N`, `not a resolution`, or
`unit test`; `every_allowlist_row_states_a_reason` fails on anything else. "It
was here first" is not a reason.

---

## 6. What to reconcile

* **Steps 1, 2, 4 and 5 are outside this pass's edit scope** (`vm/src/native/`,
  `vm/src/vm/`, `vm/src/jit/`, `native-builtins/`). The required edits are
  specified per-row in `guard.rs::ALLOWED` with file and reason.
* **`GLOBAL_VTABLE_MANAGER` is still unsound for two VMs.** The detector is in;
  the fix needs a VM parameter on `cratonvm_classloading::VtableInstallHook`,
  which per-vm-state.md §5.3 already records as a cross-owner request.
* **`SharedResolutionState::global_methods` / `global_fields`** use an
  accessor-free `ResolutionKey`. They have no production callers today.
  `classloading/src/access_control.rs` already warns that this key shape must
  not be adopted for anything carrying an access verdict; if `lockfree_resolve`
  is ever wired up, it must go through `MemberResolver` and gain the accessor
  in its key.
* **`access_checks_run()` is a process counter, not a per-VM one.** It is an
  audit aid, not state a decision reads, so process scope is correct — but if
  anything ever branches on it, that is the moment it has to become per-VM.
