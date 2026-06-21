# T11 safety-annotation coverage thresholds not met

**Status:** OPEN. Safe to fix (documentation-only), but large.

**Failing tests:** `cargo test -p cratonvm-vm --test t11_safety_conformance`
(`t11_1_*`, `t11_3_*`, `t11_4_*`).

## What the test enforces
`t11_safety_conformance` scans source files and asserts a minimum fraction of:
- **unsafe blocks** preceded by a `// SAFETY:` comment (T11.1, ≥ 90%),
- **`as` casts** annotated with `// SAFETY:` / `// Widening:` / `// Truncation` / `// Cast:` /
  `// JVM spec:` on the same or one of the 3 preceding lines (T11.3, ≥ 70%),
- **`Box::leak` / `Box::into_raw`** sites documented with `// LEAK` / `// OWNERSHIP` (T11.4, ≥ 80%).

## Current coverage vs threshold (gaps)
| File | Check | Now | Need | Missing (approx) |
|------|-------|-----|------|------|
| jit/src/x64.rs | SAFETY | 89% | 90% | ~3 unsafe blocks |
| jit/src/x64.rs | cast | 65% | 70% | ~46 casts |
| jit/src/x64.rs | leak | 47% | 80% | ~6 leak sites |
| gc/src/gen_heap.rs | SAFETY | 74% | 90% | ~52 unsafe blocks |
| vm/src/jit/helpers.rs | SAFETY | 71% | 90% | ~18 unsafe blocks |
| vm/src/runtime/interpreter.rs | SAFETY | 85% | 90% | ~8 unsafe blocks |
| vm/src/runtime/interpreter.rs | cast | 51% | 70% | **~264 casts** |

## Fix
Add **accurate** safety/cast/leak annotations to the real unsafe blocks, `as` casts, and
`Box::leak`/`into_raw` sites in the listed files until each file crosses its threshold.

**Do not game the gate** by spraying empty marker comments — the test only checks for the marker
text, so that would pass mechanically while defeating the gate's purpose. Each annotation should
state the real invariant (why the unsafe deref/pointer is valid; widening vs lossy truncation and
why it is safe per the JVM spec; ownership/lifetime of the leaked allocation).

The bulk is `interpreter.rs` (~264 casts) — best done as a dedicated, reviewed documentation pass,
ideally file-by-file.

## Risk
None to runtime behavior (comments only). The only risk is low-quality/incorrect annotations, so
this needs careful authoring + review rather than a bulk script.
