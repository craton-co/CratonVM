# VM-internal and generated classes are deliberately mislabelled `ClassOrigin::CompatibilityStub` to avoid flipping `is_synthetic_stub`

**Status:** OPEN — JDK-only wave-2 work item, filed 2026-07-31, re-verified
against the re-landed tree the same day. Not a correctness bug in `Compatible`
mode; it makes the **zero-stub acceptance criterion unachievable by
construction** and produces guaranteed false positives in the class-origin
census.

> **Evidence provenance.** The original filing quoted a
> `synthetic_name_origin` helper that the re-land removed. The equivalent
> `JDK-ONLY-WAVE2` marker now sits on `fabricate_class` in
> `classloading/src/class_manager.rs` (~3002) and is quoted below from the
> re-landed tree, read 2026-07-31. One detail changed with it: the marker now
> prescribes **`GeneratedProxy`** for `Proxy$Instance`, not `VmInternal` as the
> original record said.

## What is wrong

Every class minted through `ensure_synthetic_class` is stamped
`ClassOrigin::CompatibilityStub` — the wrapper hard-codes
`ClassOrigin::compatibility_stub(ENSURE_SYNTHETIC_STUB_REASON)` — **including
classes that are not compatibility substitutions at all**. The marker on
`fabricate_class` names two:

> every class minted here is labelled with whatever `origin` the caller passed,
> and `ensure_synthetic_class` — still the overwhelming majority of the traffic
> — passes `CompatibilityStub`. That over-reports: two of its callers are not
> compatibility substitutions at all.
>   * `cratonvm/synthetic/AnonymousObject$N` — the untyped allocation shape
>     behind every `HashMap`/`LinkedHashMap` node and friends
>     (`alloc_concurrent_synthetic` in `native-builtins`). It is
>     `ClassOrigin::VmInternal`: a VM bookkeeping type that never had, and never
>     will have, a class file.
>   * `java/lang/reflect/Proxy$Instance` — the synthetic supertype of every
>     generated `$ProxyN` (`proxy_gen` / the `Proxy` natives). It is a
>     generation artefact, not a stand-in for absent bytes.

`AnonymousObject$N` is minted in `vm/src/vm/vm_exec.rs` (~9042,
`format!("cratonvm/synthetic/AnonymousObject${num_fields}")`);
`Proxy$Instance` is `proxy_gen.rs`'s declared super class (~1660) and is
`ensure_synthetic_class`ed at ~14998 in `class_manager.rs`'s tests and by the
`Proxy` natives.

Neither stands in for a class that exists anywhere. Under contract §1 item 6
both are legitimate VM products, and §11's acceptance criterion — *"Zero
`ClassOrigin::CompatibilityStub` classes for non-array JDK, application or
dependency classes"* — is arguably already satisfiable for them. As shipped,
they are counted as stubs.

The marker states the reason for the deferral outright:

> Classifying either honestly today would flip the derived `is_synthetic_stub`
> bool from `true` to `false` for classes that ~160 read sites already reason
> about — a *Compatible-mode* behaviour change, which contract §10 forbids in
> wave 1. So the flavour is kept in the `reason` string and the fix is deferred.

The `reason` string still records *which flavour* of no-class-file class it is,
so the census can distinguish them without any behaviour change. That is the
honest half of the compromise; the `origin` itself is still wrong.

## Why it was deferred rather than fixed

This is the cleanest example in the whole feature of a *correct* deferral, and
the reasoning should survive:

`Class::is_synthetic_stub` (`classloading/src/class.rs` ~363) is a `bool`
alongside the authoritative `pub origin: ClassOrigin` (~345), kept in sync by
`Class::set_origin` (~506). Contract §5 makes it a pure derived mirror of
`origin.is_compatibility_stub()` and explicitly forbids deleting it this wave.
Re-classifying `AnonymousObject$N` or `Proxy$Instance` therefore flips
`is_synthetic_stub` from `true` to `false` for those classes, which is a **real
behaviour change in `Compatible` mode** at every read site — and `Compatible`
mode must be byte-for-byte unchanged.

**The read-site count is larger than the contract's estimate.**
`is_synthetic_stub` appears **181 times across 20 files** (ripgrep,
2026-07-31), against the contract's "~160 across 17 files". Ten of those are in
`vm/src/vm/vm_exec.rs` alone. Treat 181/20 as the working figure.

Two of those read sites are load-bearing for dispatch and are visible elsewhere
in this directory: both real-protected-stub predicates
(`vm/src/vm/vm_exec.rs`'s inline copy at ~13600 and `invoke.rs`'s
`synthetic_stub_kind_should_yield_to_real_bytecode` at ~10569) short-circuit on
`if cls.is_synthetic_stub { None }`. Flipping the bool changes which natives
yield to bytecode.

Wave 1 could not run the regression suite, so it could not make that change.

## The migration (mechanical, from the in-code marker)

> point both callers at `ensure_generated_class` with `VmInternal` /
> `GeneratedProxy` respectively, in the same wave that re-audits the
> `is_synthetic_stub` readers.

Note the change from the original record: `Proxy$Instance` should become
**`GeneratedProxy`**, not `VmInternal`. `ClassOrigin::GeneratedProxy` carries
`interfaces: Arc<[ClassId]>` (contract §5), which `Proxy$Instance` — the shared
*supertype*, not a concrete proxy — does not have a meaningful value for. That
is an open design question the migration has to answer, not a detail to resolve
by picking whichever variant compiles.

There is a second, independent flavour already handled correctly on the
*defined-from-bytes* path and worth reusing: `class_manager.rs`'s
`generated_class_origin_for_name` (~2545–2565) recognises `$$Lambda` →
`GeneratedLambda`, `$ProxyN` → `GeneratedProxy`, and `GeneratedMethodAccessor` /
`GeneratedConstructorAccessor` / `GeneratedSerializationConstructorAccessor` →
`ReflectionAccessor`. When such a name arrives through `ensure_synthetic_class`
instead, the current code still stamps `CompatibilityStub` and merely mentions
the real flavour in the `reason`. Those are the same migration.

"Known callers" in the marker means what wave 1 could identify, not a proof of
completeness. `proxy_gen.rs` has 5 `ensure_synthetic_class` sites in the current
tree, not 1; each needs its own adjudication.

## What specifically must change

1. Migrate `AnonymousObject$N` to
   `ensure_generated_class(name, n, ClassOrigin::VmInternal)`.
   `ensure_generated_class` already exists (`class_manager.rs` ~2969) and has no
   callers yet.
2. Decide, and then implement, the right origin for `Proxy$Instance` —
   `GeneratedProxy` per the marker, with an answer for its `interfaces` field,
   or `VmInternal` with the marker corrected.
3. Audit the remaining `proxy_gen.rs` sites and any `ensure_synthetic_class`
   caller whose name matches `generated_class_origin_for_name`.
4. Run the full regression suite specifically looking for behaviour changes at
   the 181 `is_synthetic_stub` read sites — this is the step wave 1 could not do
   and is the entire reason the work was deferred.
5. Only after that: delete `is_synthetic_stub` and let `origin` be the single
   source of truth (contract §5: *"Convert it to a pure derived mirror now,
   delete it in a later wave."*).

## How to verify a fix

* `--dump-class-origins` must show `vm-internal` (not `compatibility-stub`) for
  `cratonvm/synthetic/AnonymousObject$N`, the chosen origin for
  `java/lang/reflect/Proxy$Instance`, and `generated-proxy` / `generated-lambda`
  / `reflection-accessor` for the name-recognised flavours.
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
  are ~179 other read sites that have not been individually reviewed.
* Reclassifying too eagerly hides a genuine compatibility substitution behind a
  legitimate-looking origin, which makes the zero-stub gate report green while
  the substitution continues. That is the same failure mode as the
  `ensure_generated_class` misuse described in
  [`ensure_synthetic_class` cannot enforce](ensure-synthetic-class-cannot-enforce-only-record.md),
  and it is only guarded by a `debug_assert!`.
