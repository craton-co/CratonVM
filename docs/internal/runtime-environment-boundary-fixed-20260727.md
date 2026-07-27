# Runtime environment boundary remediation

## Problem

The runtime already exposed an immutable, typed `VmFlags` snapshot, but seven
core crates still bypassed it with hundreds of direct `std::env::var` and
`std::env::var_os` calls.  That made process-wide behavior depend on when a
particular subsystem happened to read the environment, made flags difficult to
inventory, and allowed tests or embedding applications to mutate declared VM
configuration after initialization.

## Resolution

`cratonvm_types::flags::runtime_var` and `runtime_var_os` now form the legacy
string compatibility boundary:

* declared CratonVM flags are captured once while `VmFlags` is constructed and
  are subsequently served from that immutable snapshot;
* ordinary, undeclared operating-system and application variables retain live
  `std::env` semantics;
* all direct `std::env::var`/`var_os` calls were removed from `vm`, `jit`, `gc`,
  `classloading`, `native-api`, `native-builtins`, and `native-collections`;
* the three previously undeclared diagnostic flags
  `CRATONVM_DBG_UCLTRACE`, `CRATONVM_MOVING_YOUNG_BAND_DBG`, and
  `CRATONVM_MOVING_YOUNG_NO_BAND_VERIFY` are now part of the canonical
  inventory.

The flag-surface check rejects new direct environment reads in those crates, so
future configuration must either use a typed `VmFlags` field or the immutable
compatibility boundary.

## Verification

* `tools/flag-census/check-surface.sh`
* `cargo test -p cratonvm-types`
* `cargo check -p cratonvm-vm -p cratonvm-jit -p cratonvm-gc
  -p cratonvm-classloading -p cratonvm-native-api
  -p cratonvm-native-builtins -p cratonvm-native-collections`

The surface census covers 566 environment variables, 544 grouped tokens, and
15 user-facing names.
