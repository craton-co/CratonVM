# VM-internal and generated classes are deliberately mislabelled `ClassOrigin::CompatibilityStub` to avoid flipping `is_synthetic_stub`

**Status:** OPEN — JDK-only wave-2 work item, filed 2026-07-31. Not a
correctness bug in `Compatible` mode; it makes the **zero-stub acceptance
criterion unachievable by construction** and produces guaranteed false positives
in the class-origin census.

> **Evidence provenance.** `synthetic_name_origin` and its
> `// JDK-ONLY-WAVE2:` marker were read directly from
> `classloading/src/class_manager.rs` in the working tree of
> `C:\craton\cratonvm` (branch `dev`, HEAD `0c54a9184`) on 2026-07-31. Those
> uncommitted wave-1 edits were subsequently reverted out of the working tree —
> see the *Wave-1 revert* note in [`README.md`](README.md). The two classes it
> names are verifiable independently: they are ordinary
> `ensure_synthetic_class` callers in the current tree.

## What is wrong

Every class minted through `ensure_synthetic_class` is stamped
`ClassOrigin::CompatibilityStub`, **including classes that are not compatibility
substitutions at all**. Two are named explicitly:

* `cratonvm/synthetic/AnonymousObject$N` (`vm/src/vm/vm_exec.rs`) — the
  allocation shape handed to natives that have no Java type.
* `java/lang/reflect/Proxy$Instance` (`classloading/src/proxy_gen.rs`) — this
  VM's own base type for generated proxies.

Neither stands in for a class that exists anywhere. Under contract §1 item 6
both are legitimate VM products (`ClassOrigin::VmInternal`), and §11's
acceptance criterion — *"Zero `ClassOrigin::CompatibilityStub` classes for
non-array JDK, application or dependency classes"* — is arguably already
satisfiable for them. As shipped, they are counted as stubs.

The relevant helper's doc comment says so outright:

> Always a `ClassOrigin::CompatibilityStub`, and that is deliberate: every class
> this path mints is one the pre-provenance code marked `is_synthetic_stub =
> true`, and `is_synthetic_stub` is now the exact derived mirror of
> `origin.is_compatibility_stub()`. Returning anything else here would flip that
> bool for classes ~160 existing read sites across 17 files already reason about
> — a behaviour change in `Compatible` mode, which contract §5 forbids.

The `reason` string still records *which flavour* of no-class-file class it is
(`"vm-internal: …"`, `"generated-proxy: …"`, `"no class file for … on any
classpath entry"`), so the census can distinguish them without any behaviour
change. That is the honest half of the compromise; the `origin` itself is still
wrong.

## Why it was deferred rather than fixed

This is the cleanest example in the whole feature of a *correct* deferral, and
the reasoning should survive:

`Class::is_synthetic_stub` is a `bool` with ~160 read sites across 17 files.
Contract §5 makes it a pure derived mirror of `origin.is_compatibility_stub()`
and explicitly forbids deleting it this wave (*"160 read sites across 17 files
are owned by other agents this wave; removing the field breaks all of them"*).
Re-classifying `AnonymousObject$N` or `Proxy$Instance` as `VmInternal` therefore
flips `is_synthetic_stub` from `true` to `false` for those classes, which is a
**real behaviour change in `Compatible` mode** at every one of those 160 read
sites — and `Compatible` mode must be byte-for-byte unchanged.

Two of those read sites are load-bearing for dispatch and are visible elsewhere
in this directory: both real-protected-stub predicates
(`vm/src/vm/vm_exec.rs` and `invoke.rs`'s
`synthetic_stub_should_yield_to_real_bytecode`) short-circuit on
`if cls.is_synthetic_stub { None }`. Flipping the bool changes which natives
yield to bytecode.

Wave 1 could not run the regression suite, so it could not make that change.

## The migration (mechanical, from the in-code marker)

> The migration is mechanical: point each caller at
> `ensure_generated_class(name, n, <origin>)`, which already bypasses this
> function entirely. Known callers to migrate:
>   * `vm/src/vm/vm_exec.rs` — `cratonvm/synthetic/AnonymousObject$N` (VmInternal)
>   * `classloading/src/proxy_gen.rs` — `java/lang/reflect/Proxy$Instance`
>     (VmInternal; no JDK class carries that name)

Note "known callers" — the list is what wave 1 could identify, not a proof of
completeness. `proxy_gen.rs` has 5 `ensure_synthetic_class` sites in the current
tree, not 1; each needs its own adjudication.

There is a second, independent flavour already handled correctly on the
*defined-from-bytes* path and worth reusing: `generated_class_origin_for_name`
recognises `$$Lambda` → `GeneratedLambda`, `$ProxyN` → `GeneratedProxy`, and
`GeneratedMethodAccessor` / `GeneratedConstructorAccessor` /
`GeneratedSerializationConstructorAccessor` → `ReflectionAccessor`. When such a
name arrives through `ensure_synthetic_class` instead, the current code still
stamps `CompatibilityStub` and merely mentions the real flavour in the `reason`.
Those are the same migration.

## What specifically must change

1. Re-land `ensure_generated_class` (reverted with the rest of wave 1's
   `class_manager.rs` edits).
2. Migrate `AnonymousObject$N` and `Proxy$Instance` to
   `ensure_generated_class(name, n, ClassOrigin::VmInternal)`.
3. Audit the remaining `proxy_gen.rs` sites and any `ensure_synthetic_class`
   caller whose name matches `generated_class_origin_for_name`.
4. Run the full regression suite specifically looking for behaviour changes at
   the `is_synthetic_stub` read sites — this is the step wave 1 could not do and
   is the entire reason the work was deferred.
5. Only after that: delete `is_synthetic_stub` and let `origin` be the single
   source of truth (contract §5: *"Convert it to a pure derived mirror now,
   delete it in a later wave."*).

## How to verify a fix

* `--dump-class-origins` must show `vm-internal` (not `compatibility-stub`) for
  `cratonvm/synthetic/AnonymousObject$N` and `java/lang/reflect/Proxy$Instance`,
  and `generated-proxy` / `generated-lambda` / `reflection-accessor` for the
  name-recognised flavours.
* The `--jdk-only-report` `counts.compatibility_classes` must drop by exactly
  the number of classes reclassified — if it drops by more, something else was
  swept up.
* **The dispatch check that actually matters:** before and after, dump the set
  of `(class, method)` pairs for which
  `synthetic_stub_should_yield_to_real_bytecode` returns `true`. It must be
  identical. If it is not, a reclassification changed native-vs-bytecode
  dispatch, which is a `Compatible`-mode behaviour change.
* Full regression suite green, and `native-builtins/tests/stub_ratchet.rs`
  unchanged at `BASELINE_SYNTHETIC_STUBS = 157` (this change touches class
  origins, not native registrations — if the ratchet moves, something is wrong).

## Blast radius if done wrong

* Flipping `is_synthetic_stub` for a class that some read site uses as a
  "trust this receiver's layout" proxy silently changes which code path runs for
  that class. The two real-protected-stub predicates are known instances; there
  are ~158 other read sites that have not been individually reviewed.
* Reclassifying too eagerly hides a genuine compatibility substitution behind a
  legitimate-looking origin, which makes the zero-stub gate report green while
  the substitution continues. That is the same failure mode as the
  `ensure_generated_class` misuse described in
  [`ensure_synthetic_class` cannot enforce](ensure-synthetic-class-cannot-enforce-only-record.md),
  and it is only guarded by a `debug_assert!`.
