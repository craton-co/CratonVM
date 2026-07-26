# Loader-blind class lookup remains in production paths

Status: open
Found: 2026-07-26 architecture release build
Base: `dev` at `3be41785e`

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

## Required fix

1. Classify every call by the loader already available at that point: current
   method owner, receiver class, constant-pool owner, initiating loader, or an
   already-resolved `ClassId`.
2. Replace the lookup with `find_class_by_name_for_loader` or a more specific
   API that encodes the intended resolution rule.
3. Add dual-isolated-loader tests for each migrated family. The same binary name
   must resolve to the caller-appropriate class and caches must not cross the
   loader boundary.
4. After the production count reaches zero, deny use of the loader-blind method
   outside narrowly named diagnostic/test modules.

The migration should be split by semantic family. A bulk textual replacement
that guesses one loader for all 47 sites is unsafe.

## Verification

```bash
cargo build --release -p cratonvm-cli 2>&1 |
  grep -c 'loader-blind; use find_class_by_name_for_loader'
```

Expected after the fix: `0`, followed by the existing multi-loader application
and proxy suites with both interpreter and default JIT modes.
