# WORKER-5 NOTE 8 — `getDefinedPackage` answers for packages the loader did not DEFINE, and it is why `RLangPackages` fails on a binary built from the integrated tree

**Status: OPEN, MEASURED.** Lane WORKER-5, 2026-08-22, on
`C:/craton/cratonvm-w5.exe` — a Windows `cargo build --release -p cratonvm-cli`
of the integrated tree at `4903bcf62`, built by this lane precisely because no
prebuilt binary matched the tree. Oracle: HotSpot 25.0.3+9. Not this lane's
surface to fix.

---

## 1. The one-line defect

```java
ClassLoader.getSystemClassLoader().getDefinedPackage("java.lang")
```

| | HotSpot | CratonVM |
|---|---|---|
| `app.getDefinedPackage("java.lang")` | `null` | **`package java.lang`** |
| `app.getDefinedPackage("java.util")` | `null` | **`package java.util`** |
| `app.getDefinedPackage("java.io")` | `null` | **`package java.io`** |
| `app.getDefinedPackage("no.such.package")` | `null` | `null` |
| `platform.getDefinedPackage("java.lang")` | `null` | **`package java.lang`** |
| `Package.getPackage("java.lang")` — SHOULD walk | `package java.lang` | `package java.lang` |

`java.lang` is defined by the **boot** loader. `ClassLoader.getDefinedPackage`'s
contract is a `Package` *"that has been **defined by this class loader**"*;
`getPackage`/`getPackages` are the ones that walk the delegation chain.

**CratonVM implements `getDefinedPackage` as "any package I can see".** The
`no.such.package` row is what makes that precise rather than a guess: it answers
`null` for a package that exists nowhere, so this is not "always non-null" — it
is resolving against the whole known package set instead of the loader's own
definitions. `getPackage` is correct, so the two methods are not distinguished.

## 2. How it surfaces

`regression-suite/src/RLangPackages.java` (WORKER 3, `caab31d49`) fails its
FIRST check:

```text
AssertionError: appLoader.getDefinedPackage(java.lang)==null:
  the application loader does not DEFINE java.lang, got package java.lang
        at RLangPackages.main(RLangPackages.java:131)
```

MEASURED on the binary above:

| arm | measured here | recorded elsewhere |
|---|---|---|
| `SUITE=all CRATONVM_ARGS=--jdk-only` | **106 / 107** — `RLangPackages` | 107/107 |
| `SUITE=all` | **105 / 107** — `RTreeRangeGc`, `RLangPackages` | 107/107 |
| `SUITE=core` | **65 / 67** — `RTreeRangeGc`, `RLangPackages` | 67/67 |

`RLangPackages` fails in **all three**. `RTreeRangeGc` is the known flake and
appears in two of the three.

It is **not** mode-dependent: it fails in strict and in compatible alike, which
distinguishes it from the five COMPATIBLE-mode defects that closed this week.
The same vector passes on HotSpot with `PASS RLangPackages (27 checks)`.

## 3. A CORRECTION to this lane's own earlier report

`WORKER-5-NOTE-7` §1a.3 filed a second finding:

> *"`RLangPackages` publishes no check count and is not in
> `harness-uncounted.txt`, so the harness flags `[G3]` on every run. That is a
> registration gap in a new vector, independent of whether the vector passes."*

**The last clause is wrong.** The vector DOES publish a count — HotSpot prints
`PASS RLangPackages (27 checks)` — it simply never reaches that line on
CratonVM, because it dies on check 1. `[G3]` is a **consequence** of the failure,
not an independent gap, and adding the vector to `harness-uncounted.txt` would
be the wrong fix: it would silence a flag that is correctly firing.

That error came from reading the harness output without running the vector
against the oracle. It is the same species as the ones this lane has been
cataloguing all week, committed by the lane that catalogues them.

## 4. The probe

`regression-suite/probes/DefinedPackageProbe.java` separates the three questions
the one assertion conflates: does `getDefinedPackage` answer for a package this
loader did not define; does it do so one loader up as well; and does `getPackage`
still walk correctly. It carries its own positive control and **labels that
control WEAK** — the probe's own class is in the unnamed package, which answers
`null` on a conforming VM too, so it does not prove `getDefinedPackage` is
capable of returning non-null for a genuinely defined package.

```bash
javac -d /tmp/dpp regression-suite/probes/DefinedPackageProbe.java
java -cp /tmp/dpp DefinedPackageProbe                    # the oracle
cratonvm --java-home "$JDK" --jdk-only -cp /tmp/dpp DefinedPackageProbe
```

## 5. What this does NOT establish

* **No cause is named.** This is a black-box oracle diff; nothing here reads the
  Rust side or points at a registrar. Locating the implementation is the owning
  lane's first step, not a conclusion of this record.
* **The strong control is missing.** Proving `getDefinedPackage` returns non-null
  for a package the app loader really DOES define needs a class in a *named*
  package on the classpath. The probe says so rather than implying its weak
  control covered it.
* **`getPackages()` was not measured**, only `getPackage`/`getDefinedPackage`.
  A loader-set defect would likely show there too.
* **Only two loaders were asked**, application and platform. The boot loader is
  not reachable as a `ClassLoader` object.
* **`RTreeRangeGc`'s second failure is the known flake**, characterised in
  `WORKER-5-NOTE-7` §1a.2 (12 ABBA-interleaved runs, both outcomes under both
  harnesses); it is not evidence about this defect.

## 6. NOMINATIONS

* **N1 — owner needed for `getDefinedPackage`.** It must consult only the
  packages THIS loader defined. The `no.such.package` row says the lookup is
  already package-set-aware, so the fix is likely a scope, not a new mechanism.
* **N2 — `RLangPackages` is a REAL failing vector on the integrated tree**, and
  the `107/107 · 107/107 · 67/67` recorded elsewhere is not reproducible on a
  binary built from that tree by this lane. Whoever measured green should say
  which binary and which commit; one of the two measurements is about a
  different artefact.
* **N3 — do NOT add `RLangPackages` to `harness-uncounted.txt`** (§3). The `[G3]`
  flag is correct; silencing it would hide the failure that causes it.

---

### INDEX ROWS (for H0 to move into `INDEX.md`)

* `WORKER-5-NOTE-8` — **`ClassLoader.getDefinedPackage` returns a Package for
  packages the loader did NOT define** (`java.lang`, `java.util`, `java.io` on
  both the application and platform loaders; HotSpot answers `null` for all).
  `no.such.package` correctly answers `null`, so it is a scope error, not a
  stub. This is why `RLangPackages` fails **106/107 strict and 105/107
  compatible** on a Windows binary built from the integrated tree — mode
  INdependent. Includes a correction to `WORKER-5-NOTE-7` §1a.3, which wrongly
  called the `[G3]` flag an independent registration gap. Probe:
  `regression-suite/probes/DefinedPackageProbe.java`. MEASURED, cause not named.
