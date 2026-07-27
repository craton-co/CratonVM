# Package-selected interpreter routing removed

Status: fixed
Found: 2026-07-26 architecture probe
Fixed: 2026-07-27 on `codex/complete-architecture-remediation-20260726`

## Problem

`Frame::is_jdk_class` was computed from class-name prefixes and made identical
verified bytecode choose different interpreter implementations. Most
`java/*`, `jdk/*`, `sun/*`, `com/sun/*`, and `org/springframework/*` code
skipped the raw-byte/superinstruction handlers and always used the decoded
fallback. The policy encoded historical compatibility incidents rather than a
method capability and imposed a measured 2.6x penalty merely for moving an
identical class into the Spring package.

## Resolution

- Removed `class_disables_interp_fast_path` and the per-frame package-policy
  field.
- All verified bytecode now enters the common raw-byte handler set.
  Unsupported opcodes, malformed shapes, null/error cases needing richer
  diagnostics, and global `--noverify` runs still use the decoded fallback.
- Unified reference load/store/return coercion on the heap-validating helper.
- Unified `aastore` bridge recovery on one allocation-only normalizer.
- Added the missing JVMTI normal `MethodExit` event to raw-byte value and void
  returns.

The decoded implementation remains as a checked fallback, but package names no
longer choose it and overlapping handlers now share the corrected boundary
semantics.

## Verification

Unique release binary:

```text
/data/data/bin/cratonvm-complete-remediation-20260726-r3
```

The opcode-rich differential probe:

```bash
tools/architecture-probe-20260726/check-interpreter-equivalence-20260727.sh \
  /data/data/bin/cratonvm-complete-remediation-20260726-r3 \
  /home/victor/jdk25
```

produced the same unsigned checksum on the verified raw-byte path and forced
decoded fallback:

```text
3471186786924291744
```

The existing interpreter integration corpus passed 924/924 and the frame unit
subset passed 53/53. A release paired measurement using identical bytecode and
two million iterations produced:

| binary | default package | `org.springframework.*` | checksums |
|---|---:|---:|---|
| pre-fix r2 | 587,361,361 ns | 1,465,455,335 ns | equal |
| fixed r3 | 552,346,284 ns | 561,028,707 ns | equal |

The existing general `differential.rs` ignored harness is not a valid
non-`main` return-value oracle: its HotSpot subprocess runner does not print
method return values. Its `diff_basic_arithmetic` mismatch was reproduced
without this change and is not used as evidence here.
