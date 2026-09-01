# The JCA providers are half-populated, and the missing half has names

**Status: MEASURED 2026-08-30, OPEN, unclaimed.** Found while verifying another
lane's `KeyStore.getInstance("JCEKS")` fix. That fix WORKS — this page is about
what the same probes found beside it.

Probes: `apps/probes/JavaSecurityReach.java` (15 rows) and
`apps/probes/SunJceServices.java` (17 rows), both diffed against HotSpot
25.0.4+7 in compatible mode and under `--jdk-only`.

## First, the thing that is NOT broken

`HANDOFF-20260828-SCOPE` §4 carried `KeyStore.getInstance("JCEKS")` as an open,
unclaimed gap. `d0beba0eb` closed it on 2026-08-29, and it is verified here:

```text
JavaSecurityReach   15 rows   compat 0 differing   --jdk-only 0 differing
```

That includes the whole chain the gap used to be blamed on — and one premise
worth retiring with it. `native-builtins/src/lib.rs`'s Cipher `<clinit>` shim
says the real chain reads `${java.home}/conf/security/java.security` and fails
with `IOException("Is a directory")`. **It does not, and has not for some
time:**

```text
File.exists true · File.isFile true · File.isDirectory false · length 74132
FileInputStream read=16 · Files.readAllBytes 74132 · Files.size 74132
Security.getProviders count 12
  SUN SunRsaSign SunEC SunJSSE SunJCE SunJGSS SunSASL XMLDSig SunPCSC JdkLDAP JdkSASL SunPKCS11
```

Every row identical to HotSpot. The provider LIST is complete and correct, and
the file is a readable 74 KB regular file. Anyone reasoning from that comment is
reasoning from a defect that no longer exists.

## What IS still missing: roughly half of each provider's services

```text
                        HotSpot   CratonVM
SunJCE  class           com.sun.crypto.provider.SunJCE   java.security.Provider
SunJCE  getServices()   194       103
SunJCE  service types    13         7
SunJCE  property count  496       252

SUN     class           sun.security.provider.Sun        java.security.Provider
SUN     getServices()    68        44
SUN     service types    13        11
SUN     property count  257       124
```

The providers are SYNTHESISED — a bare `java.security.Provider` populated by
`jca/provider_chain.rs`'s `put_service` table — rather than instances of the
JDK's own provider classes. That is a deliberate shape, not news. The news is
**which service types the table does not reach**:

```text
SunJCE missing:  AlgorithmParameterGenerator, KDF, KEM, KeyAgreement,
                 SecretKeyFactory, Signature
SUN    missing:  AlgorithmParameterGenerator, Configuration
```

Two of those are load-bearing rather than exotic:

* **`SecretKeyFactory`** is how every password-based key is derived —
  `PBKDF2WithHmacSHA256`, and the `PBEWith*` family that JCEKS and PKCS12 use to
  protect private keys. Its absence is why the JCEKS work had to serve "exactly
  what it serves for JKS, written under the right magic" rather than the real
  encrypted format.
* **`KeyAgreement`** is Diffie-Hellman. `KeyAgreement.getInstance("DH")` has no
  SunJCE service to resolve to here.

## Why this is worth a page rather than a shrug

`Security.getProviders()` answering 12 correct names makes the JCA look wired.
It is wired at the LIST level and half-wired at the SERVICE level, and the
difference is invisible until a program asks for an algorithm in one of the six
missing types — at which point it gets `NoSuchAlgorithmException` from a
provider that is present and named correctly. That is the same wrong-refuse
direction the JCEKS row was, and the same shape: code that works on every real
JDK and dies here.

**Not fixed here, and not sized.** Whether each missing type is a table entry or
a real implementation depends on whether the JDK's SPI class can run as
bytecode — which is exactly how JCEKS turned out to be tractable
(`com.sun.crypto.provider.JceKeyStore` loads fine on this VM; row 17 of
`SunJceServices` confirms it in both VMs). Somebody should ask that question per
type before assuming either answer.

## Reproduce

```bash
J=/data/jdkimages/jdk25-linux/jdk-25.0.4+7
javac -d /tmp/jca apps/probes/JavaSecurityReach.java apps/probes/SunJceServices.java
(cd /tmp/jca && $J/bin/java -cp . SunJceServices)              > hs.txt 2>/dev/null
(cd /tmp/jca && cratonvm --java-home $J -cp . SunJceServices)  > cv.txt 2>/dev/null
diff hs.txt cv.txt
```

Diff on **stdout only** — this VM's tracing goes to stderr and merging the two
turns an 8-row difference into a 30-row one.

## A note on how this was found, which cost an hour

The §4 row said "unclaimed", so I picked it up. It had been closed the previous
evening; my worktree was **91 commits behind** and the row I was reading was the
stale copy. `git show origin/dev:<page>` on the row BEFORE investigating would
have cost one command. `docs/known-issues/` already says a triage page is stale
the day after it is written; it is equally true of the page you are standing on.
