# Six flag rows whose `off_word` disagrees with their reader's default

> **STATUS (2026-10-19, gcj j3/x2): OPEN -- for the owners (none is Generational or common GC code).**
> Found by the first run, by script, of the rule of the new guard
> `types/tests/flag_default_matches_reader.rs` over the j3 base `43edda6dbe` (the guard itself has not run
> under cargo: no-build rule). The guard lists these six in `KNOWN_MISMATCHES`, and
> `the_known_mismatches_have_no_dead_rows` fails when one of them is fixed, so the list only shrinks.

`flag_groups::E::off_word` is metadata (the `CRATONVM_<GROUP>=-token` spelling); the default lives in
the reader. Of 4385 single-row keys with an `on_key` and no `off_key`, 2909 have a reader site the rule
can see (a `runtime_flag_on(` / `runtime_flag_default_on(` / `runtime_flag_default_on_if(` call opening
right before the key's literal); six disagree:

| key | row | reader | file |
|---|---|---|---|
| `CRATONVM_INTERP_FUSED_INCREMENT_EARLY_DECLINE` | `off_word: Some` (default ON) | `runtime_flag_on` (opt-in) | `vm/src/config.rs` ~4047 |
| `CRATONVM_JIT_ARM64_FIELD_SITE_TRANSIENT` | `Some` | `runtime_flag_on` | `jit/src/aarch64_backend.rs` ~851 |
| `CRATONVM_ZGC_SOFT_MAX_MANAGEABLE` | `Some` | `runtime_flag_on` | `native-builtins/src/jmx.rs` ~987 |
| `CRATONVM_ZGC_JIT_SHORT_ANNOUNCE` | `Some` | `runtime_flag_on` | `jit/src/x64/objects.rs` ~206 |
| `CRATONVM_ZGC_JIT_ANNOUNCE_EPOCH_GATE` | `Some` | `runtime_flag_on` | `gc/src/zgc/vm_tlab.rs` ~187, `jit/src/x64/objects.rs` ~522 |
| `CRATONVM_JIT_IR_VH_READ_BINDS` | `None` (opt-in) | one arm `runtime_flag_default_on` (ByteBuffer descriptors) | `jit/src/lib.rs` ~26564-26566 |

Effect: for the five `Some` rows, `CRATONVM_<GROUP>=-token` writes `KEY=0`, which the opt-in reader
already reads as off -- harmless, but the generated inventory documents the switch as default ON while it
is opt-in (either a flip that changed only the row, the j2 failure mode, or a reader returned to opt-in
without its row). The `None` row is a mixed default the inventory cannot express.

Closes it: each owner either flips the reader (`runtime_flag_default_on`, or
`runtime_flag_default_on_if(key, scoped)` for a collector-scoped default) or sets `off_word: None`, and
deletes the guard's row. ZGC rows: the ZGC round's owner.

Blind spot (stated in the guard): 1476 keys are read through a `const`, a typed `GcFlags` field, a
hand-rolled `runtime_var` match or a built name, and are not judged.
