<!--
SPDX-License-Identifier: Apache-2.0
Copyright 2024-2026 Craton Software Company
-->

# Fuzz Corpora

Tracked seed corpora are intentionally empty today. `cargo fuzz` will create
`fuzz/corpus/<target>/` directories as targets run.

When a bug is fixed, commit the minimized reproducer under the matching target
directory with a descriptive `regression-` prefix, for example:

```text
fuzz/corpus/fuzz_classfile/regression-nested-code-attribute
```

Current target names:

- `fuzz_classfile`
- `fuzz_read_class`
- `fuzz_jimage`
- `fuzz_stack_map`
- `fuzz_instruction`
- `fuzz_descriptor`
- `fuzz_verifier`
- `fuzz_asn1`
- `fuzz_keystore`
- `fuzz_tls_record`
- `difftest_bytecode`
