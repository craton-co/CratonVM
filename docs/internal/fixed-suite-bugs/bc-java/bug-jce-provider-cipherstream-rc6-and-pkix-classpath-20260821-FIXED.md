# ✅ FIXED — the two `FAIL` classes: one classpath entry, and a transformation read from a reference the allocator had already moved

## Status

**RESOLVED 2026-08-22** on `fix/bcjava-pqc-and-cipherstream-20260822`.

| # | class | was | now |
|---|---|---|---|
| 040 | `org.bouncycastle.pkix.test.AllTests` | 4 errors, 1 failure on **both** VMs | **`OK (19 tests)` on both** |
| 028 | `org.bouncycastle.jce.provider.test.AllTests` | `CipherStreamTest2: Unexpected exception` | **10/10 passes; 2/10 before** |

The original page called 040 a harness gap and 028 a VM defect, and it was right
about both. It was wrong about *why* 028 happened, and that mattered: the
diagnosis it offered — a drifted admission table — named a mechanism that had no
part in it, and the guard's own message said the same thing.

## 040 — the classpath omitted pkix's resource bundle

Five failures, all resource-bundle lookups
(`MissingEntryException: Can't find entry CertPathReviewer.noValidCrlFound.text`
and three more, plus a `QcType statement was not recognised` that reads the same
file). The keys are all present in
`pkix/build/resources/main/org/bouncycastle/pkix/CertPathReviewerMessages.properties`;
the directory was simply not on `/data/bcjca-classpath.txt`, where `prov` and
`core` both have theirs.

Re-measured on 2026-08-22, same host, same fixture, one entry added:

| classpath | HotSpot | CratonVM |
|---|---|---|
| shipped `/data/bcjca-classpath.txt` | 4 errors, 1 failure (10 s) | 4 errors, 1 failure (278 s) |
| **+ `pkix/build/resources/main`** | **OK (19 tests)** (18 s) | **OK (19 tests)** (129 s) |

**Applied.** The original page deliberately did not apply it because two long
sweeps were reading the file at the time; nothing was reading it on 2026-08-22
(checked before and after), so the entry is in, and the pre-fix file is kept
beside it as `/data/bcjca-classpath.txt.pre-pkixfix-20260822`.

### And a checker, because this is the second omission in the same file

The first was `unboundid-ldapsdk`, which made `jce.provider.test` die in
`<clinit>` **on HotSpot too** — so a real CratonVM defect read as "fails on
HotSpot as well". This one invented five divergences in the other direction.
Same file, same shape, opposite sign, and neither is visible in a results table:
a missing classpath entry surfaces as a plausible *test* failure, never as a
classpath error.

`test-infra/bcjava-classpath-audit.sh` reports entries that exist on disk and
are not on the path. It is deliberately a checker and not a generator —
regenerating the fixture would silently change which junit/hamcrest version
wins and whether each module's test tree is on the path, and every sweep that
already read the file would stop being comparable to the ones that come after.
Verified both ways: exit 1 naming `pkix/build/resources/main` against the
pre-fix file, exit 0 against the corrected one. (It also reports the superseded
`javax.mail` / `activation` jars, which are the EE8 spellings of `jakarta.mail`
/ `jakarta.activation-api` already on the path, without counting them as
omissions — putting both on one classpath gives you two `javax.activation`
namespaces and an order-dependent resolution.)

## 028 — the transformation was empty because the string had moved

### What the guard said, and why it was wrong

```text
IllegalStateException: Cipher dispatch reached the AES path for transformation ''
(family None); `classify_transformation` admitted a name this arm cannot compute.
```

The admission table was never consulted for `""` and never admitted it. The
original page spotted that (`algo` comes from `state.algorithm`, and it is
EMPTY, so nothing was admitted) and proposed a provider-SPI shadowing mechanism.
That is not it either.

`cipher_alloc` allocated the `javax/crypto/Cipher` synthetic **first** and read
the caller's transformation **second**:

```rust
let obj = try_alloc_concurrent_synthetic(ctx, "javax/crypto/Cipher", 6)?;
let algo_str = ctx.read_string(algo).unwrap_or_default();   // <- one line too late
```

That allocation can relocate or reclaim `algo`, which is the caller's `String`
argument and is not pinned. `read_string` then landed on the vacated address,
and `unwrap_or_default()` turned the miss into `""`. The `Cipher` was minted
with `algorithm: ""` in the side-table and `transformation = ""` in its own
field; `Cipher.init` wrote its state through `entry(tkey).or_default()`, which
invented a live `mode` on top of the empty algorithm and reported success; and
the failure surfaced a layer away, at `doFinal`, inside the AES arm.

### The tell was in the message all along: the name changes every run

`RC6/CTR/NoPadding` is not an RC6 fact. Running `CipherStreamTest2` standalone,
the pre-fix binary names a different transformation each time:

```text
Twofish/CBC/PKCS5Padding · Serpent/OFB/NoPadding · Twofish/ECB/PKCS5Padding
Serpent/CBC/PKCS5Padding · Twofish/ECB/PKCS5Padding
```

and one run failed instead with
`InvalidKeyException: no IV set when one expected` — the same window, a
different casualty. An algorithm-shaped symptom that renames itself run to run
is collector timing, not an algorithm.

### The A/B

`CipherStreamTest2` standalone, one worktree, two binaries built from it,
`--Xmx 1g`, same host:

| collector | `cratonvm-bcfin-base` (dev) | `cratonvm-bcfin-fix1` |
|---|---|---|
| `Generational` | **0 / 5 pass** | **5 / 5 pass** |
| default (ZGC) | **2 / 5 pass** | **5 / 5 pass** |

The Generational arm is the cheap lever: it fails in 10–36 s (the test aborts
at the first bad transformation), where a passing run takes 140–200 s. The
default collector reproduces it too, less often — which is why the 53-class
sweep saw it as a flaky `FAIL` and why one full `jce.provider.test.AllTests`
run on the unmodified binary came back `OK (1 test)` while the standalone class
was failing five times out of five under Generational.

The whole class was run on the fixed binary as well —
`jce.provider.test.AllTests`, `OK (1 test)` in 1788 s (HotSpot: 484 s on the
same contended host) — but that is a no-regression check, not the evidence:
the unmodified binary also passes it sometimes. The ten standalone runs are the
evidence.

Re-measured after merging 152 commits of `dev` on top: `CipherStreamTest2`
standalone under Generational passes 3/3, and `jce.provider.test.AllTests` is
`OK (1 test)` again. The same merge also carries `dev`'s own fixes in this
family (`String.format reclaimed its own arguments mid-format`, the
`DecimalFormatSymbols` pinning), which is the same defect shape found
independently in three places on the same day.

### The fix

`native-builtins/src/jca/cipher.rs`:

* **`cipher_alloc` takes `&str`.** Every caller already read the transformation
  from the argument on native entry, before allocating anything, so the hazard
  is removed rather than pinned around. `obj` is pinned across the
  `create_string` that follows it.
* **`try_delegate_cipher_to_named_provider` and `try_delegate_cipher_to_chain`
  take `&mut ObjectRef` and rewrite it.** Both run the provider's own bytecode
  (`build_jca_impl`, `engineSetMode`, `engineSetPadding`), so the `Cipher` can
  relocate under them. The `spi` root was already pinned there; the `Cipher`
  itself was not, and both the `spi` field write at the end and the caller's
  return value used its vacated address.
* **`record_provider_and_reread`** replaces the bare `record_requested_provider`
  at every site that hands the `Cipher` back afterwards — that helper builds a
  `Provider`, which allocates, and drops the forwarded reference on the floor.
* **`try_delegate_cipher_to_provider` is gone.** Its one caller now passes the
  provider NAME resolved at the top of `cipher_get_instance_with_provider`
  rather than re-reading `args[1]` after an allocation.
* **`Cipher.init` refuses an empty transformation** instead of inventing a state
  for it, so any residual instance of this family fails at `init`, naming
  itself, rather than reaching a substitute cipher at `doFinal`.
* **The AES-arm guard's stated diagnosis is corrected**, and now says where to
  look instead.

## The thing that "did not fit" fits

The original page recorded that the standalone run printed the stack trace and
then reported `CipherStreamTest2: Okay`, while the JUnit wrapper scored it a
failure at index 18, and said — correctly — not to assume they were the same
event until that was checked.

They are the same event. The standalone run reports
`CipherStreamTest2: Unexpected exception <transformation>` and returns a
non-`Okay` verdict; what makes the two look inconsistent is that the defect is
*flaky*, at a rate that depends on the collector. A standalone run that happens
not to trip it prints `Okay`. Measured above: 2 of 5 default-collector runs pass
on the unmodified binary.

## What is not claimed

**This does not claim the JCA natives are now free of unpinned roots.** It fixes
the ones on `Cipher.getInstance`'s paths, which is what this page's failure
needed. Neighbouring code has the same shape in places — `cipher_iv_parameters`
and `try_build_real_certificate_factory_for` both hold a reference across an
allocation — and they are left alone here because nothing on this page measures
them.

**The 53-class sweep was not re-run.** The evidence is the two classes this page
owns, measured directly and repeatedly, plus a full
`jce.provider.test.AllTests` on the fixed binary.

## The transferable part

**A symptom that renames itself between runs is not about the name.** Five
standalone runs named four different ciphers; the page had one run and read
`RC6` as a clue about RC6. One repeat would have redirected the whole
investigation.

**"The admission table and this dispatch have drifted apart" was a diagnosis
written into an error message, and it outlived its own correctness.** It named
a mechanism, so it read as evidence rather than as a guess, and the page that
found the guard reasoned from it. A guard should say what it observed —
`transformation is empty` — and, if it must speculate, say that it is
speculating.
