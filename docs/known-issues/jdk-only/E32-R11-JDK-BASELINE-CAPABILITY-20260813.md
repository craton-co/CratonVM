# E32 / R11 — the missing capability, built: checked-in JDK surface baselines and the generator that writes them

**Date:** 2026-08-13 **Lane:** E32
**Closes the capability gap named in:** `E25-R11-GUARD-POPULATION-SWEEP-20260813.md` §2.1 and §7.
**Status:** generator and baselines LANDED and self-verified; the Rust consumer is
NOMINATED, not written.
**Prov:** every number below is measured on this host today against
`openjdk 25.0.3 2026-04-21 LTS, Microsoft-13877124, build 25.0.3+9-LTS`.
**This lane did not build or run CratonVM, and did not run `cargo`.** No `.rs`
file was written.

E25 §7, in its own words:

> Across `native-builtins/src/` — 30 guards of this shape — **not one test reads
> a checked-in baseline file, and not one invokes `javap`.** Every expected set
> in the crate was typed by a person reading the code beside it. That is not
> thirty independent lapses; it is one missing capability, thirty times.

This is that capability, as data plus a generator.

---

## 1. What landed

| path | what |
|---|---|
| `scripts/jdk-baseline/JdkBaseline.java` | the oracle — reads `jrt:/modules/<module>/<binary/name>.class` out of the running JDK and parses it with `java.lang.classfile` |
| `scripts/jdk-baseline/classes.txt` | the population of baselines, one line per class/module, each annotated with the E25 sweep row it serves |
| `scripts/jdk-baseline/generate.py` | the driver — compile-cache, `--update`, `--check`, `--verify`; stdlib only |
| `scripts/baselines/jdk25-*.tsv` | **30 baselines**, 148 KB total |

Nothing else was touched. `scripts/baselines/README.md`, `.gitattributes`,
`native-builtins/`, and every guard named in E25 are **nominations** in §7.

---

## 2. The format, and why it is this format

One file per class, `scripts/baselines/jdk25-<binary.name>.tsv`. Nested classes
keep the binary `$`, so the filename is a mechanical function of the class name
and a Rust test can compute it: `jdk25-java.util.Base64$Encoder.tsv`. (A shell
consumer must single-quote the path; `$` is the only character in these names
that needs it.)

```
# jdk-baseline	1
# kind	class
# name	javax.crypto.Mac
# module	java.base
# java.version	25.0.3
# java.vm.version	25.0.3+9-LTS
# java.vendor.version	Microsoft-13877124
# source	jrt:/modules/java.base/javax/crypto/Mac.class
# generator	scripts/jdk-baseline/generate.py
# regenerate	python scripts/jdk-baseline/generate.py --update
# public-methods	17
# protected-methods	0
# rows	23
# columns	kind	name	descriptor	flags
CLASS	javax.crypto.Mac		public
EXTENDS	java.lang.Object		
IMPLEMENTS	java.lang.Cloneable		
SUPERTYPE	java.lang.Cloneable		
SUPERTYPE	java.lang.Object		
METHOD	<init>	(Ljavax/crypto/MacSpi;Ljava/security/Provider;Ljava/lang/String;)V	protected
METHOD	clone	()Ljava/lang/Object;	public,final
METHOD	doFinal	()[B	public,final
…
METHOD	getInstance	(Ljava/lang/String;Ljava/security/Provider;)Ljavax/crypto/Mac;	public,static,final
…
```

**Grammar.** Lines beginning `# ` are header `key<TAB>value`. Every other
non-blank line is exactly four tab-separated fields:
`kind<TAB>name<TAB>descriptor<TAB>flags`. Kinds are `CLASS`, `EXTENDS`,
`IMPLEMENTS`, `SUPERTYPE`, `METHOD`, `FIELD`. For a module baseline the third
column is `target` and the kinds are `REQUIRES`, `EXPORTS`, `OPENS`, `USES`,
`PROVIDES`, `PACKAGE`.

**Why TSV, argued rather than assumed.** The consumer is a Rust `#[cfg(test)]`
block in a crate that must not gain a dependency for this. TSV parses to a
complete, typed row in six tokens of `std`:

```rust
let mut f = line.split('\t');
```

JSON needs `serde`; YAML needs more; a `const` array of tuples needs the
generator to emit Rust, which makes a regeneration a source edit and drags the
formatter and the compiler into the loop. And no field can contain a tab: a JVM
descriptor, a binary name, a package name and a module name are each drawn from
alphabets that exclude it, so TSV here needs **no quoting and no escaping** —
which is the property that makes a hand-diffable format safe. `.tsv` also
matches the four baselines already in this directory.

**Why the class file and not `javap` text.** Three reasons, each of which bit
something in this tree:

1. `javap` prints a **source-level** signature and puts the JVM descriptor on a
   separate `descriptor:` line. Pairing the two means parsing prose. The class
   file carries the descriptor as one constant-pool string — the exact key
   `NativeMethodRegistry::register(class, name, descriptor)` uses.
2. Reflection cannot load a class whose class-file minor version is 65535. Every
   JEP 505 `StructuredTaskScope` type in JDK 25 is preview-versioned, and those
   are exactly the classes E25 row 16 needs an oracle for. Reading the bytes has
   no such restriction — and `--enable-preview` is not needed either.
3. `jdk.internal.access.SharedSecrets` sits in a package `java.base` does not
   export. Reading its bytes needs no `--add-exports`. A generator that needs
   flags is a generator a CI job will run without them.

**Why `SUPERTYPE` rows exist.** `javap -public` never lists an inherited member,
so a guard reading only the leaf class reads an inherited method as *absent*.
The transitive closure is emitted so a consumer can tell "not declared here" from
"not in the JDK at all", and `classes.txt` carries the four supertypes the
`SynchronousQueue` / `SSLSocket` / `SubmissionPublisher` guards actually reach.

**Determinism, and what is deliberately volatile.** Every collection is sorted
(`TreeSet`) before emission; rows are joined with an explicit `'\n'`, never
`System.lineSeparator()`, and written UTF-8. **No timestamp is emitted.** The
only volatile fields are the JDK's own three version strings, and they are there
on purpose: §5 step 4 of E25 asked that "JDK 26 adds one" stop being an
unwatched residual, and a version string in the header is what makes a JDK bump
a diff instead of silence. `--check` classifies a header-only difference as
`JDK-BUMP` and a row difference as `DRIFT`, because those need different human
responses.

---

## 3. Regenerating and checking

```
python scripts/jdk-baseline/generate.py            # --check: read-only, the CI default
python scripts/jdk-baseline/generate.py --update   # rewrite scripts/baselines/jdk25-*.tsv
python scripts/jdk-baseline/generate.py --verify   # the generator's known-answer test only
```

Exit `0` agree, `1` drift, `2` environment (no JDK 25+, compile failed).

`--check` never writes. It regenerates into a temp directory and reports four
things: `MISSING` (the generator emits a baseline nobody checked in), `STALE` (a
checked-in baseline no `classes.txt` entry produces — a file that can never go
red again), `JDK-BUMP`, and `DRIFT` with the added and removed rows printed
individually.

`--update` does **not** delete a stale file; it names it and leaves it. Removing
a baseline takes a guard's population with it, and that must be a deliberate
edit.

The Java source is compiled once per source revision into
`%TEMP%/jdk-baseline-<sha16>/`, outside the repo. Single-file source mode
(`java JdkBaseline.java`) recompiles every run — measured at ~51 s on this host —
which would have been the entire cost of a `--check`.

---

## 4. (c) — the verification, and its output

Two counts were measured **by hand, with `javap`, before this generator existed**:
`javax.crypto.Mac` = 17 public methods (E25 §1.1, re-taken here) and
`java.lang.Character` = 96.

```
$ javap -public javax.crypto.Mac | grep -c '('
17
$ javap -public java.lang.Character | grep -c '('
97
$ javap -public java.lang.Character | grep 'Character('
  public java.lang.Character(char);
```

97 lines minus the one public (deprecated) constructor = **96 public methods**.

The generator's known-answer test asserts both, and asserts them **twice** — once
against the `# public-methods` header, and once by recounting the `METHOD` rows,
because a header count checked against the program that wrote it is exactly the
restatement shape E25 is about:

```
$ python scripts/jdk-baseline/generate.py --update
--- generator known-answer test (see KNOWN_ANSWERS for provenance) ---
  OK   java.lang.Character: expected 96, header says 96, rows count 96
  OK   javax.crypto.Mac: expected 17, header says 17, rows count 17
wrote scripts/baselines/jdk25-java.lang.Character.tsv
… 30 files …

$ python scripts/jdk-baseline/generate.py --check
--- generator known-answer test (see KNOWN_ANSWERS for provenance) ---
  OK   java.lang.Character: expected 96, header says 96, rows count 96
  OK   javax.crypto.Mac: expected 17, header says 17, rows count 17
OK: 30 baselines in scripts\baselines agree with this JDK.
```

**The KAT runs in every mode, including `--update`.** A generator that cannot
reproduce two measured counts must not be allowed to overwrite a file that other
tests will then treat as the JDK's own answer.

**These two numbers are a known-answer test for the instrument, not a
completeness guard, and must never be read as one.** They are the only
hand-written literals in the whole capability, and they exist so that the
generated data has a fixed point outside itself. The distinction matters: E25's
finding is about populations transcribed from the code under test; a KAT is
measured from a *different* instrument (`javap`) than the one under test (the
class-file reader) and is the standard way to keep a generator honest.

**One decision the 96 forced.** `java.lang.Character` declares a bridge
`compareTo(Ljava/lang/Object;)I`. It is in `javap`'s 97 and it is in the
baseline. A generator that filtered bridges would answer 95 and quietly disagree
with `javap` — and it would be wrong on the merits too, because the bridge is a
real entry in the method table and a real dispatch target this VM must answer
for.

---

## 5. The denominators, now machine-derived

E25 measured seven denominators by hand. **All seven reproduce exactly** from the
generated baselines — the transcription was accurate, and it is now mechanical:

| class | E25 §3/§4 said | derived from the baseline | agrees |
|---|---|---|---|
| `javax.net.ssl.SSLParameters` | 13 of **31** | 28 public methods + 3 public ctors = 31 | yes |
| `java.security.KeyStore` | ~13 of **30** | 30 public methods (ctor is protected) | yes |
| `java.lang.management.RuntimeMXBean` | 13 of **17** | 17 public methods | yes |
| `java.lang.management.ManagementFactory` | 7 of **25** | 16 public methods + 9 public fields = 25 | yes |
| `jdk.internal.access.SharedSecrets` | 15 of **30** | 30 `METHOD getJava*Access` rows (of 64 public methods) | yes |
| `java.util.concurrent.SubmissionPublisher` | 4 of 11; **20** public members | 17 public methods + 3 public ctors = 20 | yes |
| `java.util.concurrent.SynchronousQueue` | 5 of 17; **25** public members | 23 public methods + 2 public ctors = 25 | yes |
| `java.base` exports | 14 vs **58** | `# unqualified-exports 58` (plus 166 qualified rows) | yes |

Two figures E25 did not have, now checked in: `javax.crypto.Mac` 17 and
`java.lang.Character` 96 public methods.

**A note the consumer needs:** `JAVA_BASE_EXPORTS` (`jdk25_language.rs:32`)
spells packages in **internal** form (`java/util/stream`); the module baseline
uses **binary** form (`java.util.stream`). A guard reading one against the other
must map, and saying so here is cheaper than the next lane rediscovering it.

---

## 6. THE FAILURE MODE — a baseline a test merely reads is not enough

This is the part that decides whether the capability is worth having.

E25's whole subject is guards whose expected set was transcribed from the code
under test. A checked-in baseline removes the *transcription*. It does **not**
remove the decay mode, and the decay mode is what turned E20's `already_triaged`
list into standing approval for three defects that had already been fixed. A
one-way read — "for each thing I registered, assert the JDK has it" — is the same
guard with a new data source.

**The guard must be a two-way ratchet with four distinct failure kinds**, the
shape `phase59_module_vs_essential_natives` (`phases_late.rs:10286`) already has
in this tree:

| # | condition | what it means | message must say |
|---|---|---|---|
| 1 | a baseline row with **no** triage row | the JDK declares a member this guard has never considered | "the JDK declares X. Register it, or add a row with `expect_registered: false` and a reason." |
| 2 | `expect_registered: true` and **not** registered | a registration was dropped | "X was registered and is not any more." |
| 3 | `expect_registered: false` and **registered** | the gap closed and the record did not | "X is registered now — flip the row to `true` and delete the reason. A closed gap recorded as open is standing permission." |
| 4 | a triage row naming a member **not in the baseline** | the row has rotted; the JDK removed it, or it never existed | "X is not on this class on this JDK. Delete the row." |

Kind 3 and kind 4 are the halves that get left out, and they are the ones that
matter here. Kind 3 is why E25 opens with a **BLOCKING** nomination: the moment
`Mac.getInstance(String,Provider)` was registered, the census that carried it as
an explicit `false` row went red — correctly. Kind 4 is the direction that
caught `StructuredTaskScope$Config`, below.

Three supporting properties the consumer must also have, none of them optional:

* **Anti-vacuity.** Assert the parsed baseline is non-empty and that its
  `# jdk-baseline` version is one the parser knows. An `include_str!` of a
  truncated or reformatted file must fail loudly, not silently audit zero rows.
  E25 row 32 is a "regression gate" that returns green in 0.00 s with no corpus;
  a guard that audits an empty population is the same thing.
* **A parser self-test on the data.** Recount the public methods from the rows
  and compare against the `# public-methods` header. Two independent counts of
  the same file disagreeing means the file was hand-edited or the parser is
  wrong — and the header is written by a different program than the one reading
  it, so this is a real cross-check, not a restatement.
* **A JDK-version pin.** Assert `# java.version` starts with `25.`. A baseline
  regenerated on JDK 26 silently re-baselines every guard that reads it; the pin
  makes the bump a decision.

**Never hand-edit a row.** `scripts/baselines/README.md`'s existing rule applies
here verbatim: every entry is written by the generator from a census it took. A
row typed by a person is the defect these files exist to remove, reintroduced one
line at a time.

---

## 7. NOMINATIONS

### NOM E32-1 — `.gitattributes` — **apply before any Rust test reads these files**

`core.autocrlf=true` in this repo and `.gitattributes` pins
`scripts/baselines/*.txt` to LF but says nothing about `*.tsv` — the four
existing `.tsv` baselines are checked out **CRLF** on Windows and LF on Linux.
The generator always writes LF, so without this, `--check` diverges by platform
for a difference that is not in the data, and `[CRLFwit]`/`[batch-in]` say what
happens next. `scripts/jdk-baseline/generate.py` and the nominated Rust parser
are both CR-tolerant so nothing breaks today; this makes the bytes match the
generator.

OLD (verified unique — it is the last stanza of the file):

```
scripts/baselines/*.txt text eol=lf
```

NEW:

```
scripts/baselines/*.txt text eol=lf

# Same hazard for the JDK surface baselines. `scripts/jdk-baseline/generate.py`
# writes them with an explicit '\n' so the file is byte-identical on every
# platform, and its `--check` mode diffs the working tree against a fresh
# generation. With core.autocrlf=true and no pin here, the checked-out copy is
# CRLF on Windows and LF on Linux while the generator's output is always LF, so
# the check would answer differently on the two hosts for a difference that is
# not in the data. Both readers trim_end() anyway; this makes the bytes match.
scripts/baselines/jdk25-*.tsv text eol=lf
```

### NOM E32-2 — `scripts/baselines/README.md` — the new files need a row in the table

OLD:

```
| `jdk-only-bridge-ratchet.json` | `scripts/internal/jdk-only-bridge-ratchet.py` (gitignored) — the unadjudicated-`Bridge` ratchet (wave-2 lane L6) | [`regression-suite/bridge-ratchet.sh`](../../regression-suite/bridge-ratchet.sh) |
```

NEW:

```
| `jdk-only-bridge-ratchet.json` | `scripts/internal/jdk-only-bridge-ratchet.py` (gitignored) — the unadjudicated-`Bridge` ratchet (wave-2 lane L6) | [`regression-suite/bridge-ratchet.sh`](../../regression-suite/bridge-ratchet.sh) |
| `jdk25-<binary.name>.tsv`, `jdk25-module-<name>.tsv` | the public/protected surface of one JDK 25 class or module, for any guard whose expected set would otherwise be transcribed from the registrar it audits | [`scripts/jdk-baseline/generate.py`](../jdk-baseline/generate.py) — `--update` writes, `--check` diffs, `--verify` runs its known-answer test |
```

The "Never hand-edit a number" rule already on that page applies unchanged, and
one sentence is worth adding under it:

```
A `jdk25-*.tsv` is not a number but a population, and the rule is the same: it
is written by `scripts/jdk-baseline/generate.py` from the running JDK's own
image. A row typed by a person is the defect these files exist to remove.
```

### NOM E32-3 — `native-builtins/src/jdk_baseline.rs` — **NEW FILE**, the helper a test calls

The whole consuming surface. Two functions and a type; no dependency; the
`include_str!` path is written once per class so a guard never spells a path.

```rust
//! Reads the checked-in JDK 25 surface baselines in `scripts/baselines/`.
//!
//! E25-R11-GUARD-POPULATION-SWEEP-20260813.md §7 found thirty guards in this
//! crate whose expected set was transcribed from the registrar they audit, and
//! named the cause: "not one test reads a checked-in baseline file, and not one
//! invokes `javap`". This module is the read side of the fix. The write side is
//! `scripts/jdk-baseline/generate.py`, which emits the files from
//! `jrt:/modules/<module>/<binary/name>.class` on openjdk 25.0.3+9; see
//! docs/known-issues/jdk-only/E32-R11-JDK-BASELINE-CAPABILITY-20260813.md.
//!
//! A guard built on this must use [`audit`], not a bare membership loop. A
//! one-way read is the same restatement with a new data source: it cannot
//! notice a gap that closed (kind 3) or a triage row that rotted (kind 4), and
//! those are the two directions E20 found already broken the first time anyone
//! looked. `[both halves]`.

use std::collections::BTreeSet;

// ---------------------------------------------------------------------------
// The baselines. One line per class; the file name is a mechanical function of
// the binary name, so adding a class is one `include_str!` and nothing else.
// Regenerate with `python scripts/jdk-baseline/generate.py --update`.
// ---------------------------------------------------------------------------

pub(crate) const SUBMISSION_PUBLISHER: &str =
    include_str!("../../scripts/baselines/jdk25-java.util.concurrent.SubmissionPublisher.tsv");
pub(crate) const SYNCHRONOUS_QUEUE: &str =
    include_str!("../../scripts/baselines/jdk25-java.util.concurrent.SynchronousQueue.tsv");
pub(crate) const SSL_PARAMETERS: &str =
    include_str!("../../scripts/baselines/jdk25-javax.net.ssl.SSLParameters.tsv");
pub(crate) const KEY_STORE: &str =
    include_str!("../../scripts/baselines/jdk25-java.security.KeyStore.tsv");
pub(crate) const RUNTIME_MXBEAN: &str =
    include_str!("../../scripts/baselines/jdk25-java.lang.management.RuntimeMXBean.tsv");
pub(crate) const SHARED_SECRETS: &str =
    include_str!("../../scripts/baselines/jdk25-jdk.internal.access.SharedSecrets.tsv");
pub(crate) const MAC: &str =
    include_str!("../../scripts/baselines/jdk25-javax.crypto.Mac.tsv");

/// One row of a baseline file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Row {
    /// `CLASS`, `EXTENDS`, `IMPLEMENTS`, `SUPERTYPE`, `METHOD`, `FIELD`.
    pub kind: &'static str,
    pub name: &'static str,
    pub descriptor: &'static str,
    /// Comma-separated, in a fixed order: `public,static,final,bridge,…`.
    pub flags: &'static str,
}

impl Row {
    pub(crate) fn has_flag(&self, f: &str) -> bool {
        self.flags.split(',').any(|x| x == f)
    }
}

pub(crate) struct Baseline {
    /// The binary name, from the `# name` header.
    pub class: &'static str,
    /// From the `# java.version` header, e.g. `25.0.3`.
    pub java_version: &'static str,
    pub rows: Vec<Row>,
}

/// Parse a baseline, validating it against its own header.
///
/// Panics rather than returning an error: every caller is a `#[test]`, and a
/// baseline that will not parse must not degrade into an empty population that
/// audits nothing. E25 row 32 is a "regression gate" that returns green in
/// 0.00 s with no corpus; a guard auditing zero rows is the same thing.
pub(crate) fn parse(text: &'static str) -> Baseline {
    let mut class = "";
    let mut java_version = "";
    let mut format = "";
    let mut declared_public_methods: Option<usize> = None;
    let mut declared_rows: Option<usize> = None;
    let mut rows: Vec<Row> = Vec::new();

    // `str::lines` already splits on "\r\n"; the extra trim covers a lone '\r'
    // and costs nothing. See NOM E32-1: `.gitattributes` does not pin these
    // files to LF, so on Windows the checked-out copy is CRLF.
    for line in text.lines().map(|l| l.trim_end_matches('\r')) {
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("# ") {
            let mut kv = rest.splitn(2, '\t');
            let k = kv.next().unwrap_or("");
            let v = kv.next().unwrap_or("");
            match k {
                "jdk-baseline" => format = v,
                "name" => class = v,
                "java.version" => java_version = v,
                "public-methods" => declared_public_methods = v.parse().ok(),
                "rows" => declared_rows = v.parse().ok(),
                _ => {}
            }
            continue;
        }
        let mut f = line.split('\t');
        rows.push(Row {
            kind: f.next().unwrap_or(""),
            name: f.next().unwrap_or(""),
            descriptor: f.next().unwrap_or(""),
            flags: f.next().unwrap_or(""),
        });
    }

    assert_eq!(
        format, "1",
        "unknown jdk-baseline format version {format:?}. The row grammar \
         changed under this parser; read \
         docs/known-issues/jdk-only/E32-R11-JDK-BASELINE-CAPABILITY-20260813.md \
         §2 before widening this check."
    );
    assert!(
        !class.is_empty() && !rows.is_empty(),
        "empty or headerless baseline. An include_str! that resolved to nothing \
         would otherwise audit a population of zero and pass."
    );
    assert!(
        java_version.starts_with("25."),
        "this baseline was taken on java.version {java_version:?}, not a 25.x. \
         Regenerating on a newer JDK silently re-baselines every guard that \
         reads it; make the bump a decision, not a side effect."
    );
    assert_eq!(
        Some(rows.len()),
        declared_rows,
        "{class}: the file's own `# rows` header disagrees with the rows this \
         parser found. The file was hand-edited or truncated."
    );

    let b = Baseline { class, java_version, rows };
    assert_eq!(
        Some(b.public_methods().len()),
        declared_public_methods,
        "{class}: the file's own `# public-methods` header disagrees with a \
         recount of its rows. Two programs counting the same file must agree."
    );
    b
}

impl Baseline {
    /// Public, non-constructor methods, as the `(name, descriptor)` pair
    /// `NativeMethodRegistry::find` is keyed by. Bridges are included: a bridge
    /// is a real method-table entry and a real dispatch target.
    pub(crate) fn public_methods(&self) -> Vec<(&'static str, &'static str)> {
        self.rows
            .iter()
            .filter(|r| {
                r.kind == "METHOD"
                    && r.has_flag("public")
                    && r.name != "<init>"
                    && r.name != "<clinit>"
            })
            .map(|r| (r.name, r.descriptor))
            .collect()
    }

    /// Public methods **and** public constructors — the population a native
    /// registrar can plausibly cover, since `<init>` is a registrable name.
    pub(crate) fn public_surface(&self) -> Vec<(&'static str, &'static str)> {
        self.rows
            .iter()
            .filter(|r| r.kind == "METHOD" && r.has_flag("public") && r.name != "<clinit>")
            .map(|r| (r.name, r.descriptor))
            .collect()
    }

    pub(crate) fn declares(&self, name: &str, descriptor: &str) -> bool {
        self.rows
            .iter()
            .any(|r| r.kind == "METHOD" && r.name == name && r.descriptor == descriptor)
    }

    /// Every transitive supertype, so "not declared here" can be told apart
    /// from "not in the JDK at all".
    pub(crate) fn supertypes(&self) -> Vec<&'static str> {
        self.rows
            .iter()
            .filter(|r| r.kind == "SUPERTYPE")
            .map(|r| r.name)
            .collect()
    }
}

/// One triage row: `(name, descriptor, expect_registered, reason)`.
///
/// `reason` must be non-empty when `expect_registered` is `false` and empty
/// when it is `true` — an absence with no stated reason is an omission wearing
/// a record's clothes.
pub(crate) type Triage = (&'static str, &'static str, bool, &'static str);

/// The two-way ratchet. Returns every disagreement; an empty vec is the pass.
///
/// Four failure kinds, all four required. A guard that reports only kinds 1 and
/// 2 cannot notice a gap that closed or a row that rotted, which is how E20
/// found three of six `already_triaged` rows granting standing approval for
/// defects that were already fixed.
pub(crate) fn audit(
    baseline: &Baseline,
    triage: &[Triage],
    registered: impl Fn(&str, &str) -> bool,
) -> Vec<String> {
    let mut problems: Vec<String> = Vec::new();
    let jdk: BTreeSet<(&'static str, &'static str)> =
        baseline.public_surface().into_iter().collect();
    let mut seen: BTreeSet<(&'static str, &'static str)> = BTreeSet::new();
    let class = baseline.class;

    for &(name, descriptor, expect, reason) in triage {
        if !seen.insert((name, descriptor)) {
            problems.push(format!("DUPLICATE row: {class}.{name}{descriptor}"));
            continue;
        }
        // Kind 4 — the row rotted.
        if !jdk.contains(&(name, descriptor)) {
            problems.push(format!(
                "STALE row: {class}.{name}{descriptor} is not a public member of \
                 that class on java.version {}. Delete the row — a triage row \
                 for a member the JDK does not have can never be satisfied and \
                 can never be noticed.",
                baseline.java_version
            ));
            continue;
        }
        match (expect, registered(name, descriptor)) {
            // Kind 2 — a registration was dropped.
            (true, false) => problems.push(format!(
                "DROPPED: {class}.{name}{descriptor} is recorded as registered \
                 and is not. A registration was lost."
            )),
            // Kind 3 — the gap closed and the record did not.
            (false, true) => problems.push(format!(
                "CLOSED: {class}.{name}{descriptor} is registered now, but this \
                 row still records it as absent (\"{reason}\"). Flip the row to \
                 `true` and delete the reason. A closed gap recorded as open is \
                 standing permission."
            )),
            (false, false) if reason.trim().is_empty() => problems.push(format!(
                "UNJUSTIFIED: {class}.{name}{descriptor} is recorded as absent \
                 with no reason. Say whether it is unimplemented or deliberately \
                 out of scope; an unexplained `false` is an omission, not a record."
            )),
            (true, true) if !reason.trim().is_empty() => problems.push(format!(
                "NOISY: {class}.{name}{descriptor} is registered; its `reason` \
                 field must be empty so a non-empty reason always means an \
                 absence."
            )),
            _ => {}
        }
    }

    // Kind 1 — the JDK declares it and this guard has never considered it.
    for &(name, descriptor) in &jdk {
        if !seen.contains(&(name, descriptor)) {
            problems.push(format!(
                "UNCOVERED: the JDK declares {class}.{name}{descriptor} and this \
                 census has no row for it. Register it, or add a row with \
                 `false` and a reason. This is the direction that made every \
                 guard in E25 §3 a restatement: a population transcribed from \
                 the registrar cannot contain a method the registrar never had."
            ));
        }
    }

    problems.sort();
    problems
}
```

And the module declaration. OLD (`native-builtins/src/lib.rs:4586`, verified
unique):

```rust
#[cfg(test)]
pub(crate) mod test_utils;
```

NEW:

```rust
#[cfg(test)]
pub(crate) mod test_utils;

/// Reads `scripts/baselines/jdk25-*.tsv`. Test-only: the baselines are an
/// oracle for guards, never an input to the VM.
#[cfg(test)]
pub(crate) mod jdk_baseline;
```

### NOM E32-4 — `native-builtins/src/phases_late.rs:8989` — the worked example

E25 row 20, chosen because it is the worst: *"`closeExceptionally` and
`getClosedException` — the two methods `register_p69_submission_publisher`
exists to add — are asserted by nothing."*

Verified in the working tree today: `register_p60_flow`
(`phases_late/concurrent.rs:2510,2516,2531,2552,2578,2582,2587,2592`) makes **8**
`SubmissionPublisher` registrations and `register_p69_submission_publisher`
(`:5705,5713,5769`) makes **3**. Eleven. The guard names four. The baseline says
the JDK declares twenty.

OLD (verified unique in the working tree today):

```rust
    #[test]
    fn b6_submission_publisher_core_methods_registered() {
        let r = sp_registry();
        let sp = "java/util/concurrent/SubmissionPublisher";
        assert!(r.find(sp, "submit", "(Ljava/lang/Object;)I").is_some());
        assert!(r
            .find(sp, "subscribe", "(Ljava/util/concurrent/Flow$Subscriber;)V")
            .is_some());
        assert!(r.find(sp, "hasSubscribers", "()Z").is_some());
        assert!(r.find(sp, "getNumberOfSubscribers", "()I").is_some());
    }
```

NEW:

```rust
    /// The population is **the JDK's**, not this registrar's.
    ///
    /// `scripts/baselines/jdk25-java.util.concurrent.SubmissionPublisher.tsv`,
    /// generated by `scripts/jdk-baseline/generate.py` from
    /// `jrt:/modules/java.base/java/util/concurrent/SubmissionPublisher.class`
    /// on openjdk 25.0.3+9: 17 public methods plus 3 public constructors = 20
    /// members. `register_p60_flow` makes 8 of them and
    /// `register_p69_submission_publisher` makes 3; the body this replaced
    /// named **four**, and neither of the two methods p69 exists to add
    /// (`closeExceptionally`, `getClosedException`) was among them — so the
    /// test could not have gone red if p69 had been deleted outright
    /// (E25-R11-GUARD-POPULATION-SWEEP-20260813.md row 20).
    ///
    /// Every row below is one of the JDK's twenty. `audit` fails four ways:
    /// a JDK member with no row, a `true` row that is not registered, a `false`
    /// row that IS registered, and a row naming something this JDK does not
    /// declare. The third and fourth are the ones a hand list never has.
    #[test]
    fn b6_submission_publisher_surface_is_triaged_against_the_jdk() {
        use crate::jdk_baseline;

        let r = sp_registry();
        let sp = "java/util/concurrent/SubmissionPublisher";
        let baseline = jdk_baseline::parse(jdk_baseline::SUBMISSION_PUBLISHER);
        assert_eq!(baseline.class, "java.util.concurrent.SubmissionPublisher");

        #[rustfmt::skip]
        const TRIAGE: &[jdk_baseline::Triage] = &[
            // --- register_p60_flow (8) ---
            ("<init>", "()V", true, ""),
            ("submit", "(Ljava/lang/Object;)I", true, ""),
            ("offer", "(Ljava/lang/Object;Ljava/util/function/BiPredicate;)I", true, ""),
            ("close", "()V", true, ""),
            ("isClosed", "()Z", true, ""),
            ("hasSubscribers", "()Z", true, ""),
            ("getNumberOfSubscribers", "()I", true, ""),
            ("subscribe", "(Ljava/util/concurrent/Flow$Subscriber;)V", true, ""),
            // --- register_p69_submission_publisher (3) ---
            // These two are the reason p69 exists and were asserted by NOTHING
            // until this rewrite.
            ("closeExceptionally", "(Ljava/lang/Throwable;)V", true, ""),
            ("getClosedException", "()Ljava/lang/Throwable;", true, ""),
            ("getMaxBufferCapacity", "()I", true, ""),
            // --- the nine the JDK declares and this VM does not register ---
            // UNTRIAGED, deliberately: each `false` below states what is known
            // and does not guess at intent. A reason that says "not looked at"
            // is worth more than a confident one that is wrong, and this lane
            // could not run the VM to find out which of these a real
            // SubmissionPublisher user reaches.
            ("<init>", "(Ljava/util/concurrent/Executor;I)V", false,
             "Only the no-arg constructor is registered; concurrent.rs:5701's own \
              comment says so. A caller using the Executor form gets the real JDK \
              body and a publisher with no side-table row, which is the shape E25 \
              §1.2 describes for Mac."),
            ("<init>", "(Ljava/util/concurrent/Executor;ILjava/util/function/BiConsumer;)V", false,
             "As the two-arg constructor above."),
            ("offer", "(Ljava/lang/Object;JLjava/util/concurrent/TimeUnit;Ljava/util/function/BiPredicate;)I", false,
             "The TIMED offer. The untimed overload IS registered, so a caller \
              that adds a timeout crosses from a native to the real JDK body \
              mid-API. Unmeasured."),
            ("consume", "(Ljava/util/function/Consumer;)Ljava/util/concurrent/CompletableFuture;", false,
             "Not registered; not measured."),
            ("estimateMaximumLag", "()I", false, "Not registered; not measured."),
            ("estimateMinimumDemand", "()J", false, "Not registered; not measured."),
            ("getExecutor", "()Ljava/util/concurrent/Executor;", false,
             "Not registered; no executor is modelled by the registered <init>()V."),
            ("getSubscribers", "()Ljava/util/List;", false,
             "Not registered, although `getNumberOfSubscribers` IS and reads the \
              same field-0 subscriber list. The two answers can disagree."),
            ("isSubscribed", "(Ljava/util/concurrent/Flow$Subscriber;)Z", false,
             "Not registered; same list as `getSubscribers`."),
        ];

        let problems = jdk_baseline::audit(&baseline, TRIAGE, |name, descriptor| {
            r.find(sp, name, descriptor).is_some()
        });
        assert!(
            problems.is_empty(),
            "SubmissionPublisher's registered surface disagrees with \
             scripts/baselines/jdk25-java.util.concurrent.SubmissionPublisher.tsv \
             ({} problems):\n  {}",
            problems.len(),
            problems.join("\n  ")
        );
    }
```

**PREDICTED effect: passes as the tree stands**, and it is PREDICTED, not
measured — this lane did not run `cargo`. The eleven `true` rows are the eleven
`r.register(sp, …)` calls read out of `phases_late/concurrent.rs` today; the nine
`false` rows are the JDK's twenty minus those eleven, taken from the baseline.
If it goes red, the failure text names the exact triple and the direction, and
the first thing to check is whether a registration moved between p60 and p69
rather than whether the baseline is wrong.

**What it now catches that the old body could not:** deleting
`register_p69_submission_publisher` entirely (three `DROPPED`); registering the
timed `offer` without recording it (one `CLOSED`); and — on the next JDK — a
21st member arriving with no row (`UNCOVERED`) or one of these twenty being
removed (`STALE`).

### NOM E32-5 — the remaining E25 rows now have a population to read

Rows 2, 4-6, 7, 8, 10, 13, 20, 21 of the E25 sweep each have a baseline checked
in as of today. No replacement literal is offered for them, for the reason E25
gave for NOM E25-7 and repeats here: closing each one means deciding, per absent
member, whether it is unimplemented or deliberately out of scope, and that
judgement belongs to the lane that owns the file. The population and the
denominator were the missing half; they are now `include_str!` away.

`jdk.internal.access.SharedSecrets` (NOM E25-7) is the clearest next one: the
baseline carries all **30** `getJava*Access` rows, `FACTORIES` carries 15, and
`audit` with the other 15 as `false` rows converts "15 of 30, unstated" into
fifteen recorded decisions.

---

## 8. What could NOT be baselined, and why

**`java.util.concurrent.StructuredTaskScope$Config` — because it does not
exist.** The generator refused it:

```
Exception in thread "main" java.lang.IllegalStateException: NOT IN THE RUNTIME
IMAGE: java.util.concurrent.StructuredTaskScope$Config (searched 70 modules
under jrt:/modules)
```

`jdk25_concurrency.rs:1524` defines `CLS_CONFIG` as
`"java/util/concurrent/StructuredTaskScope$Config"`, `s52_class_name_config`
(`:5103`) asserts that spelling, and `s52_total_registration_count` (`:5113`)
counts six methods on it. JDK 25's nested type is `$Configuration`
(`C:\craton\jdk25src\java.base\java\util\concurrent\StructuredTaskScope.java:752`,
`sealed interface Configuration`), and the fabricated name is already recorded by
hand in `W7-18-structured-task-scope-jep505.md` and in
`jdk25_concurrency.rs:2227`. **This is the first time an automated instrument
said so**, and it said it on the first run, which is the argument for the
capability in one line: a hand list cannot report that one of its own names is
not a JDK name. `$Configuration` and `$Subtask` are baselined instead.

**Nothing else was refused.** All 29 other entries in `classes.txt` resolved,
including the two that motivated the class-file approach: preview-versioned
`StructuredTaskScope` types and the unexported `jdk.internal.access.SharedSecrets`.

**Deliberately not attempted:**

* **GraalVM SDK `org.graalvm.nativeimage.*`** (E25 rows 1, 15) — not in the JDK
  image. It would need the SDK jar on a `--classpath`; `JdkBaseline` reads
  `jrt:/` only. The generator would take a `--jar` mode; that is a real
  extension, not a defect, and nothing in this lane needed it.
* **The pinned `h2-*.jar`** (E25 row 27) — same reason.
* **The JDK 25 `@Deprecated(forRemoval)` set** (E25 rows 11, 12) — a whole-image
  annotation walk, not a per-class surface dump. Different tool.
* **JFR's `jdk.*` event set** (E25 row 46) — comes from `jfr metadata` /
  `metadata.xml`, not from a class surface.
* **`vmIntrinsics.hpp`'s `@IntrinsicCandidate` set** (E25 row 19) — the
  `java.math.BigInteger` *surface* is baselined, but "which five are intrinsics"
  is an annotation question the surface does not answer.

---

## 9. Residuals, stated so a green `--check` is not read as more than it is

* **Nothing consumes the baselines yet.** Thirty files and a generator do not
  make a single guard red. Until NOM E32-3 and E32-4 land, this is a capability,
  not a fix, and E25's sixty-one rows are all still exactly as red-proof as they
  were. Saying otherwise would be the species of claim this whole record line is
  about.
* **`--check` is not wired into CI.** It is a script anyone can run and nobody
  runs. Where it belongs is the same place the other `scripts/` gates are
  invoked from; this lane does not own that wiring. Without it, a JDK bump is
  visible only to whoever thinks to look.
* **The two KAT numbers are hand-measured**, and that is irreducible: a
  generator with no external fixed point is a generator that agrees with itself.
  The mitigation is that they came from a *different instrument* (`javap`) than
  the one under test, and that they are checked on every `--update`.
* **`classes.txt` is a hand list.** It is a list of *what to baseline*, not a
  claim about any class's contents, so it cannot be wrong in the E25 sense — but
  a class that should be baselined and is not is invisible, exactly like every
  allowlist in E25 §4.3. The mitigation is the annotation convention: every line
  names the guard it serves, so a line with no guard is visibly speculative.
* **`--check`'s CRLF tolerance is a workaround for NOM E32-1**, and it hides the
  problem it tolerates. If that nomination is not applied, the byte-level
  guarantee this format claims is not true on this repo's Windows checkouts —
  only the line-level one is.
