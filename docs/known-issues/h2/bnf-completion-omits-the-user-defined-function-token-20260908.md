# `TestBnf` — the BNF completion list omits the user-defined function, though the procedure metadata is there

| | |
|---|---|
| **Status** | OPEN. A CratonVM differential, confirmed against HotSpot on the same fixture and classpath. |
| **Severity** | Low-moderate — one H2 class fails. No crash, no wrong answer outside the autocompletion API. |
| **Opened** | 2026-09-08 |
| **Not JIT** | Reproduces under `--nojit`, so nothing in this family is involved. |
| **How it surfaced** | It did not fail before 2026-09-07 because the class **aborted earlier**: `TestBnf` was one of the 8 `precise deoptimization unavailable` CRASH classes. With that fixed the class runs on, and this is what it runs into. |

## The failure

`org.h2.test.unit.TestBnf.testProcedures`, `TestBnf.java:138`:

```java
tokens = bnf.getNextTokenList("SELECT CUSTOM_PR");
assertTrue(tokens.values().contains("INT"));      // <- fails on CratonVM
```

`CUSTOM_PRINT` is a user-defined function registered by the test with
`CREATE ALIAS`; the assertion is that typing `SELECT CUSTOM_PR` offers `INT` as
the completion.

## The differential, narrowed to one map

`probes`-style reproducer (six lines of H2 API, no test framework), run on both
VMs with the same classpath and JDK 25.0.4:

| | HotSpot | CratonVM (`--nojit`) |
|---|---|---|
| `contents.getDefaultSchema().getProcedures()` | one `DbProcedure` | one `DbProcedure` |
| `bnf.getNextTokenList("SELECT CUSTOM_PR").size()` | **4** | **3** |
| the map | `{1#.=., 1#character=A, 1#digit=1, 2#CUSTOM_PRINT=INT}` | `{1#.=., 1#character=A, 1#digit=1}` |
| `values().contains("INT")` | `true` | `false` |

**The procedure metadata is present on both.** `DbContents.readContents` finds
`CUSTOM_PRINT` under CratonVM — the test's own earlier assertion at line 120
(`procedureName.contains("CUSTOM_PRINT")`) passes, and the probe prints a
`DbProcedure` on both VMs. So this is not a JDBC-metadata gap.

What is missing is exactly one entry: the completion the
`DbContextRule(contents, DbContextRule.PROCEDURE)` topic is supposed to
contribute. The three generic tokens (`.`, a character class, a digit class) are
identical on both, so the BNF engine itself is producing a list — it just has no
procedure in it.

## Where to look

`org.h2.bnf.context.DbContextRule.addNextTokenList` for `PROCEDURE`, and how it
matches the typed prefix against `DbProcedure.getName()`. The narrowing above
says the input to that rule is right and its output is empty, so the fault is
between them — a name comparison, a case fold, or an iteration over a collection
that reads as empty. `2#CUSTOM_PRINT=INT` is the entry to conjure; the `2#` rank
and the `INT` remainder say the rule matched the first 8 characters and offered
the rest.

## Reproducing

```java
DbContents contents = new DbContents();
contents.readContents("jdbc:h2:mem:bnfprobe", conn);       // after CREATE ALIAS CUSTOM_PRINT
Bnf bnf = Bnf.getInstance(null);
bnf.updateTopic("column_name", new DbContextRule(contents, DbContextRule.COLUMN));
bnf.updateTopic("user_defined_function_name",
        new DbContextRule(contents, DbContextRule.PROCEDURE));
bnf.linkStatements();
System.out.println(new TreeMap<>(bnf.getNextTokenList("SELECT CUSTOM_PR")));
```

or the whole class: `cratonvm --nojit -c "$H2CP" org.h2.test.unit.TestBnf`.

## And one that is NOT ours, checked at the same time

`org.h2.test.db.TestFunctions` also became visible when the crash was fixed, and
it is **not a CratonVM defect**: it fails identically under **HotSpot 25** on the
same fixture and classpath —

```
AssertionError: Failure
    at org.h2.test.db.TestFunctions.testAnnotationProcessorsOutput(TestFunctions.java:1898)
```

— same assertion, same line, `rc=1` on both. It belongs in the H2 suite's
expected-failure list, not in a VM bug report. Recorded here because the two
classes surfaced together and the pair is exactly why an oracle run is not
optional: without it this page would have carried two defects, one of them
imaginary.
