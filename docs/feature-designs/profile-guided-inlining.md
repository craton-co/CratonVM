# Profile-guided inlining

**Status:** Shipped (opt-in via `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE`) —
and in practice **doubly gated**, because the evidence it needs is also
default-off.

## What it does today

Interpreted `invokestatic` and `invokespecial` feed
`MethodProfile::call_sites`, recorded from four live sites in
`vm/src/runtime/interpreter/dispatch_static.rs`,
`.../invoke.rs` and `.../dispatch_virtual.rs`.

A monomorphic **or bimorphic** `invokevirtual` / `invokeinterface` site is
speculatively inlined behind receiver class-id guards. Each guard carries the
body *that guard's class dispatches to*, and the miss edge falls through to
normal dispatch — **never a deopt**. The policy is `plan_inline` /
`classify_receiver_shape` / `InlineRequest` / `InlineVerdict` in
`jit/src/lib.rs`; emission is in `jit/src/lib.rs` and
`jit/src/x64/bytecode_walk.rs`. It fails closed: without a
`SpeculatedReceiver` invalidation dependency from `class_id_name_resolver`, it
refuses to inline.

## The two gates

| Gate | Default | Effect when off |
|---|---|---|
| `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE` | **off** | no guarded virtual inlining |
| `CRATONVM_TIER_PGO` | **off** | profile recording never enabled (`vm/src/vm/vm_init.rs`), so every `record_*` short-circuits and there is no evidence to speculate on |

Both must be set for this feature to do anything. The first is documented as
unsoaked; the second is what makes the first inert by default.

## What is not built yet

- **`jit/src/pgo.rs` is abandoned.** Roughly 2,000 lines that nothing
  constructs, populates or reads; `jit/src/lib.rs` declares `pub mod pgo;` and
  never uses it. The live profile store is `jit/src/profile.rs`. Do not extend
  `pgo.rs` — it is not the module in the loop.
- A set of related inlining knobs remain default-off experiments:
  `CRATONVM_JIT_MAIN_INLINE`, `CRATONVM_INLINE_ALLOW_STATIC`,
  `CRATONVM_JIT_INLINE_GETFIELD`, `CRATONVM_JIT_INLINE_SELF_GUARD`,
  `CRATONVM_JIT_ENABLE_INLINE_NEW`.

## 1. What existed before either lane

`try_compile_inner`'s invoke loop asked exactly one question per candidate:

```rust
inline_site_expansion_cost_tiered(&site, site_hot)
    .filter(|cost| *cost <= inline_budget_remaining)
```

That is the HotSpot three-tier size model (`MaxTrivialSize` 6 / `MaxInlineSize`
35 / `FreqInlineSize` 325, with expansion ceilings 64 / 512 and a per-method
budget of 750 cold / 2000 hot). It is a good size model. It was also the whole
policy: no depth accounting, no recursion accounting, no receiver-type profile,
no record of why a candidate was refused, and no dependency beyond the callee's
own class name. The candidate set was `invokestatic` and `invokespecial`
only — virtual and interface sites were never considered.

Separately, `MethodProfile::record_call_site` (`jit/src/profile.rs`) existed
with **zero callers anywhere in `vm/` or `jit/`** — `CallSiteEvidence::Direct`
was unreachable outside tests. The inlining policy was reading a data source a
real run never populated for a whole class of call sites.

## 2. The policy

`plan_inline(&InlineRequest) -> InlinePlan` decides one site. It allocates no
code and mutates nothing; it takes evidence the caller already has, plus two
resolvers it may call, and returns a verdict, a price, a dependency list, and —
for a speculative verdict — **the bodies to splice**.

### Budgets and limits

| Knob | Value | Source |
|---|---|---|
| `MAX_TRIVIAL_INLINE_SIZE` | 6 | pre-existing (`MaxTrivialSize`) |
| `MAX_INLINE_SIZE_COLD` | 35 | pre-existing (`MaxInlineSize`) |
| `MAX_INLINE_BYTECODE_SIZE` | 325 | pre-existing (`FreqInlineSize`) |
| `MAX_INLINE_EXPANSION_COST` / `_HOT` | 64 / 512 | pre-existing |
| `MAX_INLINE_BUDGET` / `_HOT` | 750 / 2000 | pre-existing |
| `INLINE_MAX_DEPTH` | 9 | `MaxInlineLevel` |
| `INLINE_MAX_RECURSIVE_DEPTH` | 1 | `MaxRecursiveInlineLevel` |
| `INLINE_MIN_SPECULATION_OBSERVATIONS` | 250 | half `INLINE_HOT_SITE_OBSERVATIONS` |
| `INLINE_MONOMORPHIC_SHARE_PCT` | 90 | |
| `INLINE_BIMORPHIC_SHARE_PCT` | 92 | |
| `INLINE_MEGAMORPHIC_TYPE_CEILING` | 8 | twice the PIC's four ways |

`INLINE_MAX_RECURSIVE_DEPTH` counts **ancestors on the inline stack**, not
sibling copies. A leaf called five times from one caller is five independent
sites, each paying the per-method budget; that is not recursion and must keep
inlining.

A **bimorphic plan is charged for both bodies** — each against its own per-site
tier, the sum against the per-method budget. Anything less would let two guards
buy twice the code for one site's price.

### Receiver-shape classification

`classify_receiver_shape` reads `profile::MethodProfile::receivers` — the map
the interpreter feeds via `ProfileStore::record_receiver_borrowed`, for
`invokevirtual` and `invokeinterface`. Ranking is descending count, ties broken
by ascending class id (`FxHashMap` iteration order is not deterministic, and
two compiles of the same profile must produce the same artifact).

- `Unprofiled` — no observations. Distinct from "many types", the same way
  `CallSiteEvidence::None` is distinct from a count of zero.
- `Cold` — under 250 observations. A site executed twice with one receiver is
  not monomorphic, it is unproven.
- `Saturated` — a counter, or the total, has pinned at `u32::MAX`. Checked
  **before** any share arithmetic and before the `Cold` test, because a
  saturated profile is unreadable at any magnitude: the site reads as
  monomorphic because its majority class *stopped counting*, not because the
  program settled. This is the pgo-02 brief's "a truncated or saturated profile
  must read as megamorphic rather than as its dominant type" requirement. It is
  reported under its own name rather than as `Megamorphic` because "unreadable"
  and "polymorphic" are different facts calling for different responses; both
  refuse.
- `Monomorphic` — top type ≥ 90%. A tail is allowed: the guard routes it to
  dispatch.
- `Bimorphic` — top two ≥ 92% combined. Higher bar because the site pays two
  guards.
- `Megamorphic` — more than 8 distinct types (a dispatch hub, refused however
  dominant its top type looks), or no sufficiently dominant type. The two are
  reported apart (`megamorphic` vs `receiver-not-dominant`).

The live profile store **cannot truncate**: `MethodProfile::record_receiver`
inserts every distinct class it sees, so a one-type reading is genuinely one
type and not a full table that overflowed. Truncation exists only in
`jit/src/pgo.rs`'s unwired sketch, which caps at 8 — see §10.

### Call-site evidence for static/special sites — PGO-01

`invokestatic`/`invokespecial` have no receiver to profile, so
`classify_receiver_shape` doesn't apply to them; `MethodProfile::call_sites`
(execution count keyed by bci, kind-agnostic) is what tells a hot static call
from a cold one inside the same method. It is fed from exactly four places in
`vm/src/runtime/interpreter/invoke.rs`:

- `execute_invokestatic` and `execute_invokestatic_cached` — every
  `invokestatic`, slow-path and cached-fast-path.
- `execute_invoke_kind` and `execute_invokevirtual_cached`, both gated on
  `is_special` — every `invokespecial`. These two functions are ALSO the
  dispatch path for `invokevirtual`/`invokeinterface`, deliberately NOT
  recorded there (already covered by `receivers`; recording both would
  double-count one call site against two evidence sources).

Recording is placed once per function, right after resolution succeeds and past
every early give-up/eviction/cache-miss return — not scattered across every one
of the many downstream successful-dispatch branches each of these functions
has. This is a heuristic hotness signal for the inliner, not a correctness
input: the placement can over-count by one in a narrow redefinition-eviction
race, an acceptable trade against hunting down every branch in a 500–2000-line
function and risking missing one. Undercounting, not overcounting, was the
failure mode found in production — the recorder had zero callers for an entire
wave.

**Trap worth keeping**: a same-class **private instance method** call does
**not** compile to `invokespecial` on a modern (JDK 11+) javac — confirmed with
`javap`, it is `invokevirtual` (JEP 181, nestmate access control). A
guaranteed-`invokespecial` Java fixture needs `new Foo()`'s `<init>` call or an
explicit `super.foo()`. The pre-11 JVMS intuition is stale.

### Verdicts

- `DirectBind` — statically bound callee (`invokestatic`/`invokespecial`). One
  possible target, no guard, no speculation. Its callee is the CONSTANT-POOL
  callee, which for these kinds is the whole point.
- `Monomorphic { guard_class_id }` — splice the body behind
  `CMP DWORD [recv+0], guard_class_id`; a miss falls through to normal
  dispatch.
- `Bimorphic { guard_class_ids }` — two guarded bodies in descending profile
  order, sharing one receiver load, one null check and one dispatch tail.
- `Refuse(InlineRefusal)` — sixteen named reasons, each with a stable
  `category()` string for metrics keys and log greps.

## 3. The callee of a guarded site is not the callee the constant pool names

This is the rule the first increment got wrong, and it is the centre of the
feature.

A guard compares the receiver's class id against a class taken from the
**profile**. The constant pool names the receiver expression's **static type**.
Those two disagree at every site where the speculated class overrides the
declared method — the single most ordinary shape in Java — and the first
increment resolved the body from the constant-pool name. So a site declared
`invokevirtual A.tag` whose receiver is always `B` (which overrides `tag`)
emitted a guard admitting `B` and spliced **`A`'s body**. No crash, no
diagnostic, wrong answer.

Reproduced, not inferred (`vm/tests/pgo02_guarded_virtual_inline.rs`,
`callOverride`): calls 0–505 return `x+1000` from the interpreter, the method
compiles at the 500-invocation threshold, and call 506 returns `507` instead of
`1506`.

The same family of bug is already written up on `try_compile_inner`'s
statically-bound direct-call arm, which was restricted to
`invokespecial`/`invokestatic` after H2 miscompiled
`VersionedValue.getCurrentValue` for exactly this reason.

**The rule.** `InlineRequest::receiver_callee_resolver` resolves the body a
receiver of exactly a given class id dispatches to, and `plan_inline` calls it
once per guard class. `InlineRequest::site` — the constant-pool callee — is
`None` for a speculative site and is never a fallback: that fallback *is* the
bug. `InlinePlan::speculative_sites` carries `(class id, body)` in guard order,
and the invoke loop splices those.

This is also what gives the lane `invokeinterface` reach at all. An interface's
own method declaration has no `Code` attribute, so resolving from the
constant-pool class finds nothing to splice, and every interface site was
refused before this.

### Fail-closed rules on the VM side

`resolve_receiver_inline_site` starts the JVMS selection walk at the runtime
receiver (`find_method_recursive` performs maximally-specific default-method
selection only when handed the receiver, which is the same reason the
interpreter's own dispatch redirects to the receiver id for interface calls).
It then refuses every shape where that walk could disagree with real dispatch:

- the receiver class must be a loaded, non-interface, non-array class, and not
  a lambda proxy (a proxy's dispatch is synthesised elsewhere);
- the selected method must not be `static`, `private` or `abstract`, and the
  name must not be `<init>`/`<clinit>` — none of those are virtually
  dispatched, and a private method is never inherited, so a walk that reached
  one from a subclass receiver found something dispatch could not;
- the receiver must be a subtype of the constant-pool class;
- and the selected method must be a genuine **override**: `public` or
  `protected` (which overrides anything it inherits, in any package), or
  declared on the constant-pool class itself (in which case constant-pool
  resolution stops there and the selected method IS the resolved method).

What that last rule refuses is a **package-private** method selected from some
class other than the constant-pool class. It may or may not override — a
package-private method in a different runtime package does not (JVMS §5.4.5:
dispatch runs the resolved method, while the walk found the impostor) — and
telling those apart needs a second resolution of the constant-pool reference.
`vm/src/runtime/resolve/guard.rs` ratchets the interpreter's metadata-table
bypass budget downward and nothing raises it, so that second resolution is not
taken. Refusing costs a package-private virtual site its inline; guessing costs
a wrong body.

No speculative inline is admitted without **all** of: a receiver class-id guard
the backend can emit
(`InlineBackendCaps::guarded_inline_body_at_virtual_sites`), a resolvable body
per guard class, and a recorded invalidation dependency naming the speculated
receiver class. Any one missing ⇒ `GuardNotEmittable`, `CalleeUnresolved` or
`NoInvalidationDependency`. All refusals, never warnings.

#### A native anywhere on the receiver-to-declaring chain refuses the splice

`NativeMethodRegistry::find` is keyed on the **exact** class name. The rule "a
registered native shadows the classfile body" was therefore asked twice -- once
about the constant-pool class, once about the class that DECLARES the selected
method -- and both answers can be `None` for a method dispatch would nonetheless
run natively.

That is not a corner case; it is how the collection carriers are built.
`native-collections` mints an ArrayList-shaped snapshot under a real JDK class
name (`java/util/TreeMap$EntryIterator`, `HashMap$KeyIterator`,
`ConcurrentHashMap$EntryIterator`, ...) and registers `hasNext`/`next`/`remove`
**on that concrete class**, with a matching row in
`force_native_over_real_jdk_bytecode`. The JDK declares `hasNext` one level up,
on `TreeMap$PrivateEntryIterator`, where nothing is registered -- so the
declaring class answered "no native" for exactly the method the whole scheme
depends on shadowing, and the spliced JDK body walked a `next` chain the carrier
never populates.

`resolve_inline_site_from` now walks receiver-to-declaring asking the registry
at each step, which is the question dispatch asks:
`populate_virtual_invoke_cache` looks the native up under the RECEIVER's name
("Check native overrides FIRST") and its `native_override_below_declaring` loop
is this same walk. Refusal name: `native-shadow-on-receiver-chain`, which
reports the class it found.

Measured: a compiled `for (e : treeMap.tailMap(k).entrySet())` iterating ZERO
entries over a six-entry view --
`internal/fixed-bugs/guarded-inline-native-screen-asked-the-declaring-class-FIXED-20260904.md`,
`org.h2.test.store.TestRandomMapOps` op:1033.
`vm/tests/jit_guarded_inline_native_shadow.rs` is the arm; it is the only test
anywhere that runs with this feature's flag ON, which is why nothing caught this
for a month. **Any new admission rule added here needs an arm that sets the
flag** -- the regression suite sets no environment, so its vectors cannot
exercise this feature at all.

### Exception-path policy

Three rules, in precedence order:

1. **Precise exception frames ⇒ nothing inlines.** Mirrors the unconditional
   `inline_sites.clear()` `try_compile_inner` already performs.
2. **A speculative site inside a protected range is refused.** The guard's miss
   edge is a new control-flow edge in the middle of a `try` block, and the
   inlined body publishes no exceptional frame of its own.
3. **A statically bound site inside a protected range is unchanged** — it
   inlines exactly as it did before this policy existed.

An **uncaught** exception raised inside a guard-hit inlined body propagates
correctly: `callDivider` in the fixture divides by an instance field, and once
that field is `0` the spliced `idiv` raises `ArithmeticException` with no
handler in the inlined frame and none in the caller. The test's control is the
same call before the method was ever compiled, so it compares the compiled path
against the interpreter rather than against a hard-coded string.

**The captured stack trace still names the callee.** The brief asked for "same
exceptions, same stack traces, same `finally` execution", and the stack-trace
half is the one a spliced body threatens: it has no frame of its own, and
`FrameState::caller` is populated by nobody, so there is nothing to rebuild a
callee frame from. Measured on the reachable shape (an implicit
`ArithmeticException` out of a spliced `idiv`): the trace names one `tag` frame
compiled and one interpreted. This is a result about that shape, not a general
proof — but it is the shape this lane can produce, and it is now pinned.

Monitors are the brief's second blocker: every `FrameState` the lowerer builds
hard-codes an empty monitor list, so a spliced body could carry no monitor
state to rebuild. The resolver refuses both shapes — a `synchronized` method
(access flag) and a `synchronized` block (`monitorenter`/`monitorexit`) — and
the test asserts the refusal CATEGORY, not merely that no splice happened,
because an unprofiled site also fails to splice and that is a different fact.

`finally` gets the same treatment: a callee with a non-empty exception table is
never spliced, and the test counts the side effect on both escape routes. The
brief calls this out for a reason — "this VM has already shipped a JIT-compiled
`finally` that was not run on three escape routes; inlining multiplies that
surface".

## 4. Deopt safety, enforced rather than argued

`deopt::FrameState::caller` exists but **no production site populates it** —
every construction in `ir_lower.rs` and `x64.rs` passes `caller: None`. Inlined
scopes are therefore not representable in deopt metadata. A deopt point
published from inside a spliced body would name the CALLER's method with the
CALLEE's bci: a well-formed description of a stack that never existed, which is
the exact failure class the 2026-08-01 deopt-metadata audit found three of.

Both PGO-01 and PGO-02 rely on the **absence** of deopt points inside inlined
bodies, not on caller scopes:

- the guard is a plain `CMP`/conditional-jump pair with no deopt stub of its
  own — a mismatch falls through to the pre-existing normal-dispatch code
  (MIC/PIC/`jit_invoke_dispatch`), never traps. That is the literal reason the
  "falls back to the existing dispatch on mismatch — not to a deopt" framing
  exists: a falling-through guard needs no frame at all, sidestepping the
  caller-scope gap rather than requiring it to be fixed first;
- and `x64::try_emit_inline` — the shared splice machinery both the DirectBind
  path and the guarded-virtual path call — **refuses a splice whose body
  published any deopt metadata**. It snapshots `deopt_stubs` and `deopt_points`
  around the body and rolls the whole attempt back if either grew.

That last point used to be a claim about the source ("the emitter contains no
`build_and_record_deopt_point` on this path"). A claim like that is one edit
away from being false, and nothing would fail when it became false. It is now a
postcondition, and it is tested by **injecting the violation**
(`INLINE_TEST_PUBLISHES_DEOPT`), because no production path reaches that state
and a check nobody can make fire is a check nobody has tested.

Consequence, stated plainly: an inlined body is entered and left within one
frame described as the caller's own, and no deopt point inside it is described
at all. There is no metadata a missing caller scope could make wrong.

**Hard precondition for any future work.** A guard that *traps* rather than
falling through — the "only after that, and only if the metadata is shown
total" step the original brief deferred — needs `FrameState::caller` populated
by the producer first. Until then the postcondition above refuses it
automatically rather than trusting anyone to remember.

## 5. Dependencies and how invalidation reaches the code

`InlinePlan::invalidation_triples()` flattens to `(class, method, descriptor)`
triples, written into `CompiledMethod::inlined_methods` (deduplicated).

Two dependency kinds:

- `InlinedCallee` — the spliced body, one per body actually spliced (two for a
  bimorphic plan). It names the class that **owns the body**, which for a
  receiver-resolved site is the declaring class, not the declared supertype
  that may own no body at all.
- `SpeculatedReceiver` — the class a guarded site speculated on, one per guard.

Reach, in the VM: class define / redefine / unload funnel into
`JitCache::invalidate_matching` via `vm/src/vm/vm_init.rs`'s `load_class` and
the class-manager's redefinition/unload paths.

### The reach that was documented, and the reach that existed

A previous revision of this section recorded a "known coarseness — asserted,
not assumed": the VM walked the loaded class and its **direct** superclass, so
a dependency on `A` was not reached when `C extends B extends A` was loaded.

The direct-superclass half did not work either. `load_class` passed
`c.superclass.map(|s| s.to_string())` to the name-keyed scans; `superclass` is
a `ClassId` whose `Display` prints the raw `u32`, so the "superclass" being
matched against `inlined_methods` was a **decimal number** and matched nothing.
That half had never run.

Nothing failed when it broke, which is why it survived: the guarded and
devirtualised code stays **correct** without the eviction — an exact class-id
guard rechecks the receiver, and a MIC/PIC re-targets — so the only symptom was
code that should have been retired staying resident, paying a guard that now
always misses. A capability that reads as landed but never runs is precisely
what `audits/flag-census.md` tracks.

Both are fixed: `load_class` now walks the **full supertype closure**
(superclasses and interfaces) by name. The closure is bounded by hierarchy
depth and computed once per class *define*, not per call.

### The second channel

`deopt::InvalidationManager` models this properly —
`CompilationAssumption::StableType { bci, expected_class }` is exactly a
guarded receiver speculation. It now has a reverse index and a query:
`on_class_loaded_with_supertypes(class_id, supertypes)` returns every method
holding a `StableType` assumption on the new class **or on any ancestor**,
which is the direction that matters (a speculation on `A` is threatened by a
new *descendant* of `A`, and a class-load event knows only the id that just
arrived). Asking the caller for the closure keeps the hierarchy walk where the
class metadata lives, instead of building a second parent map in the manager
that could disagree with the real one.

Nothing registers `StableType` assumptions from `try_compile_inner` yet — the
manager lives behind `vm/src/vm/realms/jit_realm.rs`'s mutex and is not
threaded into the compiler. That was worth doing when the name-keyed channel
was coarse; now that it walks the full closure, the second channel is available
rather than needed. See §10.

## 6. What the backend emits

**`invokestatic`/`invokespecial` DirectBind splicing** existed before either
lane (`x64.rs`'s `invokestatic` and `invokespecial`-only arms consult
`inline_sites`).

**Guarded virtual/interface splicing** lives in
`jit/src/x64/bytecode_walk.rs`'s `0xb6 | 0xb9` arm, between the pre-existing
invokespecial-only inline check and the intrinsic-ladder/direct-call/
dispatch-helper block:

1. `self.inline_guard_variants[pc]` holds `(receiver class id, body)` in guard
   order — one entry for a Monomorphic verdict, two for a Bimorphic one.
   Element `[0]` is also the `inline_sites` entry for that pc.
2. The receiver (deepest of `callee_num_args` operands) is loaded and
   null-checked **once**, ahead of the chain: `null` fails every guard, and
   re-testing it per variant would be pure code size. Peeked, not popped — each
   splice does its own popping, and the miss tail needs the receiver+args
   untouched for the dispatch code that runs next.
3. Per variant: `CMP DWORD [reg+0], guard_class_id` (the identical encoding the
   pre-existing String/CRC32 intrinsic guards use) then `JNE`, then
   `try_emit_inline_site(pc, body)`. A hit emits a `JMP` past every later guard
   **and** the dispatch tail. A miss lands at the next variant's `CMP`, or —
   for the last variant, and for the shared null check — at the exact start of
   the unmodified normal-dispatch code.
4. After each successful splice the compiler's **symbolic** state (operand
   stack, oop marks, spill cursor) is restored to its pre-chain value; the
   emitted bytes are kept. So the next variant sees the same operand stack this
   one did, and the dispatch code — the only Rust-level continuation, run
   unconditionally — pops the same receiver+args positions and pushes a
   canonically-shaped result regardless of which machine-code path a given
   execution actually takes at runtime. Both paths converge on the same
   `push_from_rax` / `push_from_rax_as_xmm0` convention by construction, which
   is what makes this reconciliation sound rather than merely convenient.
5. A variant whose body cannot be spliced rewinds **only its own bytes** (the
   same seven deferred-patch-list checkpoints `try_emit_inline` itself uses);
   the earlier variants stand, and the previous miss edge — already patched to
   that offset — resolves to whatever is emitted there next, which is the
   correct landing spot either way. If no variant splices, the shared receiver
   load and null check rewind too and the site is byte-identical to never
   having attempted a guard.

The buffer estimate (`inline_extra`) and the frame spill reservation
(`inline_stack_reserve`) in `x64::driver` count the **second body** as well.
Both were derived from `inline_sites`, which holds only the primary; an
uncounted second body would overflow a buffer this backend cannot retry, and
write past the spill region into the callee-saved area.

`InlineBackendCaps::single_pass_x64()`'s
`guarded_inline_body_at_virtual_sites` is `class_id_name_resolver.is_some()` at
the `plan_inline` call site, and that resolver is `Some` only when
`CRATONVM_JIT_GUARDED_VIRTUAL_INLINE` is set. The flag is the only thing gating
the feature; with it off, virtual/interface sites skip the whole admission
block, which is byte-for-byte the pre-PGO-02 code path.

> That gating is load-bearing for a reason worth remembering. An earlier
> revision admitted `0 | 2` unconditionally and let `plan_inline` refuse them
> on caps. But `plan_inline` calls `classify_receiver_shape` *before* it
> consults the caps, so flag-off callers paid for and ran that admission
> machinery on every virtual site in every compiled method — bisected to a
> javac-self-hosting `AssertionError` with nothing to do with this feature.

## 7. Verification

`vm/tests/pgo02_guarded_virtual_inline.rs` +
`vm/tests/resources/cratonvm/PgoGuardedVirtualInline.java`, driven by REPEATED
INVOCATIONS from the test (not an internal Java loop — invocation-count tiering
is the code path this lane touches; an internal loop would need OSR, a
different, untouched compile path).

**One `Vm` for every check.** The file used to build a fresh `Vm` per check,
and only the FIRST one ever compiled anything — the tiered background compile
worker is process-global and did not warm up again — so every check after the
first ran fully interpreted, *including the guard-MISS check the file calls its
most safety-critical one*. They asserted correct results, got them from the
interpreter, and proved nothing about the compiled path. Found with
`CRATONVM_DBG_JITC=1`: exactly one `bg-compile` line in the whole run. Every
check that depends on a compiled artifact now asserts that artifact exists, so
the same silent regression to "correct, but interpreted" fails loudly.

**And it waits for the artifact rather than racing it.** Compilation is
asynchronous: crossing the invocation threshold *enqueues* the method and a
background worker installs it later. A release build runs the 700-call warm-up
in ~80 ms, routinely faster than the worker, so a single cache read reported
"never JIT-compiled" on Linux/release while passing on Windows/debug, where the
interpreter is slow enough that the worker always won. `compiled_tally` keeps
calling the method while it waits — which gives the worker both the trigger and
the time — and still fails if the artifact never appears, because "eventually
compiles" is the claim under test.

| Check | What it pins |
|---|---|
| `check_guard_hit` | monomorphic-A site: correct results past the threshold AND `speculative_sites >= 1`, so a correct result cannot come from plain dispatch and pass |
| `check_guard_miss` | builds an A-guarded compile, asserts the guard exists, then switches the receiver to three OTHER concrete classes without recompiling — every call must reach the ACTUAL runtime class |
| `check_override_receiver` | §3: constant-pool class `A`, receiver always `B`, `B` overrides. The regression case, with a compiled-path assertion |
| `check_interface_site` | `invokeinterface` with one implementation: correct AND spliced, so a regression to "correct but never inlined" is visible |
| `check_bimorphic` | two overriding classes evenly mixed; asserts ≥ 12 spliced callee bytecodes, because a one-guard lowering also produces correct answers (the second class just dispatches) |
| `check_uncaught_from_inlined_frame` | `ArithmeticException` out of a guard-hit inlined frame, compared against the same call before the method compiled |
| `check_stack_trace_through_an_inlined_frame` | the brief's "same stack traces" requirement: the CAPTURED trace of an exception raised inside a guard-hit spliced body names the callee frame exactly as the interpreted one does (measured 1 vs 1, with a floor so two zeros cannot agree vacuously) |
| `check_monitor_bearing_callees_are_refused` | the brief's second blocker: a `synchronized` method AND a `synchronized` block are both refused, and the refusal category is asserted so "refused for an unrelated reason" cannot pass |
| `check_finally_runs_at_a_guard_eligible_site` | the brief's `finally` requirement: a `finally`-bearing callee is never spliced, and the `finally` runs exactly once per call on BOTH escape routes |
| `check_thrower` | exception + catch control flow through a compiled, guard-eligible site |
| `check_polymorphic` | 4 types, none dominant: must refuse and must never mis-dispatch |
| `check_metrics_harvest` | the tally reaches `metrics::compilation_reports()` |

The tier dependency is **stated, not relied on**: `inline_tally` is a
single-pass artifact's record, and an IR artifact leaves it zeroed, so
`used_ir_backend` is asserted first — otherwise "the guard did not fire" and "a
different backend compiled the method" are indistinguishable, and they are
opposite conclusions. The file pins the tier with
`CRATONVM_JIT_IR_CALL_VIRTUAL=0`.

Policy-level tests live in `jit/src/lib.rs`'s `profile_guided_inlining_tests` —
the refusal ordering, the budget arithmetic, the two-guard dependency set, the
receiver-body-not-declared-body rule, the no-fallback rule, and the
saturated-profile refusal.
`jit/src/x64/tests.rs::inline_publishing_a_deopt_point_is_refused` is the
injected-violation test for §4.

## 8. Metrics

`CompiledMethod::inline_tally` (an `InlineDecisionTally`) is published on every
artifact and carries `candidates` / `inlined_sites` / `speculative_sites` /
`inlined_bytecodes` / `expansion_cost` / `observed_calls_inlined` / `refusals`.
`metrics::CompileRecorder::installed` harvests all of it from the artifact —
which costs nothing on the ~40 `return None` paths through `try_compile_inner`
and cannot go stale, because it is the installed body's own record — and
`CompilationReport::to_json` emits it, refusal histogram included. Enabled by
`CRATONVM_JIT_METRICS=1`; `CRATONVM_JIT_METRICS_OUT=<path>` writes one JSON
object per compilation.

A previous revision of this document recorded the harvest as not done. It was
done; the claim had only ever been checked by reading `metrics.rs`, and a
harvest that runs on no real compile is indistinguishable from one that does
not exist. `check_metrics_harvest` now asserts it against the reports a real
guarded compile publishes.

`candidates` means "sites the inliner considered", not "sites a resolver
answered for": a site past the point of budget exhaustion is tallied
(`budget-already-spent`) instead of vanishing, and so is a constant-pool callee
the resolver declined (`callee-unresolved`). Without that the histogram had no
denominator, and the most interesting measurement of all — how many hot virtual
sites are actually monomorphic — could not be read off it.

## 9. Reach: which methods can this feature apply to at all?

Guarded inlining lives in the single-pass x64 emitter. The optimizing (IR) tier
serves a virtual site from a MIC/PIC cascade, plans no inline and records no
tally, so **every method the optimizing tier accepts leaves this feature's
reach entirely** — and the `cov-*` lanes exist to make the optimizing tier
accept more methods. That makes "is guarded inlining worth extending?" a
question about population, not about the lowering.

`regression-suite/perf/guarded-inline-reach.sh` answers it for any workload in
one run: installed bodies, the single-pass/optimizing split, how many
single-pass bodies the inliner was asked about, how many got a splice, and the
refusal histogram for the rest. Every number is a count, so a loaded host does
not affect it, and no conclusion about speed can be drawn from it.

### The feature needs TWO opt-ins, and the second one is the profile

`CRATONVM_JIT_GUARDED_VIRTUAL_INLINE` buys the lowering. The *evidence* is a
separate gate: `SharedVm::new` calls `jit::profile::enable_profiling(true)`
only under `CRATONVM_TIER_PGO`, so with that flag unset every
`record_receiver` short-circuits, `classify_receiver_shape` sees
`Unprofiled`, and **every virtual site is refused `no-profile-evidence`**.
Measured, not assumed — on `regression-suite/perf/GuardedInlineReachProbe.java`
with only the inlining flag set: 15 `no-profile-evidence` refusals and zero
splices. The harness sets both flags for this reason, and anyone measuring this
by hand must too, or they will conclude the lowering does not work.

### The measurement

`GuardedInlineReachProbe` (ordinary virtual and interface dispatch: a
single-implementation interface site, a two-class overriding site, a
three-implementation site, `java.util` calls through `List`/`Map`, and
`StringBuilder`), both flags on, one run:

```
  installed bodies             18
  single-pass                   5   27.8%   <- the only population this feature can reach
  optimizing (IR)              13   72.2%   <- out of reach; the IR tier plans no inline
  single-pass w/ a splice       0    0.0% of single-pass
  refusals: callee-unresolved 17, receiver-not-dominant 1
```

**Nearly three quarters of the installed bodies are out of reach**, and the
answer is sharper than "the tiers split the population": the methods this
feature targets are exactly the ones that get *promoted*. `CRATONVM_DBG_JITC=1`
shows `monomorphicInterface`, `bimorphicVirtual`, `collectionStep` and friends
each compiled three times — C1 first, then C2, whose artifact supersedes it —
and their virtual sites reported by the IR path as
`ir-direct-call MISSED …$Op.apply`. The single-pass artifact carrying the guard
is the one that gets replaced.

So in a warm run, guarded inlining's population is *the methods the optimizing
tier refuses*, and it shrinks every time a `cov-*` lane lands. That is the
answer to "is this worth extending?": extending the SINGLE-PASS lowering is
work with a shrinking denominator. The version worth building, if any, is a
guarded-inline lowering on the IR path — and item 2 of §10 is where that would
be decided, with this instrument as the evidence.

`cov-01` demonstrated the same dependency by accident, before there was an
instrument for it.

`getstatic <A>; invokevirtual tag` — the single most ordinary virtual-call
shape there is, and the exact shape of every entry point in the fixture — was
refused by the IR builder only because it had no `0xb2` arm. The moment
`getstatic` lowered, `callA` was compiled by C2 on its next promotion, its
`inline_tally` was empty, and the guard-hit check failed with
`speculative_sites == 0`. The test now pins the tier with
`CRATONVM_JIT_IR_CALL_VIRTUAL=0`, so the dependency is stated rather than
relied on.

## 10. Still open

1. **A guard that deoptimizes instead of falling through.** Blocked on
   `deopt::FrameState::caller` being populated by a producer — a deopt-metadata
   lane, not an inlining one. §4's postcondition refuses the shape
   automatically until then, so this is a capability gap, not a risk.
2. **The IR tier has no guarded-inline lowering.** §9 is the instrument for
   deciding whether to build one; the answer depends on how much of a real
   workload the optimizing tier ends up accepting.
3. **`CRATONVM_JIT_GUARDED_VIRTUAL_INLINE` is default-OFF and — measured
   2026-08-17 — it MISCOMPILES with both gates on. Do not enable it.**
   This item used to say "turning it on is a soak decision with a measurement
   behind it, not a code change". The measurement has now been run and it is
   negative: there IS a code change to make first. See §11.
4. **Nothing registers `StableType` assumptions from the compiler.** The
   channel is wired and tested on the manager side (§5); using it needs
   `InvalidationManager` threaded out from behind `jit_realm`'s mutex. The
   name-keyed channel now has the reach that motivated this, so it would buy
   precision, not correctness.
5. **The stack-trace equivalence result covers ONE shape.** An implicit
   `ArithmeticException` out of a spliced `idiv` reconstructs the callee frame;
   a different escaping shape has not been measured, and there is no mechanism
   guaranteeing it — the guarantee would be `FrameState::caller`, i.e. item 1.
6. **A package-private method selected from a class other than the
   constant-pool class is refused** (§3). Deciding it properly needs a second
   constant-pool resolution, which the metadata-bypass ratchet in
   `vm/src/runtime/resolve/guard.rs` does not permit; a `MemberResolver`-based
   answer would.
7. **Loop-unrolled copies of a guarded site fall back to dispatch.** The guard
   map is deliberately not replicated across the loop-unroll pc rewrite —
   always correct, just not optimized.
8. **`jit/src/pgo.rs` is still unwired.** It no longer carries a rival policy:
   `ReceiverTypeProfile::shape` is a view onto `classify_receiver_shape`, with
   truncation layered on top because the live profile store has no notion of
   it. Give it a recorder before reading anything from it — every counter in it
   is permanently zero at runtime.

## 11. The soak measurement §10.3 asked for — run 2026-08-17, and it is negative

§10.3 said turning the flag on was "a soak decision with a measurement behind
it, not a code change". The measurement has been run. **With both gates set,
guarded virtual inlining miscompiles.** It is not a soak-and-ship; there is a
correctness defect to find first.

Workload `io.netty.buffer.BigEndianHeapByteBufTest` (414 test methods, the
`io.netty` suite's densest virtual-dispatch class), Azure Linux
`20.80.105.49`, one binary, arms ABBA-interleaved because that box runs at
load 8-16 and its wall clock is not trustworthy to better than ~20%.

Because both gates are independent, all four cells were measured. The defect
needs BOTH, which is the whole reason it has never been seen: the flag alone is
inert, so "flag on, suite green" was true and meaningless.

| `CRATONVM_TIER_PGO` | `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE` | ok | failed |
|---|---|---:|---:|
| off | off | 414 | 0 |
| off | **on** | 414 | 0 |
| **on** | off | 414 | 0 |
| **on** | **on** | **165** | **249** |

Confirmed on two binaries a fortnight of dev apart (`377aadd08` and
`95ecf08fb`), 249-250 failures every run:

| arm | ok | failed | `jit_entries` | wall | `NoSuchMethodError` lines |
|---|---:|---:|---:|---:|---:|
| A baseline | 414 | 0 | 116,169,136 | 71 s | 0 |
| C `TIER_PGO` only | 414 | 0 | 115,622,562 | 74 s | 0 |
| B both | 165 | 249 | 7,996,311 | 33 s | 1482 |
| B both (repeat) | 166 | 248 | 7,631,894 | 34 s | 1482 |
| C `TIER_PGO` only | 414 | 0 | 114,079,183 | 74 s | 0 |
| A baseline | 414 | 0 | 116,363,268 | 66 s | 0 |

**`TIER_PGO` on its own is clean and free** — 414/414 in both its arms, and
`jit_entries` within noise of baseline. So profile *recording* is not implicated
and the attribution is to the guarded-inline codegen alone, with the control run
in the same interleave rather than inferred.

### Do not read arm B's numbers as a speedup

Arm B is 2x faster with 14x fewer `jit_entries`. **Both are artifacts of 249
tests failing early instead of doing their work** — exactly the confounded-arm
trap the netty per-call throughput record (netty-per-call-throughput-20260813,
§3.2) documents, where a 17-passing arm was read as a 7x win over a 126-passing
one. An arm that does not do the same work is not a measurement of doing it
faster. Any future A/B here must gate on `failed=0` before comparing anything.

### The signature

Every failure is one method, 1482 stderr lines of it:

```
java.lang.NoSuchMethodError: 'long org.junit.jupiter.engine.extension
    .TimeoutInvocationFactory$TimeoutInvocationParameters.getValue()'
  org.junit.jupiter.engine.extension.SameThreadTimeoutInvocation.proceed(...:45)
  org.junit.jupiter.engine.extension.TimeoutExtension.intercept(...:161)
```

A `NoSuchMethodError` raised out of compiled code, for a method the interpreted
arm resolves without complaint, is the shape of a spliced body resolved against
the wrong class — which is precisely the hazard
`InlineRefusal::CalleeUnresolved`'s own doc comment describes for the speculated
case: "the guard would admit exactly that class, so the body behind it must be
the one that class dispatches to, and if the VM cannot hand back that
body — or cannot prove the body it found is the one real dispatch would
select — there is nothing safe to splice." The refusal exists; something is
getting past it, or the guard and the body it carries disagree. §3's
package-private caveat (§10.6) and the bimorphic second guard are the two
places to look first.

`getValue()` returning `long` is worth noting: `TimeoutInvocationParameters` is
a small carrier type, so this is a tiny accessor on a hot interception path —
the most-inlined shape there is.

### Repro

```bash
cd apps/netty-suite-runner
CLS=io.netty.buffer.BigEndianHeapByteBufTest
# clean (control) — either gate alone
CRATONVM_TIER_PGO=1 <cv-bin> --java-home <jdk25> --Xmx 1500m \
    @common.args -Dcraton.batch=1 CratonRunner $CLS
# 249 failures
CRATONVM_TIER_PGO=1 CRATONVM_JIT_GUARDED_VIRTUAL_INLINE=1 <cv-bin> \
    --java-home <jdk25> --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner $CLS
```

### Why this was being measured at all

Not to soak the flag. The `io.netty` suite has a 20-class HANG cluster that is
entirely wall-clock — `Bzip2IntegrationTest` is 21 s on HotSpot and 1964 s here,
and every one of those classes passes when given room. The cause is ~826 M
non-inlined calls at a few hundred ns each, and the ByteBuf chain that pays it
(`AbstractByteBuf.writeByte` -> `ensureWritable0` -> `_setByte` ->
`HeapByteBufUtil`) is *entirely virtual*, which `InlineRefusal::GuardNotEmittable`
records the single-pass backend as unable to inline at all. This feature is the
one built mechanism that would change that, so it was the obvious lever to
price. It cannot be priced until it is correct — and its correctness, not its
throughput, is what this section is about.
