# TYPES-ERASURE.1 consolidation hypothesis — TESTED, REFUTED

**Status: hypothesis tested and refuted. All 7 pre-existing javac-family bans (`SPRING-TESTCOMPILER.1-3`, the two unnamed `ClassFinder.fillIn`/`ClassReader.readInnerClasses`/`readAttrs` bans, `HIB-STOREDPROC-JIT.1`) remain necessary and unchanged.**

## Background

`vm/src/jit/skip_list.rs`'s own comment next to `TYPES-ERASURE.1` (added
2026-07-25) and memory
[[types-erasure-javac-jit-family-fix-20260726]] flagged an untested,
high-leverage hypothesis: since `com.sun.tools.javac.code.Types.erasure`
is called constantly during symbol/type completion — including from
inside the other 7 already-banned javac-family methods — perhaps banning
`erasure` alone makes those 7 individually-targeted bans redundant,
collapsing 8 bans into 1.

## Test performed

Built a binary with all 7 other javac-family bans temporarily disabled
(`JavacTool.getTask`, `ClassReader.readClass`, `ClassFinder.complete`,
`ClassFinder.fillIn`, `ClassReader.readInnerClasses`,
`ClassReader.readAttrs`, `Symbol$ClassSymbol.complete`), leaving
`Types.erasure` as the only active ban in the family. Ran
`JavacConsolidationProbe.java` (committed at
`docs/known-issues/repros/jitban-remaining-20260726/`) — 200 varied
in-process `ToolProvider.getSystemJavaCompiler()` compilations per run
(plain classes, inner classes, `@Deprecated` methods without an explicit
annotation value, generics/collections — deliberately excluding
`@SuppressWarnings("...")`, which hits a separate, already-documented,
unrelated bug: `suppresswarnings-annotation-duplicate-value-bug-20260726.md`).

Baseline (all 8 bans active): 100/100 compilations succeed, 0 failures.

With only `Types.erasure` banned (the other 7 lifted): **failed at
iteration 6**, reproducing `SPRING-TESTCOMPILER.2`'s exact originally-
documented symptom verbatim:

```
java.lang.NullPointerException: Cannot read field "kind" because "sym" is null
	at com.sun.tools.javac.code.Symbol.packge(Symbol.java:538)
	at com.sun.tools.javac.jvm.ClassReader.readClass(ClassReader.java:2887)
	at com.sun.tools.javac.jvm.ClassReader.readClassBuffer(ClassReader.java:3036)
	at com.sun.tools.javac.jvm.ClassReader.readClassFile(ClassReader.java:3060)
	at com.sun.tools.javac.code.ClassFinder.fillIn(ClassFinder.java:373)
	...
	at com.sun.tools.javac.comp.Modules.setupAllModules(Modules.java:1232)
	...
```

This is precisely the `ClassReader.readClass`/`Symbol.packge` miscompile
`SPRING-TESTCOMPILER.2`'s ban documents — it reproduces the moment
`ClassReader.readClass` is un-banned, regardless of `Types.erasure`
staying banned. Three more failures followed within the next 3
iterations (kinds 3, 0, 1 — i.e. not limited to one particular source
shape), confirming this isn't a fluke.

## Conclusion

`Types.erasure` and `ClassReader.readClass` (and, by strong inference,
the other bans in this family, each independently bisected to their own
exact method in their own original investigation) are **separate,
independent JIT miscompiles that happen to share a call-graph
neighborhood**, not one root cause with seven redundant symptoms. All 7
bans remain necessary. The `if false && ...` temporary test edits used
for this experiment were fully reverted (`git checkout --
vm/src/jit/skip_list.rs`) — no functional code change resulted from this
investigation, only this documentation and the probe files.

## Why the hypothesis was plausible but wrong

`erasure` is indeed called from deep inside `readClass`'s own call chain
(both are part of "resolve a class file's symbols/types" machinery), so
it's easy to imagine one bad JIT'd function corrupting shared state that
both then observe. But the actual failure mode is a single incorrect
compiled body for `readClass` itself producing `sym == null` — banning
`erasure` (a different, separately-compiled function) does nothing to
change how `readClass`'s own body gets JIT'd. Each of these 8 bans
guards a genuinely distinct x64 lowering defect in its own named method;
the "javac-family" grouping in the comments describes a shared *trigger
scenario* (repeated in-process compilation), not a shared *root cause*.

## Recommendation

Do not re-attempt this consolidation without a specific new lead (e.g. an
actual shared code pattern identified via disassembly showing the same
lowering bug in both `erasure` and `readClass`'s compiled bodies). Treat
all 8 bans in this family as independent going forward. If any of them
individually needs re-verification, test it in isolation with the other
7 (including `Types.erasure`) left active, not as a group.
