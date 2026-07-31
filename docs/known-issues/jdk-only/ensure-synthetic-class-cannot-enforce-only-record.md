# `ClassManager::ensure_synthetic_class` can record a JDK-only violation but cannot refuse one

**Status:** OPEN — JDK-only wave-2 work item, filed 2026-07-31. **DANGEROUS:
under `--jdk-only` this API records the violation and then fabricates the class
anyway, so the run reports a violation while continuing in the exact state the
contract forbids.**

> **Evidence provenance.** The `try_ensure_synthetic_class` /
> `ensure_generated_class` / `synthetic_name_origin` machinery and the
> `// JDK-ONLY-WAVE2:` marker quoted below were present in the working tree of
> `C:\craton\cratonvm` (branch `dev`, HEAD `0c54a9184`) on 2026-07-31 and were
> read directly. Between then and this record being written, the uncommitted
> wave-1 edits to `classloading/src/class_manager.rs` were reverted out of the
> working tree — see the *Wave-1 revert* note in
> [`README.md`](README.md). The **caller counts and the pre-existing
> `ensure_synthetic_class` signature below are re-verified against the current
> tree and are independent of that.**

## What is wrong

```rust
pub fn ensure_synthetic_class(&mut self, name: &str, num_fields: usize) -> ClassId
```

The return type is a bare `ClassId`. There is no error channel. A refusal cannot
be expressed, so under `CompatibilityMode::JdkOnly` the function records a
`CompatibilityClassRequested` violation and then fabricates the class regardless:

```rust
let origin = Self::synthetic_name_origin(name);
if origin.is_compatibility_stub() {
    let reason = origin.reason().unwrap_or("compatibility stub").to_string();
    self.record_compatibility_class_violation(name, Some(ClassLoaderId::Bootstrap), &reason);
}
self.fabricate_class(name, num_fields, origin)
```

The function's own wave-1 doc comment states the constraint plainly: *"This
entry point is **infallible by contract with its ~70 callers** across 33 files,
so it cannot report a refusal."*

Contract §5 requires the opposite: *"Under `JdkOnly`, every path that today
fabricates a class … must instead return the specification-appropriate
`ClassNotFoundException` / `NoClassDefFoundError` and record a
`CompatibilityClassRequested` violation."*

**The `load_class` chain does enforce.** This is the important distinction: an
absent enterprise or JDK class arriving through ordinary class loading is
correctly refused (`create_synthetic_stub`). It is this *direct* API — used by
VM bootstrap and by natives that want an allocation shape — that cannot.

## Scale (re-verified 2026-07-31 against the current tree)

`.ensure_synthetic_class(` matches **68 times across 32 files** (ripgrep,
workspace, gitignored paths excluded). Excluding 2 documentation hits and 2
integration-test hits leaves **64 live Rust call sites in 28 files**, of which
12 are `class_manager.rs`'s own unit tests. The heaviest callers:

| File | Sites |
|---|---|
| `classloading/src/proxy_gen.rs` | 5 |
| `vm/src/vm/vm_init.rs` | 5 |
| `native-builtins/src/lang_system.rs` | 5 |
| `native-io/src/process.rs` | 4 |
| `vm/src/vm/vm_exec.rs` | 3 |
| `vm/src/native/jni.rs` | 3 |
| `vm/src/vm.rs`, `native-collections/src/lib.rs`, `native-builtins/src/{lib,lang_string,keystore,util_concurrent_ext}.rs` | 2 each |
| 14 further `native-builtins` / `native-io` modules | 1 each |

The orchestrator's "~70 callers across ~33 files" is accurate to within rounding.

## The concrete symptom this produces today

`vm/src/vm/vm_init.rs`'s bootstrap block carries a wave-1 note describing the
end state precisely:

> every `ensure_synthetic_class` call in this bootstrap block fabricates a class
> with no real bytes, which is exactly what `CompatibilityMode::JdkOnly`
> forbids. The mode is already installed on `class_manager` above, so the
> refusal happens inside `classloading` and is recorded as a
> `CompatibilityClassRequested` violation. `ensure_synthetic_class` returns a
> bare `ClassId` and cannot report the refusal here, so the `get_class_mut(...)`
> wiring below simply finds nothing and is skipped — benign, but it means the
> strict boot **silently loses** `Enumeration$Impl` / `Comparator$Native` / the
> unmodifiable-view carriers rather than failing loudly.

"Silently loses, rather than failing loudly" is the whole problem: a strict run
does not crash, it quietly boots with less than it thinks it has.

## The migration recipe (from the in-code `JDK-ONLY-WAVE2` marker)

> Migrate the ~70 `ensure_synthetic_class` callers to
> `try_ensure_synthetic_class` (or, where the class is legitimately generated,
> to `ensure_generated_class` with an explicit origin), then delete this
> wrapper. Until then `--jdk-only` is diagnostic-only on this path. The
> `load_class` fabrication chain — which is where an absent enterprise or JDK
> class actually arrives — does enforce; see `create_synthetic_stub`.

Three entry points exist for this (they were added in wave 1 and are part of the
reverted change set — re-landing them is a prerequisite):

* `try_ensure_synthetic_class(name, n) -> Result<ClassId, VmError>` — same
  behaviour, but `Err(ClassFileError::ClassNotFound)` under `JdkOnly`, which
  constant-pool resolution already translates to `NoClassDefFoundError`.
  Under `Compatible` it is byte-for-byte `ensure_synthetic_class` wrapped in
  `Ok`.
* `ensure_generated_class(name, n, origin)` — for arrays, hidden classes,
  lambdas, proxies, reflection accessors and VM-internal shapes (contract §1
  item 6). Never refused, in either mode; `debug_assert`s that the caller did
  not pass a `CompatibilityStub` origin.
* `ensure_synthetic_class` — the infallible wrapper, to be deleted last.

## Why it was not fixed in wave 1

Touching 64 call sites across `classloading`, `vm`, `native-builtins`,
`native-collections`, `native-io` and `vm/src/native/jni.rs` means editing files
owned by six other agents in the same wave, and every one of those call sites
needs a *judgement*: is this class a compatibility substitution (→ `try_…`) or a
legitimately-generated shape (→ `ensure_generated_class`)? That judgement cannot
be made mechanically and cannot be validated without running the regression
suite. Contract §10 scopes wave 1 to measurement.

## What specifically must change

1. Re-land `try_ensure_synthetic_class` / `ensure_generated_class` (they were
   reverted with the rest of wave 1's `class_manager.rs` edits).
2. Migrate call sites **subsystem by subsystem**, deciding per site between the
   two fallible/legitimate entry points. `proxy_gen.rs` (5 sites) and the
   `cratonvm/synthetic/AnonymousObject$N` allocation shape in `vm_exec.rs` are
   the clearest `ensure_generated_class` candidates — see
   [VM-internal classes are mislabelled `CompatibilityStub`](vm-internal-classes-mislabelled-compatibility-stub.md).
3. Make `vm_init.rs`'s bootstrap block fail loudly under `JdkOnly` instead of
   skipping its `get_class_mut` wiring — that is the specific behaviour the note
   above flags as benign-but-wrong.
4. Delete `ensure_synthetic_class`.

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
