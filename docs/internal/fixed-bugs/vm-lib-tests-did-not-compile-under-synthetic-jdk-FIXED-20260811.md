# `cratonvm-vm --lib --features synthetic-jdk` had not compiled since two green branches met

| | |
|---|---|
| **Status** | FIXED 2026-08-11. 4,001 passed, 0 failed. |
| **Area** | `vm/src/vm/tests.rs` — the 72k-line inline test module, `#[cfg(all(test, feature = "synthetic-jdk"))]` |
| **Symptom** | `error[E0061]: this method takes 5 arguments but 4 arguments were supplied` ×10, then six red tests behind it |

## What happened

Two branches landed on 2026-08-10, hours apart, each verified green on its own:

* `7a601cac7` gave `JitMICSlot::update` a fifth parameter, `jdk_only: bool`,
  and updated every call site in its own ancestry.
* `dc55e8057` deleted 179 "dead everywhere" native registrations, including the
  field-shaped constant pseudo-natives on `java/sql/Types`,
  `java/nio/file/FileVisitResult`, `java/nio/file/StandardOpenOption` and
  `java/nio/file/attribute/PosixFilePermission`.

Neither is an ancestor of the other. `dc55e8057` never saw the arity change, and
`7a601cac7` never saw the deletions. They met in dev, and from that merge
onward this module did not compile — so its 4,000 tests had not run since.

Neither branch could have caught it, and the reason is worth naming: both
verified with **`cargo check --workspace --all-targets`, which compiles tests
and runs none.** That is enough to catch an arity break in your own ancestry
and nothing at all about whether a test still passes.

## Two layers, and the second was hiding under the first

**Layer 1 — the arity break.** Ten `mic.update(...)` calls in the `s33_mic_*`
block. All ten use synthetic sentinel entry addresses (`0x1000`, `0xABCD0000`,
…) and assert that `cached_entry_ptr` comes back holding them, so the value
that preserves each test's meaning is `jdk_only = false`: under `JdkOnly` an
entry with no live `CompiledMethod` owner is REFUSED, and every one of those
assertions would be reading a 0.

**Layer 2 — six tests that had been red since `dc55e8057`.** Once the module
compiled again:

```
test result: FAILED. 3999 passed; 6 failed
    vm::tests::file_visit_result_enum_p57
    vm::tests::posix_file_permission_enum_p70
    vm::tests::sql_types_constants_p68
    vm::tests::standard_charsets_all_variants
    vm::tests::standard_charsets_utf8
    vm::tests::standard_open_option_enum_p57
```

All six called `call_native(class, FIELD_NAME, FIELD_DESCRIPTOR)` — a field
name with a field descriptor, asking the native registry for a pseudo-method
that served a static constant (`java/sql/Types.INTEGER` with descriptor `"I"`).
`dc55e8057` deleted that whole family, correctly: no JDK image has a *method*
by those names, which is exactly why the census called them dead everywhere.
The tests were simply not updated with it, and `cargo check` could not say so.

The leftovers are still visible at the deletion sites — `let fvr =
"java/nio/file/FileVisitResult";` and `let soo =
"java/nio/file/StandardOpenOption";` in `phases_late/nio_file.rs`, and `let
types = "java/sql/Types";` under a comment reading **"KEEP (deliberate
constants)"** in `phases_late/jdbc.rs` — three bindings with no registrations
under them any more.

## What was done

* **The ten call sites** take `false`, with the reason written at the first one
  rather than left as a bare literal.
* **A test for the `true` branch.** `update` grew that parameter with no test
  taking it: `jdk_only_ic_native_refusals()`, the counter it bumps, is read
  only by `--jdk-only-report`'s diagnostics struct, so the refusal could have
  been inverted or dropped and every suite stayed green.
  `s33_mic_update_refuses_native_entry_under_jdk_only` runs the same unowned
  sentinel through both modes — the flag is the only difference between the two
  arms — and asserts that JDK-only leaves the class guard and name installed
  while refusing the entry, which is a downgrade to "class cached, target
  unresolved", not a dropped update. **Red proved:** flipping the second arm's
  flag to `false` fails it on the intended assertion (`left: 1526595584, right:
  0`).
* **The two `StandardCharsets` tests are re-pointed**, not retired: the
  field-shaped natives they called were replaced on 2026-05-21 by a registered
  `<clinit>` that populates the six real static slots. They now drive that
  `<clinit>` and read the statics back — the production path. Note the
  `<clinit>` writes through `set_static_field_by_name`, which resolves the
  class by name and is a **silent no-op** when it is not loaded, hence the
  explicit `ensure_class_initialized` first.
* **The other four are retired**, with the reason in place of the body. Unlike
  `StandardCharsets` there is no replacement mechanism to assert against, and
  re-registering a pseudo-method to keep a test green would reverse a landed,
  documented decision from inside a test file. The deletion stays governed
  where it belongs: `scripts/baselines/jdk-only-dead-everywhere.tsv`,
  `jdk-only-dead-sweep.py`, and `registry_contracts.rs` for the registrations
  that must survive.

## The residual this leaves

In `--features synthetic-jdk` builds there is now no source for
`java.sql.Types.INTEGER`, `FileVisitResult.CONTINUE`,
`StandardOpenOption.READ` or `PosixFilePermission.OWNER_READ`: no class file,
no `<clinit>`, no natives. The census says nothing calls them, and the
real-JDK/`--jdk-only` paths read the real static finals from bytecode, so this
is a legacy-mode gap rather than a product one. `StandardCharsets` shows the
shape a fix would take if one is ever wanted — a `<clinit>` populating the
statics that `synthetic_stub_fields` already models — and is the reason those
two tests could be saved while these four could not.

## Verification

| run | result |
|---|---|
| `-p cratonvm-vm --lib --features synthetic-jdk` | 4,001 passed, 0 failed, 118 ignored |
| `-p cratonvm-vm --lib` (default) | 2,487 passed, 0 failed |
| `-p cratonvm-jit --lib` | pass |
| `-p cratonvm-vm --test tier1_tests --features synthetic-jdk` | pass |
| extended interpreter corpus, `--features synthetic-jdk` | 924 passed |
| mutation: new test's second arm flipped to `false` | FAILS, as intended |
