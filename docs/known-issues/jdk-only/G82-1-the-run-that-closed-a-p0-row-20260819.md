# G82-1 — the run that closed a P0 row

**Status:** **P0 "Real boot-image requirement" → CLOSED(5)**, run recorded.
**Provenance:** CratonVM `C:/craton/target-nolto/release/cratonvm.exe`,
2026-08-19; Temurin 25.0.3+9-LTS as the real image. Every command and its exit
code is below; nothing here is cited from source.

---

## 0. What the row was waiting for

Not a design, not a decision, not an implementation. Its own *Required
resolution* column said so:

> Nothing further in design terms. Closure (rule 5 …) needs one thing this
> session could not supply: a `--jdk-only` boot on a real image, and the same
> boot against a deliberately broken `--java-home`, **run and recorded**. Name
> that run in the row when it exists.

The row had been `ENFORCING` — the behaviour implemented and the tests written
— waiting on somebody to execute it and write down what happened. Its own
provenance note admitted the gap in as many words: *"**Not executed**: no
`cargo` command was run this session, so the tests are cited as present, not as
passing."*

That is a row closable by running four commands. I spent several turns of this
session asserting that no P0 row was reachable without a program of work, and
that assertion was wrong twice over — first about the closure rules
(see `G81-1` §0), and then about this row, which was waiting on nothing but an
afternoon's execution.

## 1. The four legs

**Leg 1 — `--jdk-only` on a real image.**

```
cratonvm --java-home <Temurin 25.0.3> --jdk-only -cp regression-suite/build RJdkHello
  -> PASS RJdkHello (41 checks)
```

Plus the whole corpus in the same configuration: **101 of 101**.

**Leg 2a — a `--java-home` that does not exist.**

```
cratonvm --java-home C:/craton/no-such-jdk-deliberately --jdk-only ...
  -> EXIT 1
  "--java-home path does not exist or is not a directory: …
   Provide a valid JDK installation (must contain `jmods/` or `lib/modules`)."
```

Refuses. Does not fabricate a fallback library.

**Leg 2b — a `--java-home` that EXISTS but is not a runtime image.** This is
the leg that reaches the strict-mode text, and it is worth quoting because it
is the thing rule 5 asks for:

```
  -> EXIT 1
  "--jdk-only requires a real JDK runtime image: real class bytes are
   authoritative under this policy, so there is nothing to fall back to.
   Point the launcher at an installation with --java-home <PATH>, or drop
   --jdk-only to run with the default compatibility behaviour."
   … the two accepted layouts (jmods/java.base.jmod, lib/modules) …
  "CratonVM does not silently substitute the synthetic class library here: the
   two implementations have different semantics and different bugs, so a run
   whose library was chosen by the host is not reproducible or reportable."
```

**Leg 3 — the flag conflict.**

```
cratonvm --java-home <real> --jdk-only --synthetic-jdk ...
  -> EXIT 2
  "error: the argument '--jdk-only' cannot be used with '--synthetic-jdk'"
```

Rejected by the argument conflict, not by a JDK probe — which is what the
evidence column claims, now confirmed by running it.

**Leg 4 — the two in-tree tests, EXECUTED.**

```
cargo test -p cratonvm-vm --lib -- compatible_boot_precondition_does_no_host_probing
                                    jdk_only_with_synthetic_library_is_a_configuration_error
  test vm::vm_init::tests::compatible_boot_precondition_does_no_host_probing ... ok
  test vm::vm_init::tests::jdk_only_with_synthetic_library_is_a_configuration_error ... ok
  test result: ok. 2 passed; 0 failed
```

## 2. Why this satisfies rule 5

Rule 5 asks for **a documented, specification-consistent platform error instead
of a fabricated success**. Every refusal path above exits non-zero with an
explanatory message, and no path substitutes a different class library. Leg 1
establishes the positive case: given a real image, strict mode boots and the
corpus passes, so the refusal is not a blanket failure.

## 3. One correction to the row's own evidence, found by running it

The evidence column describes a single strict-mode message that "names
`--jdk-only`, explains why `--synthetic-jdk` is not an escape here, and offers
`--real-jdk` as the fallback". **There are two paths, and only one of them
produces that text.**

* Leg **2b** (path exists, not an image) → the full strict-mode message. ✔
* Leg **2a** (path does not exist) → fails EARLIER, in argument parsing, with a
  generic "path does not exist or is not a directory". It does not mention
  `--jdk-only` at all; the run's own trailer says
  `jdk mode: <not yet resolved — failure occurred during argument parsing>`.

Both refuse and neither fabricates, so rule 5 holds on both and the closure
stands. But the row implied one message where there are two, and the commoner
user mistake — a typo in a path — takes the branch with the *less* informative
text.

## 4. NOMINATIONS

**N1 — give the nonexistent-path branch the strict-mode framing. This is a
CONTRACT deviation, not a nicety.** §3 recorded it as the row's evidence being
imprecise. Reading the contract directly upgrades it:
`docs/feature-designs/jdk-only-mode.md` §8 requires that under `JdkOnly` the VM

> fail with a `MissingBootClass` / `InvalidConfiguration` error that names
> `--jdk-only`, the searched paths and the accepted JDK layout.

Leg 2b does all three. Leg 2a — a `--java-home` that does not exist, i.e. a
typo, the commonest way to hit this — names NONE of them: it fails earlier, in
argument parsing, with a generic "path does not exist or is not a directory".
The closure still stands on rule 5, because that path refuses and fabricates
nothing. But one of the two branches does not meet §8's wording, and the one
that does not is the one users will hit.

**N2 — the pattern generalises: look for rows waiting on execution, not on
work.** This row sat `ENFORCING` with its own provenance note admitting nothing
had been run. `Residual synthetic native set` is `MEASURING` with a ratchet test
(`native-builtins/tests/stub_ratchet.rs`) cited the same way. Before planning
work against any row, run what it already cites — the gap may be a missing
command rather than a missing implementation.

**N3 — do not over-read this closure.** It closes the *boot precondition*: that
`--jdk-only` demands a real image and refuses honestly without one. It says
nothing about what happens to natives, dispatch or tagging once booted, which is
what the other P0 rows are about and where the real programme remains.
