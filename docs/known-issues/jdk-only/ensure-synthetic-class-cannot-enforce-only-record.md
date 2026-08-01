# `ClassManager::ensure_synthetic_class` can record a JDK-only violation but cannot refuse one

**Status:** OPEN — JDK-only wave-2 work item, filed 2026-07-31, re-verified
against the re-landed tree the same day. **DANGEROUS: under `--jdk-only` this
API records the violation and then fabricates the class anyway, so the run
reports a violation while continuing in the exact state the contract forbids.**

> **Evidence provenance.** The original filing quoted a
> `synthetic_name_origin`-based body that **no longer exists**; the re-land
> replaced it with a four-argument `fabricate_class(name, num_fields, origin,
> enforce)` and a shared `admit_compatibility_class` policy choke point. The
> quotations below are from the re-landed code in
> `C:\craton\wt-jdk-only` (branch `feat/jdk-only-mode`), read 2026-07-31. The
> *defect* is unchanged; only its shape is.

## What is wrong

`classloading/src/class_manager.rs` ~2918:

```rust
pub fn ensure_synthetic_class(&mut self, name: &str, num_fields: usize) -> ClassId
```

The return type is a bare `ClassId`. There is no error channel. A refusal cannot
be expressed, so the function passes `enforce: false` and fabricates regardless
of mode:

```rust
pub fn ensure_synthetic_class(&mut self, name: &str, num_fields: usize) -> ClassId {
    self.fabricate_class(
        name,
        num_fields,
        ClassOrigin::compatibility_stub(ENSURE_SYNTHETIC_STUB_REASON),
        // Record the violation, then fabricate anyway — there is no error
        // channel on this signature.
        false,
    )
    .expect("non-enforcing fabrication never returns Err")
}
```

`enforce` is documented on `fabricate_class` (~3037) exactly as the defect
describes it: *"`true` returns the `ClassNotFoundException` the contract asks
for, `false` records the violation and fabricates anyway. Either way the
violation is recorded, and either way `Compatible` mode fabricates."*

Contract §5 requires the opposite: *"Under `JdkOnly`, every path that today
fabricates a class … must instead return the specification-appropriate
`ClassNotFoundException` / `NoClassDefFoundError` and record a
`CompatibilityClassRequested` violation."*

**The `load_class` chain does enforce.** This is the important distinction: an
absent enterprise or JDK class arriving through ordinary class loading is
correctly refused (`create_synthetic_stub`, ~7289, which routes through
`admit_compatibility_class` at ~2455). It is this *direct* API — used by VM
bootstrap and by natives that want an allocation shape — that cannot.

The re-land made the policy a single choke point, which is a genuine
improvement worth keeping: `admit_compatibility_class` is called from exactly
two places (`create_synthetic_stub` and `fabricate_class`), *"so there is no
third place a stub can be minted without the policy seeing it."* The problem is
no longer "the policy can be bypassed"; it is "the policy is seen and then
overridden by a signature".

## The fallible siblings exist — and have zero callers

Both entry points the original filing asked for were re-landed:

* `try_ensure_synthetic_class(name, n) -> Result<ClassId, VmError>` (~2941) —
  `ClassNotFoundException` under `JdkOnly`, which constant-pool resolution
  already translates to `NoClassDefFoundError`. Byte-for-byte
  `ensure_synthetic_class` under `Compatible`.
* `ensure_generated_class(name, n, origin)` (~2969) — for arrays, hidden
  classes, lambdas, proxies, reflection accessors and VM-internal shapes
  (contract §1 item 6). Never refused, in either mode; `debug_assert`s that the
  caller did not pass a `CompatibilityStub` origin.

**Neither has a single caller outside `class_manager.rs`** (ripgrep,
2026-07-31). The migration is the work; the API was never the blocker. Note in
particular that `try_ensure_synthetic_class`'s doc comment names its intended
chief caller — *"the `java/util/function/Function$Identity` stand-in minted by
the stream/function natives"* — and that caller still goes through
`ensure_synthetic_class` via `alloc_concurrent_synthetic`. See
[the ambient-`NativeKind` record](native-kind-is-ambient-and-defaults-to-syntheticstub.md)
for why that particular one is now load-bearing in a new way.

## Scale (re-counted 2026-07-31 against the re-landed tree)

`.ensure_synthetic_class(` matches **66 times across 30 files** (ripgrep,
workspace, `--include=*.rs`). Excluding 12 hits inside `class_manager.rs`'s own
`mod tests` (from line 14007) and 2 integration-test hits
(`vm/tests/jdk_only_dispatch.rs`, `classloading/tests/jdk_only_class_origin.rs`)
leaves **52 live call sites in 27 files**. The heaviest callers:

| File | Sites |
|---|---|
| `vm/src/vm/vm_init.rs` | 5 |
| `native-builtins/src/lang_system.rs` | 5 |
| `classloading/src/proxy_gen.rs` | 5 |
| `native-io/src/process.rs` | 4 |
| `vm/src/vm/vm_exec.rs` | 3 |
| `vm/src/native/jni.rs` | 3 |
| `vm/src/vm.rs`, `native-collections/src/lib.rs`, `native-builtins/src/{lib,lang_string,keystore,util_concurrent_ext}.rs` | 2 each |
| 15 further `native-builtins` / `native-io` modules | 1 each |

*(The original filing said 64 live sites in 28 files, derived from a count of 68
that included two documentation hits. Both numbers describe the same population;
the current figure is the one to work from.)*

## The concrete symptom this produces today

`vm/src/vm/vm_init.rs`'s bootstrap block mints `java/util/Enumeration$Impl`
(~1052) and `java/util/Comparator$Native` (~1107) through this API and then
wires them up with `get_class_mut`. Under `JdkOnly` the fabrication is recorded
as a violation and happens anyway, so the wiring succeeds and the run continues
in the state §5 forbids.

**UNVERIFIED against the re-landed tree:** the original filing quoted a wave-1
note in that block predicting the *other* outcome — that the fabrication would
be refused, `get_class_mut` would find nothing, and *"the strict boot silently
loses `Enumeration$Impl` / `Comparator$Native` … rather than failing loudly."*
That note is not in the re-landed `vm_init.rs`, and with `enforce: false` the
refusal it describes cannot occur on this path. Which of the two behaviours a
strict boot actually shows needs a run, not a reading. Either way the boot does
not fail loudly, which is the point of the item.

## The migration recipe (from the in-code `JDK-ONLY-WAVE2` marker, ~2899)

> `ensure_synthetic_class` returns a bare `ClassId` — there is no error channel
> — and it has ~70 callers across ~33 files, almost all of them inside
> `native-builtins` allocation helpers such as `alloc_concurrent_synthetic`,
> which likewise return a value rather than a `Result`. … Migration recipe for
> wave 2, per call site:
>   1. If the caller is generating a legitimate VM class (a lambda, a proxy, a
>      reflection accessor, an internal allocation shape), switch it to
>      `ensure_generated_class` with the matching `ClassOrigin` — it is never
>      refused, in either mode.
>   2. If the caller is standing in for a class whose real bytes should have
>      been found, switch it to `try_ensure_synthetic_class` and propagate the
>      `ClassNotFoundException` up through the native's own error path.
>   3. When no caller remains, delete this method.

## Why it was not fixed in wave 1

Touching 52 call sites across `classloading`, `vm`, `native-builtins`,
`native-collections`, `native-io` and `vm/src/native/jni.rs` means editing files
owned by six other agents in the same wave, and every one of those call sites
needs a *judgement*: is this class a compatibility substitution (→ `try_…`) or a
legitimately-generated shape (→ `ensure_generated_class`)? That judgement cannot
be made mechanically and cannot be validated without running the regression
suite. Contract §10 scopes wave 1 to measurement.

## What specifically must change

1. Migrate call sites **subsystem by subsystem**, deciding per site between the
   two entry points, which already exist. `proxy_gen.rs` (5 sites) and the
   `cratonvm/synthetic/AnonymousObject$N` allocation shape in `vm_exec.rs` are
   the clearest `ensure_generated_class` candidates — see
   [VM-internal classes are mislabelled `CompatibilityStub`](vm-internal-classes-mislabelled-compatibility-stub.md).
2. Make `vm_init.rs`'s bootstrap block fail loudly under `JdkOnly` instead of
   fabricating `Enumeration$Impl` / `Comparator$Native` behind a recorded
   violation.
3. Delete `ensure_synthetic_class`.

## How to verify a fix

* A `--jdk-only` boot on a complete real JDK image must reach `main` with **zero**
  `compatibility-class-requested` violations in the `--jdk-only-report` JSON.
  Any remaining violation names the exact class and the reason.
* Grep gate: `.ensure_synthetic_class(` must match zero non-test sites.
* `--dump-class-origins` must show no `compatibility-stub` rows for non-array
  JDK/application/dependency classes (contract §11).
* `Compatible` mode must be byte-for-byte unchanged — the existing regression
  suite plus `native-builtins/tests/stub_ratchet.rs` (`BASELINE_SYNTHETIC_STUBS
  = 157`, `SLACK = 0`).

## Blast radius if done wrong

* Migrating a **legitimately-generated** class to `try_ensure_synthetic_class`
  makes `--jdk-only` reject proxies, lambdas or array shapes — an immediate,
  loud, but wrong failure that will be misread as "strict mode doesn't work".
* Migrating a **compatibility stub** to `ensure_generated_class` is the
  dangerous direction: it silences the violation, keeps fabricating, and makes
  the zero-stub census report green while the substitution is still happening.
  Contract §11's acceptance criterion becomes unfalsifiable.
* Because `ensure_generated_class` only `debug_assert!`s on a `CompatibilityStub`
  origin, a release build will not catch the second mistake at all.
* Note that `admit_compatibility_class` dedupes by class name
  (`origin_violations_seen`), so a migrated caller that stops fabricating a
  name some *other* caller also requests will not change the violation count.
  Count call sites, not violations, when checking migration progress.
