# G81-1 — the first closed row, and it closed by being measured

**Status:** **P2 "Headful AWT" → CLOSED(5)**, test named. One defect fixed.
**Provenance:** both VMs, headless. Oracle HotSpot 25.0.3+9-LTS; CratonVM
`--jdk-only`. Probe `scratchpad` `Headful.java` (14 rows); test
`regression-suite/src/RJdkAwtHeadless.java`, `headfulRefused()` (54 checks
total). Arms `--jdk-only` 101/101, `SUITE=all` 96/101, `SUITE=core` 61/62.

---

## 0. Why this row and not a P0 one

Because I had been asserting, repeatedly, that closing any row needed a
program of work — and that assertion was never checked against the closure
rule. It is worth being precise about the mistake: I summarised the rule as
"reviewed bridge census plus retag plus dispatch centralisation", which is
**rule 1**. There are five, and rule 5 is:

> explicitly out of scope, failing with a specification-consistent error
> (`ClassNotFoundException` / `NoClassDefFoundError` /
> `UnsupportedOperationException` / a documented platform error) rather than a
> fabricated success.

The Headful AWT row's own *Required resolution* column already names it:
"Declare JDK-only scope as **headless** initially (closure rule 5)". So the
route was written down, by someone else, before this session started. What was
missing was the measurement and the test, not a decision.

## 1. What was measured

14 headful operations, both VMs, headless:

| operation | HotSpot | CratonVM |
| --- | --- | --- |
| `new Frame()`, `new Window()`, `new Dialog()`, `new Button()` | `HeadlessException` | same |
| `Toolkit.getScreenSize()`, `getScreenResolution()` | `HeadlessException` | same |
| `getScreenDevices()`, `getDefaultScreenDevice()` | `HeadlessException` | same |
| `getSystemClipboard()`, `MouseInfo.getPointerInfo()` | `HeadlessException` | same |
| `new Robot()` | `AWTException` | same |
| `GraphicsEnvironment.isHeadless()` | `true` | same |
| local GE class | `sun.java2d.HeadlessGraphicsEnvironment` | same |
| `getAvailableFontFamilyNames()` | the families | **`NullPointerException`** |

**13 of 14 were already conformant.** The row had been sitting at `OPEN` while
the behaviour it asks for was already implemented — which is the argument for
running the probe before estimating the work, and the fourth time this session
that a measurement disagreed with my estimate.

## 2. The fourteenth, and why an NPE fails rule 5 too

`getAvailableFontFamilyNames()` threw `NullPointerException` from inside the
JDK's own font machinery, which has no platform font service behind it here.

It is tempting to call that "failing, therefore out of scope, therefore rule
5". It is not. Rule 5 requires a **specification-consistent** error, and lists
what those look like. An NPE from the middle of a JDK internal is neither a
working implementation nor a documented refusal — it is the failure mode rule
5 exists to forbid, arriving in a different costume.

Fixed by returning the five LOGICAL font families — `Dialog`, `DialogInput`,
`Monospaced`, `SansSerif`, `Serif` — which the specification guarantees every
implementation provides and which HotSpot lists headless too. That is a
truthful answer, not a fabricated one; and it is deliberately NOT the full
list HotSpot returns, because physical fonts are a platform service this VM
does not have. The test asserts only the five, since the rest vary by machine
and pinning them would pin the host.

**A second lesson, cheaply bought.** The first registration was placed on
abstract `java.awt.GraphicsEnvironment` and never fired: the receiver is
`sun.java2d.HeadlessGraphicsEnvironment`, which declares its own method with
code, so virtual dispatch correctly preferred the real bytecode. The fix was
to register on the CONCRETE class — which is also the right thing under
`G79-1` §2, where registering on abstract superclasses is the defect being
counted. The wrong instinct was caught by the dispatcher doing the right
thing.

## 3. What the closure claims, and what it does not

**Claims:** headful AWT is explicitly out of scope under `--jdk-only`, fails
with the specification's own errors, and a named test in the scheduled corpus
keeps it that way.

**Does not claim:** that the P0 "wholesale Bridge over-tagging" work is done.
That is a different row and stays open; `G80-1` §4a measured why doing it in
this crate now is premature — the `Graphics2D` group cannot move to real
bytecode without a Java2D JNI layer.

**Does not claim** headless AWT is complete either — though it is now
separately measured clean, 25 of 25 rows, by the same vector.

## 4. NOMINATIONS

**N1 — re-read the closure rule against every remaining row.** This row was
`OPEN` while already satisfying rule 5, and nobody had checked. `ProcessHandle`,
`JDBC / java.sql`, `Crypto provider completeness` and `Verifier coverage` all
have "declare it out of scope" as a plausible route, and each would need only a
probe and a vector rather than an implementation. That is a cheap sweep with a
demonstrated yield of one row for a few hours.

**N2 — the P0 rows are not reachable this way, and it is worth saying why.**
They are `OPEN` because their required resolution is rule 1 or rule 3 — a
reviewed bridge, or execution from real bytecode. Neither can be reached by
declaring anything out of scope, because the classes involved are ones
`--jdk-only` is *for*. No amount of probing closes them.

**N3 — `getAvailableFontFamilyNames` returning only logical families is a
reduced answer and should be revisited if a font service ever lands.** It is
recorded here rather than left as a surprise for whoever first asks why
`Arial` is missing.
