# L5 — reflection and class metadata: 207 rows — **COMPLETE 2026-08-28**

**Read `HANDOFF-20260828-SCOPE.md` first.**

> **OWNER: this session. LANE COMPLETE** — 483 probed rows, 20 defects fixed,
> **0 residuals**, both modes. Full record:
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

## If you extend this lane

`java/lang/Module.getPackages()` for `java.base` is still short (`<100`), and
`MethodHandle.invokeExact` still does not enforce its exact signature — both
recorded in `primitive-class-had-a-loader-and-two-deeper-gaps-20260827.md` §3
and §4. Neither is a `native-won` triple in this lane's 207, so neither blocked
completion; both are real.

The long tail (71 classes with ≤3 rows each) is unowned.
