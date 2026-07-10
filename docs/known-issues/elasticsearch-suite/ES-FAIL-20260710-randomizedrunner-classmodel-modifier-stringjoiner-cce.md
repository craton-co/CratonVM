# ES suite-wide bootstrap failure: `ClassCastException` in `Modifier.toString`/`StringJoiner.add` blocks every `RandomizedRunner`-based test class

Status: OPEN — bisected, not fixed (out of scope for the fix that surfaced it)

Found: 2026-07-10, while verifying
`docs/known-issues/elasticsearch-suite/ES-FAIL-20260710-executors-factory-synthetic-mainlock-npe-FIXED.md`
against a full ES suite run. Confirmed **unrelated** to that fix (reproduces identically with and
without it — see bisection below).

## Symptom

Every ES test class using `com.carrotsearch.randomizedtesting.RandomizedRunner` (i.e. essentially
the entire suite — confirmed across `client/rest`, `action.admin.cluster.storedscripts`, and other
unrelated modules in a 60-class spot check) now fails immediately with `initializationError`,
before any test method runs:

```text
java.lang.ClassCastException: java.util.ArrayList cannot be cast to [Ljava.lang.String;
	at java.util.StringJoiner.add(StringJoiner.java:191)
	at java.lang.reflect.Modifier.toString(Modifier.java:233)
	at java.lang.reflect.Field.toGenericString(Field.java:381)
	at com.carrotsearch.randomizedtesting.ClassModel$2.compare(ClassModel.java:34)
	at com.carrotsearch.randomizedtesting.ClassModel$2.compare(ClassModel.java:31)
	at java.util.TimSort.countRunAndMakeAscending(TimSort.java:355)
	at java.util.TimSort.sort(TimSort.java:220)
	at java.util.Arrays.sort(Arrays.java:1234)
	at com.carrotsearch.randomizedtesting.ClassModel$4.members(ClassModel.java:232)
	at com.carrotsearch.randomizedtesting.ClassModel$ModelBuilder.build(ClassModel.java:85)
	at com.carrotsearch.randomizedtesting.ClassModel.fieldsModel(ClassModel.java:240)
	at com.carrotsearch.randomizedtesting.ClassModel.<init>(ClassModel.java:208)
	at com.carrotsearch.randomizedtesting.RandomizedRunner.<init>(RandomizedRunner.java:321)
```

`RandomizedRunner`'s `ClassModel` builds a sorted list of a test class's declared fields for
deterministic-order iteration, calling `Field.toGenericString()` → `Modifier.toString(int)` for
each field. `Modifier.toString(int)` builds its result with a `StringJoiner`, whose `add(CharSequence)`
does an internal cast that's failing here — something in CratonVM's `Modifier.toString`/
`StringJoiner` interaction is handing it a `java.util.ArrayList` where a `String[]` (or a
`CharSequence` it expects to treat as one) is expected.

Not investigated further in this session (out of scope), but candidate causes given the bisection
below: something in `aa21e334`'s interpreter/JIT changes altered field-modifier or varargs/array
handling in a way that surfaces here specifically because `Modifier.toString` is a JDK-25 method
built on `StringJoiner` (a class CratonVM has its own synthetic/bridge history with — see
`native-api/src/registry.rs`'s `drop_real_layout_synthetic` `StringJoiner` clause and comment).

## Bisection

Confirmed via rebuild + rerun of the exact same test class (`ScriptMethodInfoSerializingTests` /
`ScriptContextInfoSerializingTests`) at successive dev commits, holding the ES checkout and test
seed fixed:

- dev `4b08ffad` (immediately before `aa21e334`): class initializes fine, all 8 tests run (4 fail on
  the *unrelated*, separately-fixed mainLock NPE this session's main fix addresses).
- dev `aa21e334` ("Fix Spring SpEL evaluation edge cases", touches `vm/src/runtime/interpreter.rs`
  and `jit/src/x64.rs`) onward, through current dev tip (`8edd57be` at time of writing): every run
  hits the `ClassCastException` above instead, `initializationError`, 0 of the class's actual test
  methods ever execute.

This makes the entire ES suite currently unusable for suite-level pass/fail verification on dev —
any doc claiming an ES suite-wide PASS/FAIL count from a run against current dev tip should be
treated with suspicion until this is fixed or the affected run predates it.

## Reproduction

```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category all -Jit on -Vm craton -ElasticsearchRoot "<es-checkout>" -Exe <cratonvm-binary-built-from-dev-tip> -JdkHome /usr/lib/jvm/java-21-openjdk-amd64 -TimeoutSec 60 -RunName repro-cce -ModeName repro-cce -Start 1 -Count 1
```

Any class works — this is not specific to any one module or class. `-Start 1 -Count 6` (client/rest
module) and `-Start 332 -Count 1` (`ScriptMethodInfoSerializingTests`) both reproduce it identically.

Collection context:
- Host: `victor@20.83.144.174`
- Worktree: `/data/data/cratonvm-worktrees/20260710-132130-es-executors-factory-real-init`
- ES checkout: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch`
- Bisection binaries: `cratonvm-pre-spel-4b08ffad` (no bug) vs. `cratonvm-baseline-dev-aa21e334` (bug
  present) under `/data/data/cratonvm-targets/es-executors-factory-real-init-20260710/release/`
