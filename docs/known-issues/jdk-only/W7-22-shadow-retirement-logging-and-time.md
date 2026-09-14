# W7-22 — the next shadow-retirement increment: logging and date/time

> **§2 AND §3 WERE BOTH TAKEN ON 2026-09-12** as lane 4 wave 6 —
> `docs/known-issues/jdk-only-lanes/lane-4-io-nio-foreign.md` §9.21–§9.26.
> Read this box before trusting anything below it.
>
> * **§2 (`java/io/PrintWriter`, 7 rows) landed as measured.** Re-censused and
>   re-measured on Linux/JDK 25; verdict-neutral, exactly as recorded here.
>   §2.1's landing recipe is spent. The blocker it names in its first line —
>   "the blocker is a platform, not a question", the `<jdk>/<os>`-keyed gates
>   exiting 2 on Windows — was the whole of why this waited a month.
> * **§3 (`java/io/PrintStream`, 29 rows) landed too, and its blocked list had
>   decayed in four of five places.** Items 1 and 2 (`System.out` fabricated;
>   `charset` abstract) were repaired in between by `install_real_stream_fields`
>   in `native-builtins/src/lang_system.rs` — except `closeLock`, which wave 6
>   fixed. Item 3 (`native_printstream_init_outputstream` does not chain) is
>   **still true** and was made irrelevant by retiring BOTH constructors with
>   the methods. Item 4 says `write(String)` is package-private; it is
>   `private`, which did not change its disposition. **Only item 5 survived
>   intact**, and it is the one that says a row must NOT be retired:
>   `write(String,int,int)` is declared by no supported image.
> * **§3's verdict is inverted by the measurement.** The half this document
>   blocked is the half that FIXED things: retiring the constructors took the
>   carrier probe from 24 differing lines against HotSpot to 4, because
>   `charOut`, `textOut`, `charset` and `closeLock` on a user-constructed
>   stream were null.
> * The `native_printstream_init_outputstream` doc comment that claims an
>   `invoke_special` chain describes the `PrintWriter` function below it — a
>   code-move artefact, and the reason item 3 reads as already done to anyone
>   who greps for the chain.
> * §4's own correction box (its named cause is wrong, its repair dead) still
>   stands and is unaffected.

> **RECONCILED 2026-08-12 (W7-55-record-reconciliation.md).** §4's named cause
> is superseded and its prescribed repair is DEAD — see the marker at the head
> of that section, and W7-25-jul-getlogger-regression.md for the mechanism that
> was actually behind it. §4's *observation* stands. The status line below is
> otherwise as filed: the 7-row `java/io/Print*` retirement is genuinely still
> unapplied.

> ## 2026-08-12 — §2's 7-row patch is STILL NOT APPLIED, and the reason has changed
>
> It is no longer "nobody got to it". The patch was re-examined against the
> mechanism W7-25 found — *a retired shadow is silently reinstated by any other
> registrar holding the same triple under a `NativeKind` the retirement is exempt
> from* — and against the three frozen artefacts it moves. Two findings, opposite
> in sign:
>
> **1. The ambient-kind trap does NOT apply here, so the patch would take
> effect.** This is the check W7-25 says to run before believing any retirement,
> and it is run here for the first time. All seven `java/io/PrintWriter` triples
> are registered by `register_printstream_fallback_natives`
> (`native-builtins/src/logging_shims.rs:115`), whose ambient category is
> `NativeKind::Bridge` — set at `logging_shims.rs:118` and restored at
> `logging_shims.rs:419`, with every one of the seven registrations between those
> two lines. The retag in `NativeMethodRegistry::register` fires exactly on an
> effective category of `Bridge`, so it would fire. The only other registrar
> holding any of them is `native-builtins/src/lib.rs:22864-22905`
> (`println(Ljava/lang/String;)V`, `println()V`, and the `print`/`printf`/
> `format`/`flush`/`close`/`write(I)V` neighbours), which sits inside
> `register_synthetic_overrides` under an ambient `Intrinsic` — and that function
> is `#[cfg(feature = "synthetic-jdk")]`, called only from `register_builtins` on
> the `use_synthetic_jdk` arm. **It does not register on either shipping mode**,
> so it cannot reinstate anything under `--jdk-only`. This is the LogRecord
> `<init>` shape checked and found absent, not assumed absent.
>
> **2. It is blocked on three frozen artefacts, and no lane without a Linux
> build can unblock it.** `java/io/Print*` is Compatible-visible (unlike the
> `register_synthetic_overrides`-only registrars, which move nothing), so seven
> rows moving `Bridge` → `SyntheticStub` move: `bridge_shadows_bytecode` in
> `scripts/baselines/jdk-only-bridge-ratchet.json` (6,066 → 6,059),
> `BASELINE_SYNTHETIC_STUBS` in `native-builtins/tests/stub_ratchet.rs`, whose
> `SLACK` is **0**, and the per-row kind freeze
> `scripts/baselines/jdk-only-kind-map-25-linux.tsv`. Landing the seven entries
> without re-freezing all three turns three gates red for a change that is
> otherwise correct — and the artefacts are keyed `25/linux`, so re-freezing them
> from Windows or from arithmetic is exactly what §2 forbids. §2's own
> pre-landing condition ("take a Compatible arm of `PwProbe` before landing") is
> also still untaken.
>
> **Disposition: hold, as a one-run work item rather than an open question.**
> Nothing about the decision remains to be decided; what is missing is a Linux
> build. **The exact recipe is §2.1, written out on 2026-08-12 so it lands in ONE
> pass and ONE commit.** The 29 `PrintStream` rows stay blocked on §3's list and
> were not touched.
>
> **DO NOT LAND HALF OF IT.** The two `retired_shadow.rs` edits without the
> re-freezes turn three gates red for a change that is otherwise correct; a
> re-freeze without the edits locks in a baseline for a tree that does not
> produce it. They are one commit or neither.

**Status:** OPEN, blocked on a Linux build only. 36 shadow rows resolved out of
36 owned; **7 retirable (patch in §2, recipe in §2.1, not applied), 29
blocked**. Two live defects found on the way, one of them a regression in the
retirement this lane was told to follow. **A taker with a Linux build should read
§2.1 and nothing else** — it is the whole change, in order, with the commands.

**What a shadow is.** A `Bridge` native registered on a method whose real class
has perfectly good bytecode. It runs *instead* of that bytecode, so the VM's
answer is only as good as the native — and the real object's state may never be
populated, which is how a family ends up right in the members with natives and
wrong in the members without. Contract §1.4. The population is the ratchet
`bridge_shadows_bytecode`, frozen at **6,066** in
`scripts/baselines/jdk-only-bridge-ratchet.json`.

**Scope.** The four files this lane owns: `native-builtins/src/logmanager.rs`,
`native-builtins/src/logging_shims.rs`, `native-builtins/src/util_time.rs`,
`native-builtins/src/date_format_fast.rs`. Precedent:
`java/util/logging/`'s 84 triples, retired 2026-08-11 through
`native-api/src/retired_shadow.rs` — see the retired
`bridge-reclassification-wave` write-up.

---

## 0. How the population was sized — and why not by grep

Not from an `rg` count; the README records grep-derived sizes here as wrong by
up to an order of magnitude, always the same direction. Taken instead from a
census the way `regression-suite/bridge-ratchet.sh` takes it — one boot,
`--real-jdk --explain-jdk-only --dump-native-registry`, against
`Eclipse Adoptium jdk-25.0.3.9-hotspot`, with a one-line probe on its own
classpath. Binary: `target/release/cratonvm.exe` built 2026-08-11 19:41
(it predates several of the day's dev merges; every claim below says which
binary produced it).

A row is a shadow when `image_declaring_method` says the image has the class
and the target carries `Code` — declared, or inherited with
`inherited_has_code`. That is the gate's own predicate, re-implemented against
the same census file.

| file | rows registered | of which `Bridge` shadows |
|---|---|---|
| `logging_shims.rs` | 103 | **36** |
| `logmanager.rs` | 101 | **0** |
| `util_time.rs` | 0 | 0 |
| `date_format_fast.rs` | 1 (`Intrinsic`) | 0 |

Windows total for the whole census: 6,079 shadow rows against the frozen
Linux 6,066 — a +13 platform difference, not a drift, and it is why nothing
below re-seeds a baseline.

Three of the four files hold **no** shadow rows, and each for a different
reason worth writing down:

* **`util_time.rs` is `#[cfg(feature = "synthetic-jdk")]`** (`lib.rs`, the
  `pub mod util_time;` line) and its three registrars are called only from
  `register_builtins`, the synthetic arm. It is not compiled into the shipped
  `cratonvm-cli` build at all, so it registers nothing in either JDK mode and
  **cannot move any of the three ratchets** — all of which take their census in
  Compatible mode. Its module doc already states the intended disposition
  ("a fallback, not a parallel implementation"). Retiring shadows there is not
  this campaign's work; deleting the module when T2.5.15 closes is.
* **`date_format_fast.rs` registers one `Intrinsic`**, not a `Bridge`:
  `java/text/DateFormat.format(Ljava/util/Date;)Ljava/lang/String;`. It is a
  performance intrinsic that cross-checks itself against the real bytecode once
  per output shape and *returns the bytecode answer* on any disagreement, so it
  is not a §1.4 shadow in the sense that matters — it cannot give an answer the
  bytecode would not. See §5.
* **`logmanager.rs`'s `java/util/logging/` rows are already retired** — the
  census shows them `synthetic-stub` with `kind_stated: true`, which is
  `retired_shadow.rs` doing its work. Its remaining 55 `Bridge` rows are on
  `org/jboss/logging/*` and `org/jboss/logmanager/*`, classes no supported
  image declares, so they are `class_absent` rows and not shadows. See §4 —
  the retirement is live and it is **broken**.

---

## 1. The instrument under-reports, and this is the first thing to fix

`CRATONVM_ENFORCE_NATIVE_SHADOW` is the dial the precedent was measured with.
**It yields at most once per triple, and then hands every later dispatch back
to the native.** Measured, `--jdk-only`, `CRATONVM_ENFORCE_NATIVE_SHADOW=java/io/PrintStream`:

```java
System.out.println("one"); System.out.println("two"); System.out.println("three");
for (int i = 0; i < 3; i++) System.out.println("loop" + i);
System.out.print("printA"); System.out.print("printB"); System.out.println();
```

```text
dial OFF   one two three loop0 loop1 loop2 printAprintB\n
dial ON        two three loop0 loop1 loop2       printB          <- no trailing newline
```

`"one"` is lost and `"two"`/`"three"` are not, at three different bytecode
indices, so this is per-TRIPLE and not per-call-site. `printA` is lost and
`printB` is not — the same shape on a second triple. `println()V` has exactly
one dispatch in the program, so it is lost outright, which is why the trailing
newline is missing. Identical under `--nojit`, so the JIT is not the mechanism.

Source-verified cause: **exactly one dispatch site in the VM consults the
dial.** `grep -rn jdk_only_enforce_shadow vm/src` finds one live reader,
`resolve_step1_native` in `vm/src/runtime/interpreter/native_override.rs`.
Every other path takes the native regardless — `resolve_native_for_dispatch`
(`vm/src/runtime/interpreter.rs`) hard-codes `compat_native_wins = true` with
a comment saying so, and `invoke_or_native` (`vm/src/vm/vm_exec.rs`) reaches
the same answer for a `Bridge` on a concrete JDK method. This is
`docs/architecture/natives-over-real-jdk-classes.md` §1 read from the other
side: the chain that *reinstates* the native on warm, cached, reflective and
JIT paths also reinstates it against the dial.

**Why that matters more than it sounds.** A retirement re-tags the kind at
registration, and *every* path honours a `SyntheticStub` refusal. So the dial
is a strictly weaker experiment than the retirement it licenses: it can read
verdict-neutral on a workload the retirement then breaks. §4 is that exact
case, live in the tree today. Add it to `W6-5`'s taxonomy of tests that read
green while measuring nothing.

**How to use it anyway, and how this lane did.** Since the yield lands on the
*first* dispatch of each triple, a probe in which each triple's first use is
the case under test gets one honest per-triple verdict per process. Every arm
below is built that way, and the ones that could not be (a receiver whose
constructor triple was already spent) are called out where they occur.

---

## 2. `java/io/PrintWriter` — 7 rows, RETIRABLE

All seven `Bridge` rows `register_printstream_fallback_natives` puts on
`java/io/PrintWriter`. Every one has real bytecode in the image
(`image_declaring_method.declared && has_code`), and the receiver's state is
real whichever constructor built it.

| triple | real bytecode? | state real? | observable change | verdict |
|---|---|---|---|---|
| `<init>(Ljava/io/OutputStream;)V` | yes | yes | none | retire |
| `write(Ljava/lang/String;)V` | yes | yes | none | retire |
| `write(Ljava/lang/String;II)V` | yes | yes | none | retire |
| `println(Ljava/lang/String;)V` | yes | yes | none | retire |
| `println()V` | yes | yes | none | retire |
| `println(I)V` | yes | yes | none | retire |
| `println(Ljava/lang/Object;)V` | yes | yes | none | retire |

**Why the state is real, and it is not luck.**
`native_printwriter_init_outputstream` ends with an `invoke_special` chain into
the real `PrintWriter(OutputStream, boolean)` — which is registered nowhere, so
it always runs as bytecode and populates `lock`, `out`, `charOut` and `textOut`
the way the JDK does. A `PrintWriter` over a `Writer` never enters a native at
all (`PrintWriter(Writer)` is not registered). So both construction paths leave
a receiver real bytecode can use, and that is what the arms show.

**Measured**, `--jdk-only`, one binary, only the dial differing, verdicts
written to a **file** because the console is part of what is under test:

```text
PwProbe — 7 triples over a ByteArrayOutputStream, then over a StringWriter,
          then a PrintWriter wrapping System.out
  HotSpot 25.0.3 control                            8/8 ok
  strict, dial OFF                                  8/8 ok
  strict, ENFORCE=java/io/PrintWriter               8/8 ok
  strict, ENFORCE=java/io/PrintWriter,java/io/PrintStream   8/8 ok
  "PW-OVER-SYSOUT" reached the console in all four arms

PwNativeCtor — the <init> yield spent on a throwaway, so the receiver under
               test is NATIVE-built while each method's yielded first dispatch
               lands on it
  HotSpot control / strict OFF / strict ENFORCE=java/io/PrintWriter   6/6 ok each
```

The second probe is the one that matters: it is the arm in which a native
constructor and retired methods meet, and it is green. §3 shows what that arm
looks like when the state is *not* real.

`PrintWriter(System.out)` is the JUnit ConsoleLauncher shape and the reason
this cannot be waved through on the OutputStream case alone: it chains real
`BufferedWriter`/`OutputStreamWriter` bytecode down onto a `PrintStream`
receiver whose state §3 shows is fabricated. It still works, because what it
lands on is `PrintStream.write([BII)V`, which stays a native.

### Out-of-file patch (not applied)

`native-api/src/retired_shadow.rs` is the campaign's home for this decision and
is not this lane's file. Two edits, both mechanical.

1. Widen the prefix discriminator in `triple_is_retired_shadow`, which today
   short-circuits on `java/util/logging/` alone:

```rust
pub fn triple_is_retired_shadow(class_name: &str, method_name: &str, descriptor: &str) -> bool {
    // Cheap discriminator: every entry is under one of these prefixes, and
    // almost no registration is, so the common case costs one prefix compare.
    // Keep this in sync with the table — a prefix missing here makes every
    // entry under it answer `false`, which reads as "not retired" and is
    // invisible. `every_entry_is_reachable_through_the_predicate` is the test
    // that catches it.
    if !(class_name.starts_with("java/util/logging/") || class_name.starts_with("java/io/Print")) {
        return false;
    }
    RETIRED_SHADOW_TRIPLES
        .binary_search(&(class_name, method_name, descriptor))
        .is_ok()
}
```

2. Insert these seven entries in sorted position (`java/io/…` sorts before
   `java/util/…`, so they go at the head of the table). The table is
   binary-searched and `the_table_is_sorted_and_unique` asserts the order.

```rust
    ("java/io/PrintWriter", "<init>", "(Ljava/io/OutputStream;)V"),
    ("java/io/PrintWriter", "println", "()V"),
    ("java/io/PrintWriter", "println", "(I)V"),
    ("java/io/PrintWriter", "println", "(Ljava/lang/Object;)V"),
    ("java/io/PrintWriter", "println", "(Ljava/lang/String;)V"),
    ("java/io/PrintWriter", "write", "(Ljava/lang/String;)V"),
    ("java/io/PrintWriter", "write", "(Ljava/lang/String;II)V"),
```

`the_table_is_not_empty`'s floor (`>= 80`) still holds; the count becomes 91.

**Ratchet effect — arithmetic, not a measurement. Do not paste these numbers
into the baselines.** Seven rows move `Bridge` → `SyntheticStub`, so
`bridge_shadows_bytecode` 6,066 → **6,059** (down 7),
`bridge_without_acc_native` 8,912 → **8,905** (down 7), and
~~`BASELINE_SYNTHETIC_STUBS` … 1,038 → 1,045~~ **— that third figure is STALE as
written and must not be pasted anywhere.** `native-builtins/tests/stub_ratchet.rs`
no longer holds one constant at 1,038: it holds **two**, split by the
`management` feature — `BASELINE_SYNTHETIC_STUBS_MANAGEMENT = 1263` and
`BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT = 1253`, with `SLACK` still `0`. **Both**
move, by the same +7, and both must be re-frozen from that configuration's own
printed recount line (§2.1 step 5). That file's own history is the reason: the
1,038 it froze on 2026-08-11 was six above what any run of it produced, because
it was hand-derived. Both baselines must be re-frozen from one real run on
the platform they are keyed to — `sh regression-suite/bridge-ratchet.sh
--update-baseline --note "…"`, which the script re-freezes as a pair on
purpose. The frozen artefact is keyed `25/linux`; this lane measured on
Windows and so is not entitled to seed it.

`java/io/Print*` is a Compatible-mode-visible package, so unlike the
`register_synthetic_overrides`-only registrars these rows *will* move the
ratchet. A `SyntheticStub` registers and dispatches normally in `Compatible`
mode, so `--real-jdk` behaviour is unchanged — but that is a property of the
mechanism, not of this measurement, and §2's arms did not test Compatible.
Take a Compatible arm of `PwProbe` before landing.

### 2.1 The landing recipe — one Linux pass, one commit

Written out 2026-08-12 because the blocker is a platform, not a question, and
because every previous "hold" note left the taker to re-derive the commands.
**Prerequisite: a Linux host with a JDK 25 image.** All three frozen artefacts
are keyed `<jdk-feature>/<os>`; the gate scripts derive the OS half from the
running host, so on Windows they look up `25/windows`, find no baseline and exit
**2** ("REFUSING") — which is neither a pass nor a fail, and is why re-freezing
from this host is impossible rather than merely discouraged.

```sh
# 0. Clean tree at the tip you intend to land on.
export JAVA_HOME=/path/to/jdk-25            # a real JDK 25 runtime image

# 1. THE TWO SOURCE EDITS, verbatim from §2's "Out-of-file patch" above:
#      (a) widen `triple_is_retired_shadow`'s prefix discriminator in
#          native-api/src/retired_shadow.rs to admit `java/io/Print`
#      (b) insert the seven ("java/io/PrintWriter", …) entries in SORTED
#          position — they sort BEFORE every `java/util/logging/` row, so at
#          the HEAD of the table.
#    Do (a) and (b) together: entries under a prefix the discriminator does not
#    admit answer `false`, which reads as "not retired" and is invisible.

# 2. The table's own guards, before anything expensive:
cargo test -p cratonvm-native-api retired_shadow
#    the_table_is_sorted_and_unique, every_entry_is_reachable_through_the_predicate
#    and the_table_is_not_empty (floor >= 80; the count becomes 95: 88 + 7).

# 3. Build the binary the census and the probe both use:
cargo build --release -p cratonvm-cli

# 4. §2's still-untaken pre-landing condition: the COMPATIBLE arm of PwProbe.
#    §2's four arms were all --jdk-only. Run the same probe with --real-jdk and
#    require 8/8, with HotSpot as the control:
#      $JAVA_HOME/bin/java  -cp <dir> PwProbe          # control, must be 8/8
#      target/release/cratonvm --real-jdk -cp <dir> PwProbe
#      target/release/cratonvm --jdk-only -cp <dir> PwProbe
#    A SyntheticStub still dispatches in Compatible, so this arm is expected to
#    be unchanged — it is a falsifier for that expectation, not a formality.

# 5. Re-freeze the stub ratchet. BOTH configurations, each from its OWN run's
#    printed `stub-ratchet: const <NAME>: usize = <N>;` line. Do not hand-derive
#    +7 — that is the exact mistake the 1038 seed was.
cargo test -p cratonvm-native-builtins --test stub_ratchet -- --nocapture
cargo test -p cratonvm-native-builtins --features management \
    --test stub_ratchet -- --nocapture
#    Paste each printed line over the constant it names in
#    native-builtins/tests/stub_ratchet.rs. Keep SLACK = 0.

# 6. Re-freeze the bridge ratchet AND the kind map — ONE command, ONE census,
#    on purpose: they are two readings of one measurement and the script
#    refuses to let them come from different runs.
sh regression-suite/bridge-ratchet.sh --update-baseline \
    --note "W7-22 §2: retire the 7 java/io/PrintWriter shadows (Bridge -> SyntheticStub)"
#    Rewrites scripts/baselines/jdk-only-bridge-ratchet.json AND
#    scripts/baselines/jdk-only-kind-map-25-linux.tsv. `--note` is REQUIRED; the
#    script refuses to freeze without one.

# 7. Confirm green with no --update-baseline, and confirm the corpus:
sh regression-suite/bridge-ratchet.sh
CRATONVM_ARGS=--jdk-only SUITE=all bash regression-suite/run.sh

# 8. Commit steps 1, 5 and 6 TOGETHER.
```

Two things to read before pasting anything in step 6. First, an
`--update-baseline` run **regenerates the kind-map baseline's header from the
census**, so check the diff for header fields that existed on the Linux baseline
and did not survive. Second, this is the moment the four *separate* pre-existing
drifts on these artefacts come due and they must not be absorbed as one number
— README §2.1's `W7-20` row enumerates them (its own retag, the four scalar
`StringBuilder.insert` overloads, the `Formatter.formatMessage` retag of
`3b20b83b5` that postdates the 12:20 freeze, and whatever else is in the 182
commits). Attribute each; a single re-freeze that silently swallows all of them
is how a regression reads as IMPROVED.

---

## 3. `java/io/PrintStream` — 29 rows, BLOCKED

Same registrar, same file, opposite verdict, and the difference is entirely in
the receiver.

**The measurement, `--jdk-only`, `ENFORCE=java/io/PrintStream`:**

| receiver | result |
|---|---|
| user-constructed, ctor yielded too | **26 of 26 triples ok** |
| user-constructed by the NATIVE ctor | `close()` → `NullPointerException: Cannot invoke "java.io.BufferedWriter.close()" because "this.textOut" is null` |
| the VM-minted `System.out` | **every output triple silently produces nothing**, exit 0 |

One process per triple, the operation under test being that triple's first
dispatch, console captured:

```text
selector       HotSpot      strict OFF   strict ON
println(String)  [MARK]       [MARK]       []
println(int)     [7]          [7]          []
print(String)    [MARK]       [MARK]       []
printf           [MARK7]      [MARK7]      []
write([BII)      [MARK]       [MARK]       []
append           [MARK]       [MARK]       []
flush            []           []           []      (no-op either way)
```

**Why it is silent rather than loud**, and this is the part that makes it
dangerous. `javap -c java.io.PrintStream` on the JDK 25 image:

```text
private void writeln(java.lang.String);
     5: invokevirtual  ensureOpen:()V
     9: getfield       textOut …
   Exception table:
       from  to  target type
          0  61      74  Class java/io/IOException
    74: astore_2  75: aload_0  76: iconst_1  77: putfield trouble:Z  80: return
```

`ensureOpen()` throws `IOException("Stream closed")` when `out == null`, and
`writeln`'s own exception table catches `java/io/IOException` and sets
`trouble = true`. **A retired `PrintStream` shadow over `System.out` does not
fail — it discards.** Nothing in the corpus asserts on stdout's presence, so a
suite would read green while the VM printed nothing.

**What is not real, measured by reflection** (`--add-opens
java.base/java.io=ALL-UNNAMED`, HotSpot as control):

| `System.out` field | HotSpot | CratonVM `--jdk-only` |
|---|---|---|
| `FilterOutputStream.out` | `BufferedOutputStream` | **null** |
| `PrintStream.charOut` | `OutputStreamWriter` | **null** |
| `PrintStream.textOut` | `BufferedWriter` | **null** |
| `FilterOutputStream.closeLock` | `Object` | **null** |
| `PrintStream.charset` | `sun.nio.cs.MS1251` | instance of the **abstract** `java.nio.charset.Charset` |

The registrar says so itself, in a comment that has been right the whole time:
*"Our System.out/err are fd-backed synthetic PrintStreams (slot 0 = fd id);
their inherited FilterOutputStream `out` field is never populated."* The
natives were written **because** the state is fake. Retiring them without
fixing that is the §1.4 order run backwards.

The native constructor is the second half of the same story:
`native_printstream_init_outputstream` writes `out` and `lock` and nothing
else, so a receiver it built passes `ensureOpen()` and then NPEs on
`textOut` — which is the `close()` row above, and is what a *partial*
retirement of this class looks like. `PrintWriter`'s native constructor chains
to real bytecode; `PrintStream`'s does not. That one difference is the whole
verdict split between §2 and §3.

### What must become real first — the blocked list

Retiring the `java/io/PrintStream` rows needs all of these, and the first is
the whole job:

1. **`System.out` / `System.err` must be constructed, not fabricated.** They
   need a real `OutputStream` over the process fd in `FilterOutputStream.out`,
   a real `OutputStreamWriter` in `charOut`, a real `BufferedWriter` in
   `textOut`, and a plain `Object` in `closeLock` — i.e. the receiver must come
   out of `PrintStream(OutputStream, boolean, Charset)` rather than out of an
   allocator. Everything else on this list is downstream of it.
2. **`PrintStream.charset` must be a concrete `Charset`.** It is currently an
   instance of the abstract `java.nio.charset.Charset`, which is one of the
   five blocker families the retired
   `jdk-only-step1-bytecode-available-RESOLVED-20260806.md` names; real
   `writeln` reaches it through `charOut`.
3. **`native_printstream_init_outputstream` must chain to a real constructor**
   the way `native_printwriter_init_outputstream` already does, or be retired
   in the same increment as the methods. Retiring the methods and keeping this
   constructor reproduces the `close()` NPE above on every user-constructed
   stream.
4. `PrintStream.write(Ljava/lang/String;)V` is **package-private** in the JDK 25
   image. It has `Code`, so the census counts it as a shadow and a retirement
   would be legal, but no probe outside `java.io` can reach it; it must retire
   with the family or not at all.
5. `PrintStream.write(Ljava/lang/String;II)V` is **not declared by the image**
   (`declared: false`) and is therefore *not* in the 29. It must be held back
   by name, exactly as `retired_shadow.rs` holds back
   `Logger.log(Level, Supplier, Throwable)`: retiring it replaces a working
   native with a `NoSuchMethodError`, and the registrar's own comment records
   that JUnit's ConsoleLauncher calls it.

---

## 4. Live defect: the `java/util/logging` retirement broke strict-mode JUL

> **§4's NAMED CAUSE IS WRONG, AND ITS PRESCRIBED REPAIR IS DEAD — DO NOT APPLY.
> Reconciled 2026-08-12.** The *regression* below is real and was measured
> correctly. The *cause* named under "What must become real first" is not it, and
> "build the singleton through its real constructor" was written, measured
> **INERT IN BOTH MODES**, and reverted. The real mechanism is more general: a
> retired shadow is silently reinstated by any other registrar holding the same
> triple under a `NativeKind` the retirement is exempt from. The fix that landed
> is a category change — `r.with_category(NativeKind::Bridge, …)` at
> `native-builtins/src/phases_early.rs:20627-20637`, commits `4eaa5d321` and
> `3b20b83b5`. Full write-up: W7-25-jul-getlogger-regression.md. Index entry:
> W7-55-record-reconciliation.md §2.4. Kept below unedited for the causal A/B,
> which is the reusable part.

This is the precedent this lane was told to follow, and it is a regression.

`Logger.getLogger("x")` — the first call any JUL user makes — throws under
`--jdk-only`:

```text
java.lang.NullPointerException: Cannot invoke
  "java.util.logging.LogManager$LoggerContext.demandLogger(String, String, java.lang.Module)"
  because the return value of "java.util.logging.LogManager.getSystemContext()" is null
    at java.util.logging.LogManager.demandSystemLogger(LogManager.java:498)
    at java.util.logging.Logger.demandLogger(Logger.java:641)
    at java.util.logging.Logger.getLogger(Logger.java:708)
```

**Causal A/B, one probe, four binaries, kind read from each binary's own
census** (`--dump-native-registry`, row
`java/util/logging/Logger.getLogger(Ljava/lang/String;)Ljava/util/logging/Logger;`):

| binary | that row's kind | `JulProbe --jdk-only` |
|---|---|---|
| `CratonVM-a5old-20260811` (19:41-class, pre-retirement) | `bridge` | **16 of 17 ok** |
| `CratonVM-cmid2-20260811` | `synthetic-stub`, `kind_stated` | NPE on the first call, 0 of 17 |
| `CratonVM-dim4-20260810` | `synthetic-stub`, `kind_stated` | NPE, 0 of 17 |
| `CratonVM-a5walk-20260811` | `synthetic-stub`, `kind_stated` | NPE, 0 of 17 |
| `CratonVM/target/release` (this lane's census binary) | `synthetic-stub`, `kind_stated` | NPE, 0 of 17 |

The split is exactly on the presence of the retirement, and HotSpot 25.0.3 is
17 of 17. Compatible mode is unaffected (a `SyntheticStub` still dispatches
there) — 16 of 17, the missing one being §4.1.

**Why the acceptance measurement could not see it**, three reasons and all
three are reusable:

* **No corpus vector calls `Logger.getLogger`.** Checked, not assumed:
  `grep -l "java.util.logging\|Logger.getLogger" regression-suite/src/*.java`
  matches **nothing**, across all 57 vectors and all three class lists. The
  23/4 that licensed the retirement is a vector-level verdict and JUL owns no
  vector, which is exactly the case the README's licence carves out — "a record
  claiming something narrower than its vector asserts still owns that claim".
* **The dial yields once per triple (§1).** Even a vector that called
  `getLogger` twice would have taken the native the second time, so the
  workload the dial measured is not the workload the retirement produces.
* **The dial and the retirement are not the same experiment.** One dispatch
  path honours the dial; every path honours the re-tag.

**What must become real first.** `LogManager`'s singleton is allocated, never
constructed. `allocate_log_manager` (`logmanager.rs`) calls
`try_alloc_concurrent_synthetic(ctx, class_name, LM_NUM_FIELDS)` and writes
four slots by index; `<init>` never runs. Measured against HotSpot with
`--add-opens java.logging/java.util.logging=ALL-UNNAMED`, on the singleton
`LogManager.getLogManager()` returns:

| field | HotSpot | CratonVM (both modes, both binaries) |
|---|---|---|
| `props` | `Properties` | **null** |
| `systemContext` | `LogManager$SystemLoggerContext` | **null** |
| `userContext` | `LogManager$LoggerContext` | **null** |
| `rootLogger` | `LogManager$RootLogger` | **null** |
| `configurationLock` | `ReentrantLock` | **null** |
| `closeOnResetLoggers` | `CopyOnWriteArrayList` | **null** |
| `listeners` | `Collections$SynchronizedMap` | **null** |
| `loggerRefQueue` | `ReferenceQueue` | **null** |

`allocate_log_manager`'s own comment argues the nulls are safe — *"most
Quarkus/JBoss code reads it via accessors we no-op, so null is safe"* — and
that was true right up until the retirement stopped no-opping the accessors.
Until `LogManager` is built by its real constructor, `Logger.getLogger`,
`Logger.getGlobal`, `LogManager.getProperty`, `readConfiguration` and
`reset` cannot run as bytecode.

**Two dispositions, and the choice is the orchestrator's, not this lane's.**

* **Hold back the four triples that dereference `systemContext`/`rootLogger`**
  — `Logger.getLogger(String)`, `Logger.getLogger(String,String)`,
  `LogManager.getLogManager()`, `LogManager.<init>()V` — from
  `RETIRED_SHADOW_TRIPLES`, the same way `Logger.log(Level,Supplier,Throwable)`
  is held back. Cheap, reversible, and restores 16 of 17 today.
* **Or build the singleton for real**, which is the §1.4-correct answer and is
  the same shape of work item as §3's list.

Either way `probes/` needs a JUL vector: the probe this lane used is
17 assertions over `Logger`/`Level`/`LogManager`/`LogRecord` with a `Handler`
of its own, so it observes records without depending on the console, and it
separates HotSpot from both CratonVM modes. Its absence is why this shipped.

### 4.1 `Logger.log(Level, Supplier)` drops the record — Compatible mode, pre-existing

Independent of the retirement, and visible on every binary tested including the
pre-retirement one:

```text
HotSpot            [INFO:i, WARNING:w, SEVERE:s, FINE:f, INFO:L, INFO:P{0}, INFO:T, INFO:sup]
CratonVM compat    [INFO:i, WARNING:w, SEVERE:s, FINE:f, INFO:L, INFO:P{0}, INFO:T]
```

`native_jul_logger_log_supplier` (`logmanager.rs`) resolves the supplier and
calls `crate::emit_framework_log` — the console sink — and never fans the
record out to the logger's installed `Handler`s. Its sibling
`native_jul_logger_log_record`, twenty lines below, does exactly that fan-out
and says why in its own comment. So an application `Handler` sees seven of
eight `log` overloads: right in the members with one implementation, wrong in
the members with the other. Not fixed here — the fix is a Compatible-mode
behaviour change and this lane's mandate is that Compatible stays
byte-for-byte unchanged.

---

## 5. `date_format_fast.rs` — one `Intrinsic`, deliberately not a shadow

`java/text/DateFormat.format(Ljava/util/Date;)Ljava/lang/String;` is a native
in front of real bytecode, and it is not a §1.4 shadow in the sense the ratchet
counts: it is registered `NativeKind::Intrinsic`, which §1 exempts from the
strict yield, and it is the only kind that *cannot* give an answer the bytecode
would not. Every unsupported receiver, calendar, cutover, `zeroDigit` and
pattern letter falls through to `format(Date, StringBuffer, FieldPosition)` as
bytecode, and every supported one is cross-checked against that bytecode once
per output shape, with a mismatch returning the bytecode answer and poisoning
the fast path permanently. Retiring it buys nothing and costs the 557x it was
written to close (`org.apache.juli.TestOneLineFormatterPerformance` is a ratio
test). **Verdict: keep, and it should stay out of any future shadow census as
an `Intrinsic` rather than be re-argued each wave.**

---

## 6. What is proven and what is not

**Proven by running the existing binary** (`target/release/cratonvm.exe`,
2026-08-11 19:41, JDK 25.0.3+9, HotSpot control on every arm):

* the census row counts in §0, and that three of the four files hold no
  shadow rows;
* the dial's once-per-triple behaviour (§1), reproduced on two triples and
  under `--nojit`;
* every `PrintWriter` arm in §2 and every `PrintStream` arm in §3;
* the JUL regression in §4, with a pre-retirement binary as the control and
  each binary's own census as the discriminator;
* the `log(Level, Supplier)` drop in §4.1.

**Not proven, and not claimed.** Nothing was rebuilt. The §2 patch has not been
compiled, applied or run; "retirable" there means *every arm that can be run
without a rebuild is verdict-neutral*, not *the retirement was executed*. The
ratchet deltas are arithmetic. The Compatible-mode arm of `PwProbe` was not
taken. The strict corpus was not re-run on this branch — the binary is not this
branch's, and running `regression-suite/run.sh` against a foreign binary would
attribute its results to source it was not built from.

## 7. Adjacent, not this lane's files

* `native-api/src/retired_shadow.rs` — §2's patch, and §4's hold-back.
* `native-builtins/tests/stub_ratchet.rs` — `BASELINE_SYNTHETIC_STUBS`, re-freeze
  from a real run.
* `scripts/baselines/jdk-only-bridge-ratchet.json` — same, as a pair, on linux.
* `probes/` — the JUL vector §4 says is missing.
* `vm/src/runtime/interpreter/native_override.rs` and
  `vm/src/runtime/interpreter.rs` — if the dial is ever to mean what its
  documentation says, the second path has to consult it too. Today it does not,
  and §1 is the cost.

---

## 8. Triage re-read against source, 2026-08-12 (lane A24, doc-only)

Nothing here was built or run. The point of this pass is narrow: a record that
says "hold, blocked on a platform" decays into "nobody checked" unless somebody
re-confirms that the hold is still the true state. It is.

### 8.1 §2's seven-row patch is STILL unapplied — checked, not assumed

`native-api/src/retired_shadow.rs` today contains:

* **no** `("java/io/PrintWriter", …)` entries — zero matches for the string
  `"java/io/PrintWriter"` in the file;
* **no** `java/io/Print` arm in `triple_is_retired_shadow`'s prefix
  discriminator.

Both halves of §2's out-of-file patch are absent, which is the correct state:
§2.1 step 1 warns that landing (b) without (a) makes every new entry answer
`false` invisibly, and neither is there. **Disposition unchanged: hold.**

### 8.2 The three frozen artefacts have not moved, so §2.1 needs no re-derivation

This is the part most likely to have gone stale, since §2's own history is a
figure that was wrong when written. It has not:

* `native-builtins/tests/stub_ratchet.rs:493` `BASELINE_SYNTHETIC_STUBS_MANAGEMENT
  = 1263` and `:498` `BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT = 1253`, with
  `:534` `const SLACK: usize = 0;`. Exactly the two constants and the slack §2's
  correction block names, at exactly those values. The `:506`/`:508` cfg pair
  selecting between them by the `management` feature is intact, and `:528`/`:530`
  still print the constant's own name, which is what §2.1 step 5 tells the taker
  to paste from.

So §2.1 is executable as written. **The `1,038` figure struck through in §2
remains struck through** — do not resurrect it; and do not hand-derive `+7` onto
1263/1253 either, for the reason §2.1 step 5 gives.

### 8.3 §1's dial finding survives, with one refinement

§1's cause — *exactly one dispatch site in the VM consults the dial* — still
reads true, and the dial has since been given a scoped form:

* `vm/src/runtime/env_cache.rs:473` `jdk_only_enforce_shadow()`, and `:559`
  `jdk_only_enforce_shadow_for(class_name)`, whose own doc says *"This is the
  predicate dispatch must ask"* because the unscoped bool alone answers only "is
  anything enforced";
* exactly **one** caller of either outside `env_cache` itself, in all of
  `vm/src`: `vm/src/runtime/interpreter/native_override.rs:7135`,
  `strict_bridge && …jdk_only_enforce_shadow_for(class_name)`.

The refinement does not change the verdict. §1's real claim is that the dial is a
*strictly weaker experiment than the retirement it licenses* — one dispatch path
honours the dial, every path honours the re-tag — and that is unaffected by the
predicate becoming class-scoped. §2's "retirable" therefore still means *every
arm runnable without a rebuild is verdict-neutral*, which is what §6 already
says.

### 8.4 Is this record's evidence SCHEDULED?

**No, and it is worth being blunt about it.** Every measurement in §§1-4 is a
bespoke probe (`PwProbe`, `PwNativeCtor`, the per-triple `PrintStream` selector
runs, the JUL probe), and the string `probes` occurs **zero** times in
`regression-suite/run.sh` at any `SUITE=` value. Nothing that licensed §2's
verdict, and nothing that found §4's regression, runs in any suite.

§4 already establishes the sharpest form of this — *no corpus vector calls
`Logger.getLogger`*, which is why the JUL regression shipped — and §4's closing
line ("`probes/` needs a JUL vector … Its absence is why this shipped") is still
outstanding. §3's danger case is the same species and worse: a retired
`PrintStream` shadow over `System.out` **discards** rather than throwing, and
nothing in the corpus asserts on stdout's presence, so the suite would read green
while the VM printed nothing. That is not a hypothetical about a future
retirement; it is the reason §3 is blocked.

### 8.5 Residuals, unchanged

1. **§2's seven rows** — hold, blocked on a Linux host with a JDK 25 image. Not a
   question, a platform. §2.1 is the whole recipe and is current (§8.2).
2. **§2's untaken pre-landing condition** — the **Compatible** arm of `PwProbe`.
   §2's four arms were all `--jdk-only`. Still untaken; it is §2.1 step 4.
3. **§3's 29 `PrintStream` rows** — blocked on the five-item list, of which item 1
   (`System.out`/`System.err` must be constructed, not fabricated) is the whole
   job and the rest are downstream of it.
4. **§4's disposition is the orchestrator's call and has not been made here.**
   Note that §4's own banner records the *cause* as superseded (W7-25) and the
   category-change fix as landed, so the "hold back the four triples" option
   described in §4 may already be moot; this lane did not re-measure the JUL
   regression and does not claim either way.
5. **§4.1's `Logger.log(Level, Supplier)` record drop** — Compatible-mode,
   pre-existing, explicitly out of this lane's mandate, and still open.
6. **A JUL vector in `probes/`** — and, given §8.4, a *scheduled* JUL vector
   would be worth more than a probe one: a probe that nothing runs is the exact
   gap §4 diagnoses.

No nominations. Every code change this record wants is already written out as an
out-of-file patch in §2 and sequenced in §2.1; adding a second copy here would
give a taker two texts to reconcile, which is how a one-pass recipe becomes a
two-pass one.
