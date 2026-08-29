# L5 — reflection and class metadata: 207 rows — **COMPLETE 2026-08-28**

**Read `HANDOFF-20260828-SCOPE.md` first.**

> **OWNER: this session. Dispatch worklist COMPLETE** — 483 probed rows, 20
> defects fixed, both modes. The two items it left open are being closed now;
> see "The residuals" below. Full record:
> `L5-reflection-lane-complete-20260828.md`. The lane is free for anyone who
> wants to extend it into the long tail; nothing here is still in flight.
> worktree `C:\craton\cratonvm\.claude\worktrees\h2-known-issues-206dee`
> branch `claude/jdk-only-mode-handoff-09b48c`
>
> `native-builtins/src/lang_class.rs` is FREE again — the collision warning that
> stood here while the lane was in flight no longer applies.

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

## The residuals — IN PROGRESS, same owner

The two items this record listed as "real but not blocking" are being closed by
the same session, in `L5-residuals-module-packages-and-invokeexact-20260828.md`.
`probes/L5ModuleInvokeSweep.java` (125 rows) is the instrument.

* **`Module.getPackages()` — DONE.** All 15 rows clean in both modes. The
  63-entry hand list shadowed the VM's own `ModuleRegistry`, which already
  answered >100 through `getDescriptor().packages()` on the same VM. The probe
  also found that all SEVEN `Module`/`ModuleDescriptor` collection accessors
  returned MUTABLE sets — not in any record, found only because a row asked.
* **`invokeExact` — in flight.** 17 rows, and the gap is wider than the record
  said: arity was unchecked too (`max.invokeExact(1)` answered `1` on a
  two-argument handle). Plus one plain wrong VALUE on the `invoke` side —
  `(long) max.invoke(1, 2)` is `2` on HotSpot and `0` here.

**These touch `native-builtins/src/lang_invoke.rs`, `jboss_jdkspecific.rs`,
`vm/src/vm/vm_exec.rs` and `vm/src/runtime/interpreter/invoke.rs`.** If your
lane needs any of those, say so before editing.

The long tail (71 classes with ≤3 rows each) is unowned.
