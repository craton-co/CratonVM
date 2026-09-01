# `--diff-hotspot`: point the launcher at your own program

*Scope: the `--diff-hotspot` mode of the `cratonvm` launcher
(`vm-cli/src/main.rs`, module `diff_hotspot`). Sibling of
[`docs/testing/differential.md`](differential.md), which covers the `difftest`
crate — the corpus-and-fuzzer door. This page is the single-program door.*

---

## 1. Why this exists

An independent audit on 2026-09-01 ran **273 differential assertions** against
HotSpot 25 — arithmetic edge cases (`Integer.MIN_VALUE / -1`, shift masking,
NaN / subnormal / `-0.0` bit patterns, `d2f` rounding), `StrictMath` raw bit
results, shortest-round-trip `Double.toString`, `String.format` including
`%g` / `%a` / `%(d`, collections and iteration order, exception messages and
`finally` semantics, reflection, `MethodHandle` / `VarHandle`, NIO buffers, nine
charsets, serialization, zip / deflate / gzip, `java.time` across a DST
boundary, locale formatting, concurrency, and 14 crypto primitives — and found
**zero divergences**, on the first run, with no tuning.

Almost nobody knows that, because reproducing it meant writing a harness. This
mode is that harness, reduced to one flag:

```bash
cratonvm --diff-hotspot -cp build/classes com.example.Main --your --args
```

It is a credibility instrument as much as a debugging one. **The expected result
of pointing it at ordinary code is "no divergence."**

### Which door to use

| You have | Use |
|---|---|
| One program, your own classpath, your own arguments | `cratonvm --diff-hotspot` (this page) |
| A directory of programs, a mode matrix, a committed known-divergence ledger, a CI gate | `cratonvm-difftest run` / `gate` ([`differential.md`](differential.md)) |
| A question about which *execution path* (interpreter fast / decoded / single-pass / IR / OSR / deopt) disagrees | `cratonvm-difftest run --modes …` — this mode runs one configuration, the one you asked for |

The two are deliberately separate. `cratonvm-difftest` drives the `cratonvm`
binary as a subprocess, so the launcher cannot link it back without inverting
the dependency, and a shipped launcher has no business carrying a fuzzer. The
comparison logic here is a small, self-contained re-implementation; where a rule
is shared (`vm-diagnostics` filtering, the nondeterminism maskers, the exit-code
tokens `<timeout>` / `<signal>`) it is spelled the same way on purpose.

---

## 2. What it does

1. Removes its own `--diff-*` tokens from argv.
2. Runs the **same** remaining command line as a child CratonVM process,
   `--diff-runs` times (default 2).
3. Runs the equivalent command line on the reference `java`, once.
4. Compares **stdout**, **stderr** and **exit status**, and reports the **first**
   divergence with the three lines before it.

Both sides are subprocesses. The CratonVM side is a re-exec of the launcher's
own binary rather than an in-process `Vm`, because comparing streams means
owning the pipe, because a re-exec cannot suffer the configuration skew that
`vm/tests/differential.rs` records as a live source of false divergences, and
because a SEGV in the VM under test has to be an *observation* rather than the
end of the comparison.

### The command line

```text
cratonvm --diff-hotspot [DIFF OPTIONS] [VM OPTIONS] -cp <CP> <MainClass> [args...]
cratonvm --diff-hotspot [DIFF OPTIONS] [VM OPTIONS] --jar <FILE.jar> [args...]
```

| Option | Meaning |
|---|---|
| `--diff-hotspot` | Enable the mode. Boots no VM in this process. |
| `--diff-ignore <PATTERN>` | Mask any line containing `PATTERN` on **both** sides. `*` is a wildcard; everything else is a literal substring. Repeatable. |
| `--diff-runs <N>` | CratonVM-side runs used for the self-nondeterminism check. Default `2`. `1` disables the check. |
| `--diff-timeout <SECONDS>` | Per-child wall clock. Default `120`. A CratonVM overrun renders as `<timeout>` on the exit channel, which never compares equal to a clean exit. |
| `--diff-java-arg <ARG>` | An extra argument for the reference `java` only (e.g. `--add-opens=…` the CratonVM side does not need). Repeatable. |
| `--diff-strict` | Treat a difference that only the built-in maskers explain as a failure (exit `1` instead of `0`). |

`--diff-ignore` is a **substring with a `*` wildcard, not a regular
expression.** The launcher links no regex engine, and promising syntax it cannot
honour would be worse than saying so.

Every `--diff-*` token is removed before the launcher's own argv pipeline runs,
so a Java program is still free to take an argument spelled `--diff-ignore`:
like every other launcher option, these are recognised only *before* the program
selector (`--jar <jar>` or the first bare main-class token).

---

## 3. Finding the reference JDK

Exactly the four-step precedence [`docs/CONFIG.md`](../CONFIG.md) documents for
`--java-home`, in this order:

1. `--java-home <PATH>` → `<PATH>/bin/java`
2. `CRATONVM_JAVA_HOME` → `<value>/bin/java`
3. `JAVA_HOME` → `<value>/bin/java`
4. `java` on `PATH`

A step is skipped, and the walk continues, when `<java> -version` fails **or**
when the banner identifies CratonVM. That second rule is not hypothetical: this
repository ships a `java`-named alias binary (`--features java-bin-alias`) and a
Maven shim tree, and `CRATONVM_JAVA_HOME` exists precisely because `JAVA_HOME`
routinely points at one. Without the check, comparing CratonVM against CratonVM
would report a serene and meaningless "no divergence".

If no step yields a usable JDK, the mode exits `2` and prints all four steps
with what each one gave.

The reference JDK and the JDK whose class library the CratonVM side loads are
the *same* installation whenever `--java-home` / `CRATONVM_JAVA_HOME` /
`JAVA_HOME` is set, because both sides read the same variable. That is the point
of following the documented precedence rather than inventing a fifth rule.

---

## 4. Exit codes

| Code | Meaning |
|---|---|
| `0` | No divergence — or a difference that only the built-in nondeterminism maskers explain (see §5), unless `--diff-strict`. |
| `1` | A divergence that survived the maskers, on stdout, stderr or the exit status. |
| `2` | The comparison could not be performed: no usable reference JDK, the CratonVM child could not be spawned, `--synthetic-jdk` was passed, or no program was named. |
| `3` | The CratonVM side did not reproduce **itself** across `--diff-runs`, and nothing outside that instability diverged. No verdict was reached on the unstable lines. |

A CI job can use these directly. `0` is the pass; `2` says "provision a JDK on
this runner", not "the VM is wrong"; `3` says "your program is
nondeterministic", not "the VM is wrong".

---

## 5. Nondeterminism, and not crying wolf

Identity hash codes, `HashMap` iteration order on some shapes, thread names and
interleaving, wall-clock timestamps and absolute paths differ legitimately
between any two JVMs. A differential tool that reports those as bugs gets
switched off, which is strictly worse than not shipping it. Three defences, in
order of strength:

**1. Self-consistency first.** CratonVM is run twice by default. A line that
differs between two CratonVM runs cannot be evidence about HotSpot — it is the
*program* being nondeterministic. Those line indices are excluded from the
verdict on both sides and reported as `unstable`. When two runs disagree on line
*count*, alignment past that point is gone, so everything from the first
disagreement to the end is marked unstable: an over-approximation the report
states rather than hides.

**2. Strict first, relaxed only to explain.** The verdict is byte-exact. Only
when it fails is the pair re-compared with five maskers on; if that makes the
difference vanish, the finding is downgraded to `no semantic divergence`, the
line is printed with the rule that accounts for it, and the exit code is `0`.
`--diff-strict` keeps it a failure.

| Masker | Target | The real divergence it would hide |
|---|---|---|
| `identity-hash` | `@<hex>` with ≥ 4 hex digits and a non-identifier terminator | A meaningful value shaped `name@hexdigits` |
| `hex-address` | `0x<hex>` runs not preceded by an identifier character | A wrong `Integer.toHexString` / `Long.toHexString` result |
| `thread-id` | digits after `Thread-`, `thread-`, `worker-`, `pool-`, `pid=`, `tid=`, `nid=` | A wrong thread *count* or naming scheme |
| `timestamp` | `yyyy-mm-dd` and `hh:mm:ss[.fff]` | A `java.time` formatting bug |
| `absolute-path` | `/`- or `X:\`-rooted paths, reduced to the last segment | A wrong path in an exception message |

Because these run only as a *second opinion*, none of them can silently mask a
strict-equality pass. A run that prints `no divergence` compared byte-for-byte.

**3. `--diff-ignore <PATTERN>`** for the residue only you can name. Masking
replaces the line rather than deleting it, so both sides keep the same indices
and a reported line number still means something.

### Two things are filtered unconditionally

* **Line endings.** CRLF → LF plus a trailing-whitespace trim on both sides. A
  trailing newline or a Windows line terminator is never reported as a
  divergence. The cost: a program whose last byte is *deliberately* a bare `\r`
  or a trailing space cannot be distinguished from one whose last byte is not.
* **CratonVM's own stderr chatter** — lines containing `[cratonvm]`,
  `[NativeBridge]` or a `cratonvm_*` tracing target. This is not a knob: the
  launcher prints `[cratonvm] main-vm run() returned Ok` on every clean exit, so
  without it the stderr channel would diverge on every single run and the mode
  would be useless on its first invocation. The cost: a *program* that itself
  prints one of those three tokens on stderr loses that line from the
  comparison. Same three shapes as the fuzzer's `vm-diagnostics` rule
  (`difftest/src/normalize.rs`).

---

## 6. What is forwarded to the reference side, and what is not

The reference command line is built from the **same** `Args` parse the CratonVM
side performs — `insert_program_args_separator` → `normalize_java_launcher_argv`
→ `extract_system_properties` → `extract_hotspot_flags` → clap — so the two
sides cannot disagree about which token was the main class or where the
program's own arguments began. Disagreeing about that is the classic way a
differential harness blames the VM for its own bug.

**Forwarded** (each can change program-observable behaviour): `-D<k>=<v>` system
properties, `-ea` (any unscoped assertion request), `-Xmx`, `--module-path`,
`--add-reads` / `--add-exports` / `--add-opens` / `--add-modules`,
`--enable-preview`, `--enable-native-access`, the main class (a `/` spelling is
converted to `.`) or `--jar`, and the program's own arguments verbatim.

**Not forwarded**: everything CratonVM-specific, and the `-XX:` / `-agentlib:` /
`-agentpath:` / `-javaagent:` family that `extract_hotspot_flags` removes before
clap. `--diff-java-arg` is the escape hatch.

### Interaction with the other program-selection flags

| Flag | Behaviour | Why |
|---|---|---|
| `--jar <FILE.jar>` | **Works.** Both sides get `-jar <FILE.jar>` and the same program arguments; `-cp` is ignored on both sides. | The launcher already ignores `-cp` when `--jar` is set, and the jar's manifest `Class-Path` is what supplies the classpath. Giving the reference side a stray `-cp` would make the two sides load different code. |
| `--jdk-only` | **Works**, and is meaningful. The CratonVM side runs under the strict policy; the comparison is unchanged. | `--jdk-only` keeps the real JDK class library and only forbids fabricated compatibility classes and synthetic-stub natives. A divergence under it is a real finding — arguably a more interesting one than a divergence under the compatible policy. |
| `--synthetic-jdk` | **Refused**, exit `2`. | The synthetic class library is a deliberately *different* implementation of `java.*` (~5,200 Rust stubs). Every difference it produces against HotSpot is expected, so the comparison would measure nothing while reading as a wall of red. The error message says so and points at `--jdk-only`. |
| `--real-jdk` | Works (it is the default). | Nothing to reconcile. |

---

## 7. A worked example

```bash
$ cat Arith.java
public class Arith {
    public static void main(String[] a) {
        System.out.println(Integer.MIN_VALUE / -1);
        System.out.println(1 << 33);
        System.out.println(Double.toString(0.1 + 0.2));
        System.out.println(Math.copySign(0.0, -0.0));
        System.out.printf("%a%n", 1.0);
    }
}
$ javac -d out Arith.java

$ cratonvm --diff-hotspot -cp out Arith
=== cratonvm --diff-hotspot ===
  program        : Arith
  classpath      : out
  cratonvm       : /data/cvm/target/release/cratonvm
  reference java : /data/toolchain/jdk-25/bin/java  [JAVA_HOME=/data/toolchain/jdk-25]
                   openjdk version "25.0.3" 2026-04-21 LTS
  runs           : cratonvm x2, java x1, timeout 120s

  stdout agree     stderr agree     exit-status agree (0)
  wall           : cratonvm 214 ms, java 96 ms

VERDICT: no divergence. CratonVM and the reference JDK agree on stdout, stderr and exit status.
$ echo $?
0
```

A divergence reports one line, not a diff dump:

```text
VERDICT: DIVERGENCE.
  first divergence: stdout, line 3
          1 | -2147483648
          2 | 2
    cratonvm     3 | 0.30000000000000004
    java         3 | 0.3

  Before filing this: identity hash codes, HashMap iteration order on some shapes,
  timestamps, thread interleaving and absolute paths differ legitimately between any
  two JVMs. The five maskers (identity-hash, hex-address, thread-id, timestamp,
  absolute-path) were applied and the difference survived them, and 2 CratonVM run(s)
  agreed with each other on this line — but a program-level race can still defeat
  both. Re-run with --diff-runs 5 if you are unsure.
```

And a nondeterministic program is reported as such rather than as a bug:

```text
unstable: the CratonVM side did not reproduce itself across 2 runs (1 stdout line(s), 0 stderr line(s)).
          Those lines carry no verdict and are excluded below — a line that differs
          between two CratonVM runs is the *program* being nondeterministic, not a
          HotSpot divergence. Mask them with --diff-ignore for a clean run.
          stdout     4 | elapsed 37 ms

  stdout agree     stderr agree     exit-status agree (0)

VERDICT: no divergence on the comparable output, but the CratonVM side was not
         self-consistent — see the unstable lines above. Exit 3.
```

```bash
$ cratonvm --diff-hotspot --diff-ignore 'elapsed*ms' -cp out Bench   # → exit 0
```

---

## 8. Limits, stated rather than papered over

* **One reference run.** The reference JDK is run once. A HotSpot-side
  nondeterminism is therefore invisible to the stability check and can present
  as a divergence. `difftest`'s corpus gate runs the reference twice for exactly
  this reason (`--check-determinism`); this mode trades that second JVM start
  for latency. If a reported divergence looks like noise, `--diff-runs 5`
  strengthens the CratonVM side and `--diff-ignore` closes the rest.
* **stderr line indices are post-filter.** VM-diagnostic lines are replaced, not
  deleted, so indices are stable — but a reported stderr line number counts
  filtered lines, not raw ones.
* **Exit status is compared, signals are not decoded.** A signal kill renders as
  `<signal>`; the signal number is not part of the comparison.
* **No per-channel exception dimension.** `difftest` splits an uncaught
  exception into presence / type / message / frames and reports each
  independently. Here an uncaught exception is simply stderr text, so a
  divergence names the line rather than the observable. Use
  `cratonvm-difftest run` when that distinction matters.
* **One configuration.** This mode runs the command line you gave, twice. It
  does not fan out across the execution-path modes; that is
  `cratonvm-difftest run --modes …`.
