# P4-B: `--synthetic-jdk` mode, run for the first time — it boots, and 53 missing natives now have names

**Status: MEASURED 2026-08-29** on `azure-host-2` (`azureuser@20.80.105.49`),
worktree `/data/cvm-l7dod-20260828` at `9a83d6ebf`, binary
`/data/l7syn-target/release/cratonvm` built
`cargo build --release -p cratonvm-cli --features synthetic-jdk`.

Phase 4's P4-B, from `docs/feature-designs/jdk-only-completion-roadmap.md` §4:

> **the `--features synthetic-jdk` binary has never been run in that mode, and
> no `RJdk*` vector ever has.** … There is no "run it once" — it needs its own
> build of its own binary.

Both halves are now false, and the reason neither had happened turns out to be
smaller than the roadmap assumed.

---

## 1. The arm did not COMPILE, and it was one symbol

The roadmap frames P4-B as expensive because it needs a separate build. The
build is 7 minutes. What actually stood in the way is that nothing had compiled
the feature in long enough for it to rot:

```text
cargo check -p cratonvm-native-builtins --features synthetic-jdk --lib
native-builtins/src/util_time.rs:5575: error[E0425]: cannot find value `DAYS_IN_MONTH`
native-builtins/src/util_time.rs:5589: error[E0425]: ...
native-builtins/src/util_time.rs:5603: error[E0425]: ...
```

Three uses, no definition, in `Month.length(boolean)` / `maxLength()` /
`minLength()` — synthetic-mode-only natives, so the arm nobody builds is the
only one that ever sees them. Fixed through `civil_date`'s existing table rather
than a fourth copy of it; after that, `cargo check -p cratonvm-cli --features
synthetic-jdk` is clean and the release build succeeds.

**The lesson is about the roadmap entry, not the bug.** "It needs its own build"
was read as a cost and used as a reason to defer. The cost is one `cargo check`,
and paying it once would have caught this the day it landed. A feature arm with
no CI step is a feature arm that stops compiling.

## 2. It boots, and it runs ordinary code

```text
cratonvm --synthetic-jdk --Xmx 1g -cp . SynHello
SYN sb=0,1,2,3,4,
SYN list=[x, y] size=2
SYN map.get=7
SYN math=4 str=abc
SYN RESULT OK
[cratonvm] main-vm run() returned Ok — VM main exiting normally
```

`StringBuilder`, `ArrayList`, `HashMap`, `Math`, `String` — all from the
synthetic class library, no `--java-home`, exit 0.

`probes/DodArrayStoreSweep` — 62 rows of array-store, `toArray` and covariance
cases — also runs to completion with `DOD RESULT OK`. One difference from the
other two modes, and it is a real finding rather than a probe artefact: every
`ArrayStoreException` carries a `null` message, because the synthetic library
has no `java/lang/ArrayStoreException.<init>(String)`. The VM's own store checks
are right; the exception it constructs cannot carry their text.

## 3. The `RJdk*` corpus: 49 vectors, 1 pass, 0 hangs

```text
SYNRUN-TOTALS pass=1 fail=48 timeout=0
```

`RJdkLookupIn` passes. Every other vector **terminated with a named error** —
no hangs, one abort (`RJdkStringCodePoints`, rc=134).

**1 of 49 is not a scandal and should not be read as one.** The `RJdk*` corpus
is the JDK-ONLY corpus: those vectors exist to assert that real JDK bytecode is
what runs, which is the opposite of what synthetic mode promises. The number
worth having is not the pass rate, it is the taxonomy and the gap list.

```text
first error, by kind          missing natives, distinct   53
  AssertionError        20    spread over java.util.concurrent, java.util,
  NoSuchMethodError     15    java.util.logging, java.lang.invoke,
  NoSuchFieldError       4    java.lang, sun.security.x509
  NullPointerException   3
  RuntimeException       1    every one carries its caller and bytecode offset:
  NoSuchMethodException  1      java/util/Vector.elements()Ljava/util/Enumeration;
  IllegalMonitorState    1        caller=RJdkEnumerations.properties()V @pc=117
  ClassFormatError       1      java/util/concurrent/Phaser.bulkRegister(I)I
  ClassCastException     1        caller=RJdkPhaser.registration()V @pc=42
```

The **AssertionError** bucket is the interesting one: 20 vectors got far enough
to run their own assertions and disagree with them, rather than dying on a
missing method. Those are behavioural differences in the synthetic library, and
they are the population a synthetic-mode lane would work.

The **NoSuchMethodError / NoSuchFieldError** bucket (19) is the shopping list:
53 named methods, each with the vector and `@pc` that reached it. That is the
artefact this run exists to produce, and it did not exist before today.

## 4. A diagnostic that named the wrong mode

Every one of those 53 lines read:

```text
WARN Missing native method in real-JDK mode method=java/util/Vector.elements()...
```

while the VM was in synthetic mode. It is the only diagnostic that says which
configuration produced the gap, and in the one configuration whose gaps had
never been catalogued it named the other one. A reader's first move on seeing
that is to conclude the run was misconfigured and stop — which may be part of
why this catalogue is new. It now reports the mode it is actually in
(`jdk-only` / `synthetic-JDK` / `real-JDK`), with the mode as its own field.

## 5. What this does and does not license

**Does:** P4-B's two standing claims are discharged. A residual that lives only
in synthetic mode can now be adjudicated — the roadmap's stated consequence of
this lane. The gap list is concrete and per-caller.

**Does not:** this is not a synthetic-mode correctness campaign, and 1/49 is not
a baseline anyone should quote as a quality number without reading §3. Nothing
here changes shipped behaviour: the `synthetic-jdk` feature is not in any
default build, and a shipping binary still refuses `--synthetic-jdk` with the
same message as before.

## Reproduce

```bash
source /data/toolchain/env.sh
cd <worktree>
cargo build --release -p cratonvm-cli --features synthetic-jdk -j6
S=target/release/cratonvm

# SynHello is twelve lines and is inlined here rather than cited: `probes/` was
# deleted from the tree by `3b2901531`, and a reproduce block should not point
# at a file the reader has to restore.
cat > SynHello.java <<'EOF'
public final class SynHello {
    public static void main(String[] a) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 5; i++) { sb.append(i).append(","); }
        System.out.println("SYN sb=" + sb);
        java.util.List<String> l = new java.util.ArrayList<>();
        l.add("x"); l.add("y");
        System.out.println("SYN list=" + l + " size=" + l.size());
        java.util.Map<String,Integer> m = new java.util.HashMap<>();
        m.put("k", 7);
        System.out.println("SYN map.get=" + m.get("k"));
        System.out.println("SYN math=" + Math.max(3, 4) + " str=" + "AbC".toLowerCase());
        System.out.println("SYN RESULT OK");
    }
}
EOF
javac -d . SynHello.java
$S --synthetic-jdk --Xmx 1g -cp . SynHello

for f in regression-suite/src/RJdk*.java; do
  v=$(basename "$f" .java)
  timeout 120 "$S" --synthetic-jdk --Xmx 1g -cp regression-suite/build "$v" \
      > out/$v.out 2> out/$v.err
done
# the gap list (the ANSI escapes must come off first, or `method=` will not match)
sed -e 's/\x1b\[[0-9;]*m//g' out/*.err | grep -o 'method=.*' | sed 's/method=//' | sort -u
```

`regression-suite/build` must already be compiled — the runner script does that
in real-JDK mode, and synthetic mode reuses the same classes.
