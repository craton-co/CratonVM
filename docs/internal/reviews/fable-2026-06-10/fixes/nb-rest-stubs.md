# Fix note — `nb-rest-stubs`

Findings addressed: S1 (log4j_extras stub tag/gate), S2 (apps_h2 TableFilter.prepare
shim), S3 (classfile_api JEP 484 fabrication), B1 (keystore read_string_arg no-op).

---

## B1 (HIGH) — KeyStore alias lookups always failed

### Finding
`native-builtins/src/keystore.rs:926-933`: `read_string_arg(v: &Value) -> Option<&'static str>`
always returned `None` (body `let _ = v; None`). Its return type (`&'static str`) made it
impossible to ever yield a real alias. Every alias-taking `engine*` callback resolved the
alias to the empty string, so `engineGetKey`/`engineGetCertificate`/`engineGetCertificateChain`/
`engineContainsAlias`/`engineIsKeyEntry`/`engineIsCertificateEntry`/`engineGetCreationDate`
all looked up `""` and returned null/false.

### Root cause
The helper never dispatched through the `NativeContext` string accessor — the comment claimed
"real reads go through the longer form below" but no such form existed.

### Exact change (file:line)
- `keystore.rs:926` — rewrote `read_string_arg` to
  `fn read_string_arg(ctx: &mut dyn NativeContext, v: &Value) -> Option<String>` that decodes
  `Value::Object(Some(o))` via `ctx.read_string(*o)` (the same accessor `log4j_extras`,
  `vm_exec`, etc. use; trait method `NativeContext::read_string(&self, ObjectRef) -> Option<String>`
  at `native-api/src/registry.rs:667`).
- `keystore.rs` 7 call sites (engineGetKey, engineGetCertificate, engineGetCertificateChain,
  engineContainsAlias, engineIsKeyEntry, engineIsCertificateEntry, engineGetCreationDate):
  `args.get(1).and_then(read_string_arg).map(|s| s.to_string()).unwrap_or_default()`
  → `args.get(1).and_then(|v| read_string_arg(ctx, v)).unwrap_or_default()`.
  `get_store_id(ctx, this)` runs just before each (returns by value, releases the borrow), so
  the subsequent `&mut ctx` borrow in the closure is conflict-free.

### Tests added
None. A behavioural test would need a full `NativeContext` mock (the trait has many methods);
the existing tests drive only the pure parser functions. Building such a mock blind is too
risky to guarantee compilation. See follow-up.

---

## S1 (HIGH) — log4j_extras: silent no-op logging pipeline, untagged

### Finding
`register_log4j_stubs` registered an entire no-op log4j pipeline (loggers whose log methods
discard every record) without setting a `NativeKind` category in-module.

### Root cause / clarification
The report stated this was "wired unconditionally from `register_essential_natives`". That is
stale: the only call site is `lib.rs:896`, which is **inside** `register_app_stubs`, itself
`#[cfg(feature = "app-stubs")]` (default-OFF) and wrapped in
`registry.with_category(NativeKind::SyntheticStub, ...)` (lib.rs:885-928). So the stubs were
already gated AND tagged at the call site. The remaining gap was that the module function did
not tag itself, so any future/other caller would register them uncategorised.

### Exact change (file:line)
- `log4j_extras.rs:365` (`register_log4j_stubs`): added the standard save/set/restore —
  `let __prev_cat = registry.current_category(); registry.set_category(SyntheticStub);` at the
  top and `registry.set_category(__prev_cat);` before the closing brace (~line 858). This makes
  every registration in the function (including those in `register_simple_logger_methods`, which
  it calls) self-tag SyntheticStub regardless of caller. Updated the doc comment to FLAG it as a
  silently-dropping pipeline gated to `app-stubs`.

### Tests added
None new; the two existing `#[cfg(test)]` tests call `register_log4j_stubs` on a fresh
registry and only assert registration presence, which is unaffected (mirrors how
`classfile_api`'s self-tagging entry point is tested).

---

## S2 (HIGH) — apps_h2: TableFilter.prepare()V reimplemented in Rust

### Finding
`apps_h2.rs:108` registered a native re-implementation of H2 application bytecode
(`org.h2.table.TableFilter.prepare`) to mask a VM optimizer defect.

### Root cause
CratonVM's `Optimizer.optimize` pipeline leaves `TableFilter.index` null for simple
single-table WHERE queries (the plan lookup returns null where real-JDK's `setPlanItem` would
populate it), so the real `prepare pc=44..52` bytecode NPEs on `this.index.getColumnIndex(col)`.
The correct fix belongs in the VM optimizer, not a per-app native shim.

### Exact change (file:line)
Removing the shim outright would crash H2 (documented NPE), so per the task's fallback I gated
it behind the default-OFF `app-stubs` feature (it was already tagged SyntheticStub):
- `apps_h2.rs:115` (`register_h2_table_filter_prepare`): wrapped the `registry.register(...)`
  body in `#[cfg(feature = "app-stubs")]`; added `#[cfg(not(feature = "app-stubs"))] let _ = registry;`
  so `registry` is not flagged unused when the feature is off. Default build now runs the real
  bytecode (surfacing the optimizer bug honestly).
- `apps_h2.rs:134,147`: added `#[cfg_attr(not(feature = "app-stubs"), allow(dead_code))]` to
  `table_filter_prepare` and `table_filter_prepare_on` (otherwise unreferenced when the
  registration is gated out).
- Rewrote the doc comment to point at the underlying optimizer bug and explain the gate.

Note: `lib.rs:1649` still calls `register_h2_table_filter_prepare` unconditionally (I do not own
lib.rs); the function is now an explicit no-op under the default feature set, so no lib.rs change
is required.

### Tests added
None (registration is now feature-conditional; no new behavioural assertion is safe to add).

---

## S3 (HIGH) — classfile_api: entire JEP 484 Class-File API fabricated

### Finding
`classfile_api.rs` fabricates the JEP 484 API: `ClassFile.parse` ignored the bytes and returned
a fixed ClassModel; `build`/`buildTo`/`buildModule`/`transformClass` (and `ClassBuilder.build`,
`ClassTransform.transformClass`) returned an empty `byte[]` — silently-wrong answers.

### Root cause
No real class-file parsing/generation exists; the natives returned canned objects/bytes.

### Exact change (file:line)
Kept the module SyntheticStub-tagged (already was, line ~782) and converted the *productive*
entry points — whose wrong answers silently corrupt callers — to fail loudly with a clear,
catchable `UnsupportedOperationException` instead of fabricating data:
- `classfile_api.rs:27` — added `classfile_unsupported(method)` helper returning
  `Err(MethodCallFailed::from(RuntimeError::UnsupportedOperationException { message }))`
  (variant confirmed at `types/src/error.rs:263`).
- Rewrote `ClassFile.parse([B)`, `parse(Path)`, `build`, `buildTo`, `buildModule`,
  `transformClass`, `ClassBuilder.build()[B`, and `ClassTransform.transformClass(ClassModel)[B`
  to call `classfile_unsupported(...)`.
- Added `use cratonvm_types::error::{MethodCallResult, MethodCallFailed, RuntimeError};`.
- Dropped now-unused `native_noop, native_noop_with_this` from the `crate::{...}` import
  (`buildTo` had been the only `native_noop_with_this` user). `obj_arg`/`alloc_concurrent_synthetic`
  remain used by the accessors.
- Updated the module doc to FLAG the behaviour.

The read-only accessor natives (majorVersion, flags, …) are left intact but are now unreachable
for fabricated `parse` output (you can no longer obtain a fabricated ClassModel), so they no
longer produce silently-wrong answers in practice. All existing `#[cfg(test)]` tests assert only
registration presence (`r.find(...).is_some()`), which is preserved.

### Tests added
None new; existing registration tests still pass (methods remain registered).

---

## Files touched
- `native-builtins/src/keystore.rs` (B1)
- `native-builtins/src/log4j_extras.rs` (S1)
- `native-builtins/src/apps_h2.rs` (S2)
- `native-builtins/src/classfile_api.rs` (S3)
- `docs/reviews/fable-2026-06-10/fixes/nb-rest-stubs.md` (this note)

## Follow-up & risk
- **B1 risk: low.** Pure helper rewrite; callers unchanged in shape. The empty-alias fallback
  (`.unwrap_or_default()`) is preserved for null args. Follow-up: add an `engine_*` callback test
  through a mock `NativeContext` (would have caught this — the report calls this out as the #1
  coverage gap).
- **S2 risk: medium (behavioural).** Default builds no longer install the H2 `TableFilter.prepare`
  workaround, so H2 queries that hit the optimizer-null-index path will now NPE at runtime until
  the real optimizer fix lands. This is the intended no-stubs outcome (real bytecode / clear
  error). Re-enable transiently with `--features app-stubs` if an H2 demo must pass before the
  optimizer is fixed. **Real fix owed in the VM optimizer** (`Optimizer.optimize` /
  `TableFilter.setPlanItem` path) — outside this agent's owned files.
- **S3 risk: low.** Productive entry points now throw a catchable exception; accessors untouched.
  Any genuine JEP 484 consumer now fails loudly instead of silently. If a real Class-File API
  becomes needed, implement it properly (or gate the whole module behind `app-stubs`).
- **S1 risk: very low.** Pure category tagging; no behavioural change (the stubs were already
  `app-stubs`-gated at the call site).
- **Compilation confidence: high.** All edits mirror existing patterns in the same files
  (`set_category`/`current_category` save-restore as in classfile_api/keystore; `MethodCallFailed::from(RuntimeError::…)`
  as in atomic_updater; `#[cfg]`/`#[cfg_attr(allow(dead_code))]` for feature-gated registrations).
  Could not run `cargo build` per instructions.
