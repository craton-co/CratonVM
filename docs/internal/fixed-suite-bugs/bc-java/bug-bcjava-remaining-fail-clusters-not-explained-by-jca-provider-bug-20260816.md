# bc-java: remaining FAIL clusters not explained by the JCA provider alias/attribution bug — CLOSED

## Status
**CLOSED 2026-08-17** on `fix/bcjava-residuals-20260816`. This was a triage
pointer: five clusters of FAIL signatures left over after the JCA
alias/attribution fixes, none of them root-caused at the time. All five are now
accounted for. The measured end state, and everything that is still open, is on
the sibling page
`bug-bcjava-residual-suite-failures-20260816.md` (retired alongside this one);
prefer its numbers.

## What each cluster was

**Cluster 1 — "Error during cipher finalisation" (`cert.crmf`, `mime`).**
FIXED. The doc's guess — that it shares a cause with the AEAD `doFinal` item —
was half right: it is a `javax.crypto.Cipher` gap, but not that one.
`Cipher.init(mode, key, AlgorithmParameters)` dropped the caller's parameters
outside the PBES2 arm and recorded an EMPTY IV, and `init(ENCRYPT_MODE, key)` on
an IV-taking mode did not generate one. Both classes pass.

**Cluster 2 — certificate/key "unsupported encoding" (`cert.path`, `mozilla`).**
Already closed by the JCA fix, as the superseding note on the original said.

**Cluster 3 — `cert.test`, two independent failures.** Both FIXED, and the
doc's merge suggestion was wrong on one of them.
* The `ML-DSA` half was NOT the same bug as `cert.plants`: it went away with the
  provider-chain fallback for `Signature`, which lets a key minted by one
  provider reach an SPI that will take it.
* The `AttrCertTest: principal[0] for entity names don't match` half was
  `X500Principal.toString()`. It answered the RFC 2253 string; the JDK's is the
  `", "`-separated form with RFC 1779's quoting and the FULL keyword map —
  `EMAILADDRESS=mlorch@vt.edu` where `getName(RFC1779)` writes
  `OID.1.2.840.113549.1.9.1=...`. The test compares the whole string.

**Cluster 4 — NIST cert-path policy divergence (`jce.provider.test.nist`).**
The doc's instinct to check "whether this is an isolated case or part of a wider
pattern" was right: it was 42 methods, not one. Two causes.
* 4 of them (PKITS 4.3.3/4.3.4/4.3.5/4.3.11) were `X500Principal.equals`, which
  compared the RFC 2253 string instead of the CANONICAL form, so two DNs
  differing only in attribute-name case or in runs of spaces were unequal and a
  CRL could not be matched to its issuer. FIXED.
* The remaining 35 were **a JIT defect**, not a PKIX one:
  `CRATONVM_JIT_DENY=org/bouncycastle/jce/provider/ProvRevocationChecker.check`
  took the class from 12 failures to 0 on a 208-vector run, and `--nojit` did
  the same. Root-caused and FIXED — the class now passes all 286 vectors. See
  `fixed-suite-bugs/jit/bug-jit-compiled-body-loses-a-callee-thrown-exception-20260817.md`.

**Not covered here — `pqc.jcajce.provider`.** The doc asked for its two shapes
to be separated. They were: the process-crashing `NotImplemented` was the JCA
fix's Bug C and is gone, and what is left is the throughput cliff on its own
page. There is no third problem.

**`org.bouncycastle.test.AllTests`** — confirmed a harness-configuration issue,
as this page said; it needs `-Dtest.java.version.prefix=25` and then passes.

## The lesson worth keeping
Two of the five clusters were not JCA bugs at all. Both presented as
crypto/PKIX failures with crypto/PKIX exception messages, and both were settled
in one run each by `--nojit` — which nothing in the original triage tried,
because the failures looked like algorithm problems. **Run the `--nojit` arm
before attributing a wrong ANSWER to the layer whose exception you are reading.**
