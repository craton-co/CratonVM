# L5 — reflection and class metadata: 207 rows — **COMPLETE 2026-08-29**

**Read `HANDOFF-20260828-SCOPE.md` first.**

> **OWNER: this session. LANE FULLY COMPLETE, residuals included.**
>
> * dispatch worklist — 483 probed rows, 20 defects, all 207 `native-won`
>   triples covered (`L5-reflection-lane-complete-20260828.md`);
> * the two items that record left open, plus five more the probe found —
>   125 rows, 1 differing, both modes
>   (`L5-residuals-module-packages-and-invokeexact-20260828.md`).
>
> **28 defects across 608 probed rows.** Nothing is in flight; every file this
> lane held is released. The long tail below is unowned and is the natural
> extension.
>
> worktree `C:\craton\cratonvm\.claude\worktrees\h2-known-issues-206dee`
> branch `claude/jdk-only-mode-handoff-09b48c`

## Families

```text
java/lang/Class              67 bridge-with-code rows
java/lang/reflect/Field      32
java/lang/ClassLoader        29
java/lang/System$1           29   (the JavaLangAccess impl — reached INDIRECTLY)
java/lang/reflect/Method     26
java/lang/Module             24
                            ---
                            207   (9%)
```

Registrars: `native-builtins/src/lang_class.rs` (28 231 lines) and
`native-builtins/src/lib.rs`, plus `phases_late/reflect_invoke.rs` for `Module`.

## Result

| probe | rows | compat | `--jdk-only` |
| --- | ---: | --- | --- |
| `ClassShadowSweep` | 261 | 0 diffs | 0 diffs |
| `FieldMethodShadowSweep` | 132 | 0 diffs | 0 diffs |
| `ClassLoaderShadowSweep` | 90 | 0 diffs | 0 diffs |

All 207 `native-won` triples covered. **20 defects, all fixed, no residuals.**
The full account — including two items first recorded OPEN and then fixed, and
two wrong placements of one guard — is in
`L5-reflection-lane-complete-20260828.md`.

## The two findings most useful to other lanes

**`Field`/`Method` came back 132/132 with one defect.** Every widening and
narrowing rule holds in both directions, every `invoke` edge holds, all the
metadata holds. A clean family is a result: it said this lane's work was in the
LOADING path, not the accessor path, and the next 90 rows confirmed it. Do not
skip a family because you expect it to be clean, and do not keep digging in one
that measured clean.

**Two items were deferred for reasons a single lookup refuted.** `Module.canUse`
was recorded as needing a Java callback into the native written to avoid a null
`descriptor` field — but `ctx.module_uses()` already exposes the VM's own module
registry. The duplicate-`defineClass` error type was recorded as needing a new
`LinkageError` variant — but `DuplicateClassDefinition` already existed, mapping
to `java/lang/LinkageError` with HotSpot's wording. **Before recording something
as too expensive, check the API you are assuming you lack.**

## The residuals — DONE, same owner

The two items this record listed as "real but not blocking" are being closed by
the same session, in `L5-residuals-module-packages-and-invokeexact-20260828.md`.
`probes/L5ModuleInvokeSweep.java` (125 rows) is the instrument.

* **`Module.getPackages()` — DONE.** All 15 rows clean in both modes. The
  63-entry hand list shadowed the VM's own `ModuleRegistry`, which already
  answered >100 through `getDescriptor().packages()` on the same VM. The probe
  also found that all SEVEN `Module`/`ModuleDescriptor` collection accessors
  returned MUTABLE sets — not in any record, found only because a row asked.
* **`invokeExact` — DONE.** All 17 rows, both modes. The gap was wider than the
  record said: arity was unchecked too (`max.invokeExact(1)` answered `1` on a
  two-argument handle, and `(1,2,3)` answered `2`). The recorded objection —
  a strict check killed every Groovy `IndyInterface` call site — was against
  `MH_DESC`, which adapters do not maintain; the check reads `type`, which they
  do. `RJdkHandles` (40 rows, the whole adapter family) passes with it on.
* **Three more the probe found that no record mentions:** a retyped handle lost
  its return VALUE (`(long) max.invoke(1, 2)` was `0`, HotSpot `2`), an unbound
  virtual handle answered `0` for a null receiver instead of NPE, and `invoke`
  accepted a narrowing primitive argument.

**Final: 125 rows, 1 differing, both modes.** The remaining row is `invoke`'s
REFERENCE-argument cast, left open with its reason and a pointer to the
measured table whoever takes it will need.

**These touched `native-builtins/src/lang_invoke.rs`, `jboss_jdkspecific.rs`,
`vm/src/vm/vm_exec.rs` and `vm/src/runtime/interpreter/invoke.rs`** — all
released now.

The long tail (71 classes with ≤3 rows each) is unowned.
