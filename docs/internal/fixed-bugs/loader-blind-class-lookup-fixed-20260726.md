# Loader-blind production class lookup removed

Status: fixed
Found: 2026-07-26 architecture release build
Base: `dev` at `3be41785e`
Fixed: 2026-07-27 on `codex/complete-architecture-remediation-20260726`

## Evidence

A default real-JDK release build emits 47 warnings of this form:

```text
use of deprecated method
`cratonvm_classloading::ClassManager::find_class_by_name`:
loader-blind; use find_class_by_name_for_loader
```

The calls are distributed across `vm/src/runtime/interpreter.rs`,
`vm/src/vm/vm_exec.rs`, and `vm/src/vm/vm_util.rs`, including invocation,
reflection, exception, proxy, array, and utility paths. Past targeted
loader-identity bugs have been fixed, but the deprecated API remains generally
available and in active production code.

## Architectural problem

A binary class name is not a unique class identity. The defining loader is part
of that identity, and resolution also depends on the initiating loader. A
global name lookup can return a class from the wrong namespace, make a valid
class appear missing, or poison a cache whose key omits loader identity.

Suppressing the warning or making the global lookup choose a preferred loader
does not fix the contract.

## Resolution

All 47 production calls were classified and migrated:

- constant-pool, exception-table, reflection, and JIT metadata references use
  `find_class_by_name_for_class`, deriving the requesting loader from an
  already-resolved owner `ClassId`;
- platform identities use an exact bootstrap-only lookup;
- JNI `UnregisterNatives` retains the exact `ClassId` supplied by the class
  mirror instead of resolving its name again;
- genuinely context-free diagnostics use an ambiguity-refusing unique lookup;
- built-in lookup is parent-first and never searches a child namespace, while
  user lookup never scans an unrelated user namespace.

The VM crate now has `#![deny(deprecated)]`. The deprecated loader-blind API is
therefore a compile error in production code, including all-feature builds.
The source count is zero across `vm/src`.

## Verification

```bash
git grep -n '\.find_class_by_name(' -- vm/src
# no output

cargo test -p cratonvm-classloading loader_lookup_tests -- --nocapture
# 3 passed

cargo test -p cratonvm-classloading --test wp_security_robustness \
  two_loaders_same_name_yield_distinct_class_ids -- --nocapture
# 1 passed

cargo check -p cratonvm-vm --all-features
# passed

cargo build --release -p cratonvm-cli \
  --target-dir /data/data/target-complete-architecture-remediation-20260726
# passed; unique binary:
# /data/data/bin/cratonvm-complete-remediation-20260726-r2
```

The release log contained zero
`loader-blind; use find_class_by_name_for_loader` warnings.
