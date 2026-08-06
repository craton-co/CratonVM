# In-VM `javac` intermittently dies in `Modules.setupAllModules`

**Status:** OPEN, **not reproduced**. Seen 3 times in ~10 runs on 2026-08-05,
then 0 times in 79 deliberate attempts the same day. Filed so the observation is
not lost, and so the next person does not repeat the attempts below.

## What was seen

```
cratonvm --real-jdk --java-home /data/data/jdk25-real \
  -cp /data/data/jdk25-real/lib/jrt-fs.jar com.sun.tools.javac.Main \
  -nowarn -d <outdir> @<file-list>
```

exits **4** (javac's `EXIT_ABNORMAL` — an unexpected `Throwable` escaped),
writes zero class files, and takes ~6 s against the ~60 s a successful run of
the same corpus takes, i.e. it dies during module setup before compiling
anything. javac's crash handler fires (`printing javac parameters to:
/tmp/javac.<ts>.args`). The tail of the stack:

```
com.sun.tools.javac.code.Symbol$ClassSymbol.complete(Symbol.java:1472)
com.sun.tools.javac.comp.Modules$1.complete(Modules.java:652)
com.sun.tools.javac.code.Symtab.lambda$enterModule$0(Symtab.java:865)
com.sun.tools.javac.code.Symbol.complete(Symbol.java:703)
com.sun.tools.javac.comp.Modules.lambda$setupAllModules$2(Modules.java:1285)
com.sun.tools.javac.comp.Modules.setupAllModules(Modules.java:1309)
com.sun.tools.javac.comp.Modules.initModules(Modules.java:239)
com.sun.tools.javac.main.JavaCompiler.initModules(JavaCompiler.java:1047)
... JavaCompiler.compile / Main.compile / Main.main
```

**The head of the exception was never captured** — the observation was made in
passing during an unrelated A/B and the `grep` used at the time cut it off. That
is the first thing the next attempt needs.

## What has been ruled out

79 runs, all clean, on the Azure Linux host, corpus = 60 generated single-class
files (`ls /data/tmp/jm/src`, regenerable — 60 classes × 40 trivial methods,
ASCII only):

| arm | shape | runs | failures |
|---|---|---:|---:|
| `/data/data/bisect-bins/cvm-b1.bin` | sequential | 12 | 0 |
| `dev` tip (`2ed2d65d4`) | sequential | 14 | 0 |
| `dev` tip | 5-way parallel × 4 rounds | 20 | 0 |
| `dev` + `String.hashCode` re-registered `Intrinsic` | sequential | 15 | 0 |
| `dev` + `String.hashCode` `Intrinsic` | 5-way parallel × 4 rounds | 20 | 0 |

Two hypotheses were tested and neither held:

* **Host load.** The original failures happened with load average ~30 and ~4 GB
  available; the 5-way parallel batches reproduce that pressure (available RAM
  driven from 12 GB to 6 GB) and stayed clean.
* **The `String.hashCode` `Intrinsic` registration.** All three original
  failures were on the one binary of three that had it, which made it the only
  configuration difference on the table — and the other two binaries succeeded
  immediately afterwards. Rebuilt that exact configuration on top of `dev`: 35
  runs, 0 failures. **The correlation was three samples and did not survive.**

The one thing not reproduced is the original environment itself: the failures
came in a run of three consecutive attempts using output directory
`/data/tmp/jout2`, and every attempt after switching to a different output
directory succeeded. That is almost certainly coincidence — the directory was
`rm -rf`'d and recreated before each of the three failures — but it is the only
uncontrolled variable left, and it is recorded rather than dismissed.

## Where to look when it does reproduce

`Modules$1.complete` is the module-symbol completer, and `Symtab.enterModule`
reaches it while `setupAllModules` walks the module graph read out of
`lib/modules` through `jrt-fs.jar`. So the suspects are the jimage reader, the
`jrt` filesystem provider, and anything that makes the walk order vary between
runs — identity hash codes over `Symbol`s (which do not override `hashCode`,
so their iteration order moves with allocation addresses and therefore with GC
timing) being the obvious source of run-to-run nondeterminism in an otherwise
deterministic compile.

A loop harness is at `/data/tmp/jm/loop.sh` (sequential) and
`/data/tmp/jm/par.sh` (parallel); both keep full stderr per run and delete it
only on success, so the next failure captures itself.

## Related, but not the cause

`native_string_hash_code`'s process-wide hash-slot latch was found while reading
this path and is fixed
([`hash_slot_for`](../../native-builtins/src/lang_string.rs), test
`hash_slot_is_never_guessed_from_an_unreadable_string`). It could produce
exactly this shape of failure — silent `String` corruption decided by whichever
string a process hashes first, which is timing-dependent — but **nothing
connects it to these three failures**, and it is not reachable through the
registration in either shipped mode. It is listed here so the connection is not
made later by assumption; if this flake reproduces on a binary that already has
that fix, the two are definitively unrelated.
