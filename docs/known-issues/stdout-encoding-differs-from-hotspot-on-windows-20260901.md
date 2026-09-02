# `System.out` writes UTF-8 where HotSpot follows the console, so a differential run diverges on every non-ASCII line

| | |
|---|---|
| **Status** | **OPEN — not fixed here, deliberately.** Deciding what `System.out`'s default charset should be is a compatibility judgement with a blast radius across every Windows user; it belongs in a reviewed change of its own. §9 states the options and a recommendation. |
| **Symptom** | A 273-assertion HotSpot differential that reports **zero** divergences on Linux reports **ten** on Windows. All ten are the same shape: HotSpot prints `?` where CratonVM prints the character. |
| **Cause** | CratonVM pins `stdout.encoding`, `stderr.encoding` and `native.encoding` to the literal `"UTF-8"` at boot and stamps a literal `"UTF-8"` `Charset` on `System.out` / `System.err`. Nothing in the tree ever asks the host what its console or locale encoding is — `GetConsoleOutputCP`, `GetACP` and `GetOEMCP` appear **zero** times in the repository. HotSpot derives the value from the host, which JEP 400 left it doing on purpose. |
| **Not a semantics defect** | Re-running **both** VMs with `-Dstdout.encoding=UTF-8 -Dfile.encoding=UTF-8` gives **0 divergences on both probes**. The characters are right in both VMs. This is a disagreement about which bytes carry them. |
| **Severity** | Zero for program results. High for **tooling**: any harness that compares CratonVM's stdout against HotSpot's — including this repo's own `--diff-hotspot`, which landed the same day — reports a false divergence on the first non-ASCII line, on every non-UTF-8 host. §7. |
| **Also wrong, and less arguable** | `native.encoding` is *specified* as "derived from the host environment and the user's settings" and to be unsettable from the command line. CratonVM answers a fixed `UTF-8`. That one is not a judgement call. §4.4. |

## 1. The witness

Measured 2026-09-01 by the audit orchestrator, on **one** Windows machine,
against a freshly built binary, with stdout going to a console window. Both
sides ran the same class files.

```
                    HotSpot 25 (Windows console)   CratonVM (Windows console)
s_lower             hello, ??? world               hello, é中😀 world   (UTF-8 bytes)
s_upper             HELLO, ??? WORLD               HELLO, É中😀 WORLD
sb_surrogate        b?a                            b😀a
cs_utf8_bad         [?, (]                         [<U+FFFD>, (]
file_rw             h?llo                          héllo
```

Ten rows, five shapes, one mechanism: every row is a character the Windows
console code page cannot represent. HotSpot's encoder substitutes `?` (byte
`0x3f`); CratonVM's emits the UTF-8 sequence.

`cs_utf8_bad` is the tell that this is an *encoder* difference and not a decoder
one: both VMs decoded the malformed input to U+FFFD correctly and identically,
and then disagreed only about how to write U+FFFD out.

## 2. What this rules out, and how

**Pinning the property collapses all ten.** Re-run both VMs with
`-Dstdout.encoding=UTF-8 -Dfile.encoding=UTF-8` and both probes report **0
divergences**. That is the whole proof that this is encoding and not semantics:
if either VM had the wrong *characters* — a broken `toUpperCase`, a lost
surrogate pair, a mis-decoded byte — forcing a common output charset could not
make the difference vanish. It vanishes, so the characters agree and only the
bytes on the wire differed.

This repository has drawn the same conclusion before, from the other end. The
`java.io.File` normalisation survey
(`docs/known-issues/jdk-only/bug-file-path-normalisation-windows-arm-20260826.md`
§4) found *sixteen* of its thirty remaining "differences" were this and nothing
else —

```text
HotSpot   [unicode/???] getName |???|          stdout.encoding=Cp1251
CratonVM  [unicode/<utf-8 bytes>] getName …    stdout.encoding=UTF-8
```

— and stated the general rule it took a false 30 to learn: *a cross-VM stdout
diff must not carry an encoding-dependent byte.* The
`PrintStream.charset()` page
(`docs/known-issues/jdk-only/bug-printstream-charset-answers-the-abstract-base-20260825.md`
§5) then named this exact question and deferred it: "CratonVM answers **UTF-8**
where HotSpot answers the console encoding (`Cp1251` on this host, from
`stdout.encoding`) … A separate question worth its own measurement." This page
is that measurement.

Note the code page in that earlier record: **Cp1251**, not 437 or 1252. Which
characters mangle, and therefore which rows of a differential go red, is a
property of the *machine*, not of Windows.

## 3. It reproduces on Linux, with no Windows console — MEASURED

This is the part that makes the issue cheap to work on. HotSpot derives
`stdout.encoding` from the host on **every** platform, not only Windows, so
setting a non-UTF-8 locale on Linux forces the identical mechanism.

Measured 2026-09-01 on `vm1` (Linux x86-64, Temurin 25.0.4+7), HotSpot only,
`probes/StdoutEncoding.java`:

```text
                          java (LANG=C.UTF-8)   java (LC_ALL=C)
prop stdout.encoding      UTF-8                 ANSI_X3.4-1968
prop stderr.encoding      UTF-8                 ANSI_X3.4-1968
prop native.encoding      UTF-8                 ANSI_X3.4-1968
prop file.encoding        UTF-8                 UTF-8            <- does NOT move
default charset           UTF-8                 UTF-8            <- does NOT move
stream out.charset        UTF-8                 US-ASCII         <- moves with the locale
enc latin1 bytes          c3 a9                 3f
enc cjk bytes             e4 b8 ad              3f
enc astral bytes          f0 9f 98 80           3f
verdict representable     true                  false
raw mixed                 hello, é中😀 world     hello, ??? world
raw upper                 HELLO, É中😀 WORLD     HELLO, ??? WORLD
raw sb                    b😀a                   b?a
raw replchar              <U+FFFD>(             ?(
```

The right-hand column is the Windows witness of §1, produced on Linux by one
environment variable. Three things fall out of it:

* **`file.encoding` and `stdout.encoding` are independent.** `file.encoding`
  stayed `UTF-8` while `stdout.encoding` moved. That is JEP 400's rule made
  visible, and it is the distinction the whole page turns on (§5).
* **`System.out` does not use the default charset.** `Charset.defaultCharset()`
  answered `UTF-8` while `System.out.charset()` answered `US-ASCII` in the same
  process.
* **A console is not required.** Run on a pty and on a pipe on this host, the
  answers were the same in both; the locale decided. On Windows the console
  code page is an *additional* input — see §8 for what that leaves unmeasured.

CratonVM was **not** run in either arm: there is no built binary on the audit
host, and this page was written under a no-build rule. From the source (§4) its
answer is `UTF-8` in both columns, but that is reasoned, not measured.

## 4. What CratonVM actually does — from the source

### 4.1 The property is a literal, in two tables

```text
vm/src/vm/vm_init.rs:3557-3561,3572     file.encoding, native.encoding, sun.jnu.encoding,
                                        stdout.encoding, stderr.encoding, stdin.encoding
                                        — each `.insert(…, "UTF-8".to_string())`
native-builtins/src/system_bootstrap.rs:322-337
                                        the same six, plus sun.stdout.encoding and
                                        sun.stderr.encoding, each `"UTF-8".to_string()`
```

The comment above the first block reads *"Encodings — JDK 18+ pinned to UTF-8
for stdout/stderr/file/native."* JEP 400 pinned `file.encoding`. It did not pin
the other three (§5), so the premise written down beside the code is false for
three of the four keys it names.

`system_bootstrap.rs` carries its own warning that these two tables "overlap
without agreeing" and that only `vm_init.rs`'s is the one reaching
`System.getProperties()` in real-JDK mode. Both say `UTF-8`, so the disagreement
does not matter here — but it means a fix has two doors, and the file says so in
capitals: *"Add a key to both or you will add it to neither."*

One key in the second table is a trap for whoever fixes this. `sun.stdout.encoding`
and `sun.stderr.encoding` (`system_bootstrap.rs:323-324`) are set to `UTF-8`;
HotSpot 25 leaves them **null** (measured, §3, `prop sun.stdout.enc null` in both
locale arms). Per §5's chain those two keys are consulted *before* the
platform-computed value, so a `sun.stdout.encoding` that is present and pinned
would override a correctly derived `stdout.encoding` and quietly undo the fix.
Whether that table reaches `System.getProperties()` in real-JDK mode is exactly
what its own warning says is unclear; `probes/StdoutEncoding.java` prints the key
and settles it.

### 4.2 The stream's charset is a *second*, independent literal

`System.out` and `System.err` do not get their charset from the property.
`native_system_init_phase1` calls `install_charset` on each
(`native-builtins/src/lang_system.rs:5103` and `:5108`), and that helper asks
for the charset by name:

```text
native-builtins/src/lang_system.rs:4921   let want = ctx.create_string("UTF-8");   // Charset.forName(want)
native-builtins/src/lang_system.rs:4940   let name = ctx.create_string("UTF-8");   // stub fallback
```

No `getProperty("stdout.encoding")` on either path, and no per-stream
distinction — `out` and `err` are stamped by the same call with the same
literal, so `stderr.encoding` could not diverge from `stdout.encoding` even if
the property were consulted. `PrintStream.charset()`'s own native
(`native-builtins/src/lib.rs:36888-36933`) repairs a missing or non-concrete
field with a third hard-coded `"UTF-8"`.

### 4.3 The encoder itself is already correct — which is what makes this cheap

The write path does honour whatever charset is stamped. `printstream_encode`
(`native-builtins/src/lib.rs:28140`) reads the receiver's `charset` field and
routes ISO-8859-1, US-ASCII and everything else through the right encoder; its
own doc comment records the measurement that put it there
(`new PrintStream(sink, true, US_ASCII).print("a中b")` emitting three high-bit
bytes before the fix). `None` — meaning "just use `as_bytes()`, i.e. UTF-8" — is
returned for exactly one reason on the system streams: the charset stamped on
them *is* UTF-8.

So the machinery to write cp1251 or cp437 on `System.out` already exists and is
already tested. What is missing is only the derivation of *which* charset to
stamp.

### 4.4 Console vs pipe, and the console code page

* **Does CratonVM read a console code page anywhere?** No.
  `GetConsoleOutputCP`, `GetConsoleCP`, `GetACP`, `GetOEMCP` and
  `windows_sys::…::Console` each occur **zero** times outside `target/`.
* **Does it distinguish a console from a pipe?** Yes — but only for
  `java.io.Console`, never for encoding. `java/io/Console.istty()Z` and
  `ttyStatus` are backed by the real `std::io::IsTerminal`
  (`native-builtins/src/lib.rs:9356-9366`, `:10744`), which is `isatty` /
  `GetConsoleMode`. That per-stream truth is computed and then not used to
  choose an encoding.
* **Does it read the locale?** Yes, and correctly — `vm/src/vm/vm_init.rs:426-460`
  implements the JDK's `LC_ALL` ▸ category ▸ `LANG` precedence, and
  `native-builtins/src/locale_bootstrap.rs:186-190` does the same for
  `user.language` / `user.country`. **That machinery is not wired to the
  encoding properties.** On Unix, deriving `native.encoding` from the locale is
  therefore a small change against code that already exists and already has the
  precedence right; only the Windows console leg is new.
* **`native.encoding` on Windows** is the fixed string `UTF-8`, from the two
  tables in §4.1. HotSpot answers the ANSI code page. `System`'s own property
  table specifies `native.encoding` as "derived from the host environment and
  the user's settings" and says setting it on the command line has no effect;
  a constant satisfies neither clause. This is the one key in the group where
  "which behaviour is right" is not a judgement call.

## 5. What Java 25 actually specifies

Read from the JDK 25 sources on the audit host
(`/data/toolchain/jdk-25/lib/src.zip`), not from memory.

**`jdk/internal/util/SystemProps.java:82-102`** — the rule, verbatim:

```java
// "file.encoding" defaults to "UTF-8", unless specified in the command line
// where "COMPAT" designates the native encoding.
String fileEncoding = props.get("file.encoding");
if (fileEncoding == null) {
    put(props, "file.encoding", "UTF-8");
} else if ("COMPAT".equals(fileEncoding)) {
    put(props, "file.encoding", nativeEncoding);
}
...
putIfAbsent(props, "stdout.encoding", props.getOrDefault("sun.stdout.encoding",
        raw.propDefault(Raw._stdout_encoding_NDX)));
putIfAbsent(props, "stdout.encoding", nativeEncoding);
```

`file.encoding` is **assigned** `"UTF-8"`. `stdout.encoding` is **derived** —
`sun.stdout.encoding`, then a value the platform native code computed (the
console encoding on Windows), then `native.encoding`. JEP 400 changed the first
and deliberately did not change the second. That is the whole distinction, and
getting it backwards would make this page worse than not writing it.

**`java/lang/System.java:1820-1822`** wires it up:

```java
setOut0(newPrintStream(fdOut, props.getProperty("stdout.encoding")));
initialErr = newPrintStream(fdErr, props.getProperty("stderr.encoding"));
```

and `newPrintStream` (`:1705`) is `new PrintStream(…, Charset.forName(enc, UTF_8))`
— falling back to UTF-8 only when the property is *absent*.

**`java/lang/System.java:603-611`**, the specification of the property itself:

> `stdout.encoding` — Character encoding name for `System.out` and
> `System.console()`. The Java runtime **can be started with the system property
> set to `UTF-8`**. Starting it with the property set to another value results
> in **unspecified behavior**.

Two consequences worth stating plainly:

1. The **default** is host-derived and is *not* UTF-8. A program is entitled to
   find `System.out.charset()` equal to the console's charset, and library code
   does (`Console.charset()`, `PrintWriter(OutputStream)` since JDK 19, every
   `?`-substituting logger).
2. `-Dstdout.encoding=UTF-8` — the pin used in §2 and throughout this repo's
   differential runs — is the **one** override the specification blesses. It is
   a supported workaround, not a hack.

`native.encoding`'s entry (`:595-598`) is the counterpart: host-derived, and
"setting this system property on the command line has no effect."

## 6. Which behaviour is right

**HotSpot's is the specified one.** §5 is not ambiguous: the default value of
`stdout.encoding` is derived from the host, and CratonVM's constant is a
different behaviour, not a different-but-conforming one. `native.encoding` is
stronger still — a constant contradicts the sentence that defines the property.

**And CratonVM's is, in isolation, the nicer behaviour.** This should be said
rather than waved away. Under CratonVM the characters survive: pipe the output
to a file, to `grep`, to another program, and you get correct UTF-8 instead of a
row of `?` that has destroyed information irrecoverably. The Windows console
code page is a 1990s artefact that JEP 400 spent an entire JEP working around
everywhere *except* here, and OpenJDK's own choice was contested at the time.

The argument that settles it is not aesthetic. It is that **a VM whose value
proposition is being a drop-in replacement does not get to relitigate a JDK-wide
policy question on its own.** A user who wants UTF-8 out of HotSpot passes
`-Dstdout.encoding=UTF-8`; the same flag should mean the same thing on
CratonVM, and today it is the only setting CratonVM has. The cost of the
divergence lands on tooling that has no way to know about it (§7), and it lands
silently — the output *looks* better, so nobody investigates.

There is also an asymmetry in the failure modes. If CratonVM follows the console
and someone wanted UTF-8, they get `?` and one documented flag fixes it. If
CratonVM keeps UTF-8 and someone wanted parity, they get a red differential
whose cause is invisible from the output — as demonstrated by §2's sixteen false
rows in an unrelated survey, which cost a whole re-measurement to retract.

## 7. `--diff-hotspot` will hit this, and its normalizers do not mask it

`docs/testing/diff-hotspot.md` landed on 2026-09-01 — the same day as this
finding — and exists specifically so that anyone can reproduce the
zero-divergence result. Checked against the implementation:

* **The child streams are decoded, not compared as bytes.**
  `vm-cli/src/main.rs:7578-7579` does
  `String::from_utf8_lossy(&stdout).into_owned()` on each side's raw output.
  HotSpot's `0x3f` is ASCII and survives as `?`; CratonVM's `c3 a9` decodes to
  `é`. The comparison then sees `hello, ??? world` against `hello, é中😀 world`.
  Worse, on a code page whose substitution is not `?` — a cp1252 `é` is byte
  `0xe9`, which is not valid UTF-8 — `from_utf8_lossy` turns HotSpot's own
  correct output into U+FFFD, so the report blames the VM for a defect in the
  harness's decoder.
* **None of the five maskers touches it.** `RELAX_RULES`
  (`vm-cli/src/main.rs:7815-7821`) is exactly `identity-hash`, `hex-address`,
  `thread-id`, `timestamp`, `absolute-path`. Nothing normalises character
  encoding, and by design the maskers only run as a second opinion after a
  byte-exact comparison has already failed — so the divergence is reported, at
  exit `1`, as a real finding.
* **The unconditional filters do not reach it either.** Only CRLF→LF, a
  trailing-whitespace trim, and CratonVM's own `[cratonvm]` / `[NativeBridge]` /
  `cratonvm_*` stderr chatter are removed.
* **`--diff-runs` cannot help.** The difference is perfectly reproducible on
  both sides, so the self-consistency check passes and the verdict is
  `DIVERGENCE`, not `unstable`.

The result: **on any Windows machine without a UTF-8 console, `cratonvm
--diff-hotspot` on a program that prints one non-ASCII character reports a
divergence that is not one.** That is a gap in a feature that just shipped, and
it is the strongest practical argument on this page — it is, after all, exactly
how the ten rows were found.

Two things would close the gap without touching `System.out` at all, and both
are separable from the compatibility judgement in §9:

1. **Say so in `docs/testing/diff-hotspot.md` §5 or §8**, with the workaround:
   both sides accept `-Dstdout.encoding=UTF-8` and it is forwarded to the
   reference JDK by the existing `-D` rule, so `cratonvm --diff-hotspot
   -Dstdout.encoding=UTF-8 …` is a one-token fix that is *specification-blessed
   on both sides* (§5).
2. **Detect it and say it in the report.** The mode already knows both command
   lines; a divergence whose two lines differ only in non-ASCII characters, on a
   host where the reference JDK's `stdout.encoding` is not UTF-8, could name the
   cause instead of printing a mystery. That is a hint, not a masker — silently
   normalising encodings would hide real charset defects, which are precisely
   what a JVM differential should catch.

## 8. Measured, reasoned, and not measured

**Measured** — by the orchestrator, on one Windows machine, against a freshly
built binary: the ten divergences of §1; that pinning
`-Dstdout.encoding=UTF-8 -Dfile.encoding=UTF-8` on both sides yields 0
divergences on both probes.

**Measured** — by this page, on `vm1` (Linux x86-64, Temurin 25.0.4+7),
**HotSpot only**: everything in §3.

**Read from source, not run**: §4 in full (CratonVM) and §5 in full (JDK 25
`src.zip` on the audit host). §7's four claims are read from
`vm-cli/src/main.rs` at `32f7d47c3`.

**Not measured, and not assumed:**

* **Which console code page the Windows machine had.** The `?` substitution is
  consistent with cp437, cp1251, cp1252 and others; this repo's own earlier
  record on a Windows host says `Cp1251`. The witness does not identify it and
  nothing here depends on which it was.
* **The redirected case on Windows.** `cratonvm … > out.txt` versus a console
  window was not run on either VM. HotSpot's Windows native code computes the
  console encoding through a different door than the ANSI code page, and whether
  redirecting stdout changes its answer — it is a long-standing Windows wart
  that a process attached to a console reports a console code page even when its
  own stdout is a pipe — is **not** established here. CratonVM's answer cannot
  change, because it is a constant.
* **stderr specifically on Windows.** All ten rows are stdout rows. CratonVM
  stamps `err` from the same call with the same literal (§4.2), so the same
  divergence is expected there; it was not observed.
* **A Windows host with "Use Unicode UTF-8 for worldwide language support"
  enabled.** That beta option sets the console and ANSI code pages to 65001, at
  which point HotSpot's `stdout.encoding` is UTF-8 and this divergence vanishes
  entirely. Untested. It means a second Windows machine can legitimately report
  zero divergences and prove nothing.
* **CratonVM under a non-UTF-8 locale.** No binary exists on the audit host and
  no build was permitted. §3's CratonVM column is a source reading.
* **Whether CratonVM honours `-Dstdout.encoding=<not UTF-8>`.** §4.2 says the
  charset stamped on `System.out` never reads the property, which predicts that
  `-Dstdout.encoding=ISO-8859-1` changes what `System.getProperty` reports and
  **not** what bytes appear — a self-inconsistency worse than the divergence
  itself. `probes/StdoutEncoding.java` settles it in one run: `prop
  stdout.encoding` and `stream out.charset` are printed side by side and must
  agree. Run it before designing any fix.

## 9. Options, and a recommendation — not implemented here

**A. Follow the host, as HotSpot does.** Derive `native.encoding` from the
console code page on Windows / the locale on Unix, derive `stdout.encoding` and
`stderr.encoding` from it per §5's chain, and stamp *that* charset in
`install_charset`.
*For:* the specified behaviour; `native.encoding` becomes conforming rather than
merely different; every golden-file and differential harness stops lying.
*Against:* CratonVM gets visibly *worse* at displaying text on legacy Windows
consoles, on purpose. The blast radius is every Windows user and every piece of
CratonVM-internal code that assumes stdout is UTF-8. Needs a new Windows native
call and a decision about the console-vs-pipe case §8 lists as unmeasured.

**B. Keep UTF-8 and document the divergence.** Change nothing; write it down in
`docs/testing/diff-hotspot.md` and the compatibility notes.
*For:* zero risk, zero work, and the current behaviour is genuinely more useful
to a human.
*Against:* leaves `native.encoding` contradicting its own specification, leaves
every third-party differential harness broken on Windows with no signal, and
leaves the trap that has already cost this repository one 30-row false
measurement.

**C. Make it a flag.** Keep one behaviour as default and offer the other behind
`CRATONVM_STDOUT_ENCODING` (or a `-XX:`-style token).
*For:* both audiences served; the A/B is available to whoever finally decides.
*Against:* a flag alone decides nothing — the default is still the whole
question, and an unexercised second path rots.

**Recommendation: A, with C as its kill switch, staged, and B's documentation
done first because it is free.**

1. **Now, and separable:** the two `diff-hotspot.md` items in §7. They cost a
   paragraph and they stop a just-shipped feature from producing false red on
   Windows. Nothing else in this list blocks them.
2. **Next:** run `probes/StdoutEncoding.java` on a built CratonVM binary, on
   Linux under `LC_ALL=C` and on the Windows machine both to a console and
   redirected. That closes four of the five unmeasured items in §8 in one
   sitting and, critically, settles whether `stdout.encoding` and
   `System.out.charset()` even agree with each other today (§8, last bullet). A
   fix designed before that measurement is a fix designed on a guess.
3. **Then:** fix `native.encoding` first, alone. It is host-derived by
   specification, the Unix derivation reuses locale code that already exists and
   already has the `LC_ALL` ▸ category ▸ `LANG` precedence right
   (`vm/src/vm/vm_init.rs:426-460`), and it is the fallback everything else
   derives from. It is also the only part of this page with no compatibility
   judgement in it.
4. **Then:** `stdout.encoding` / `stderr.encoding` and the `install_charset`
   stamp, together, behind an opt-out that restores today's unconditional UTF-8
   — registered in `types/tests/flag-surface.txt`, `docs/config/flag-inventory.md`
   and `docs/flag-tokens.md` like every other switch, and verified in both
   directions on the witness of §1.

The trade-off this rests on: the UTF-8-is-nicer argument is real but it is an
argument about what the *JDK* should do, and OpenJDK considered it and declined.
A clone that acts on its own answer buys a marginally better console at the cost
of the one property it cannot afford to lose — that a program, a test harness,
or a golden file cannot tell the two VMs apart. The kill switch costs one flag
and keeps the better console available to anyone who wants it.

## Reproduce

```bash
javac -encoding UTF-8 -d probes probes/StdoutEncoding.java

# HotSpot follows the host; CratonVM does not. The LC_ALL=C pair is the
# Windows witness, reproduced on Linux.
java             -cp probes StdoutEncoding
cratonvm         -cp probes StdoutEncoding
LC_ALL=C java    -cp probes StdoutEncoding
LC_ALL=C cratonvm -cp probes StdoutEncoding

# The pin that collapses it — the one override the specification blesses.
java     -Dstdout.encoding=UTF-8 -Dstderr.encoding=UTF-8 -cp probes StdoutEncoding
cratonvm -Dstdout.encoding=UTF-8 -Dstderr.encoding=UTF-8 -cp probes StdoutEncoding

# The harness gap of §7, and its one-token workaround.
cratonvm --diff-hotspot                          -cp probes StdoutEncoding
cratonvm --diff-hotspot -Dstdout.encoding=UTF-8  -cp probes StdoutEncoding
```

On Windows, run each VM twice — once to a console window, once with `> out.txt`
— and diff each VM against *itself*. HotSpot's answer is allowed to change when
stdout stops being a console. CratonVM's cannot.

## See also

* `docs/known-issues/jdk-only/bug-printstream-charset-answers-the-abstract-base-20260825.md`
  §5 — named this question and deferred it.
* `docs/known-issues/jdk-only/bug-file-path-normalisation-windows-arm-20260826.md`
  §4 — sixteen false rows from this same mechanism, and the rule that a cross-VM
  stdout diff must not carry an encoding-dependent byte.
* `docs/testing/diff-hotspot.md` §5 — the maskers, and where this belongs.
* `probes/StdoutEncoding.java` — the witness.
