# FIXED — the JDK provider alias tables: 401 rows measured, 264 seeded, and the null provider they exposed

## Status

**FIXED 2026-08-26** on `fix/jca-provider-alias-tables-20260826`.

The SUN `MessageDigest` corner of this was closed on 2026-08-24 (40 rows, its
own page). That fix priced the rest and left it open: 401 alias rows across the
five JDK providers, of which SUN's digests were one column. This is the rest.

| | before | after |
|---|---|---|
| alias rows served, `getInstance(name, provider)` | 67 of 401 | **331 of 401** |
| of those, refused with a canonical this VM *does* serve | **264** | **0** |
| no-provider names attributed as HotSpot attributes them | 216 of 583 | **483 of 583** |
| names answered by the *wrong* provider | 0 | **0** |
| `Signature.getProvider()` returning `null` where HotSpot names one | 71 | **0** |

## The measurement, which is most of the work

`Security.getProvider(p).keySet()` on HotSpot 25 for SUN, SunJCE, SunRsaSign,
SunEC and SunJSSE, filtered to `Alg.Alias.<Type>.<Alias>`, is **401 rows**.
Each was replayed through `<Type>.getInstance(name, provider)` on both VMs,
recording the outcome for the ALIAS and for its CANONICAL separately. That
second column is what makes the set safe to act on:

| | rows | meaning |
|---|---|---|
| canonical OK, alias OK | 67 | already served |
| canonical OK, alias refused | **264** | the gap — spelling only |
| canonical refused | 70 | the algorithm is not implemented |
| alias OK, canonical refused | 0 | (would be incoherent) |

Only the 264 are seeded. Seeding any of the 70 would trade a refusal at
`getInstance` for a failure one call later, which is a worse answer than the
refusal — those stay `NoSuchAlgorithmException`, and the 35 distinct algorithms
behind them (DSA with SHA-2/SHA-3, the PBE ciphers, `AES/KWP`, the TLS KDF key
generators, `SSLContext.TLS` by name) are unimplemented-algorithm gaps, not
alias gaps.

Reading each provider's own map would have been the obvious probe and does not
work here: `Provider.keySet()` answers EMPTY on CratonVM. That is a separate
gap, untouched by this change, and it is why every measurement above asks
`getInstance` instead.

## Seeding is necessary and was not sufficient

With the 264 rows seeded and nothing else, **212 resolved and 52 did not**. The
52 were not scattered; they were four engines:

```text
43  SunJCE Cipher
 4  SunEC  KeyAgreement
 3  SunJCE KEM
 2  SunJCE KeyAgreement
```

Those are exactly the engines that gate on a hand-written name table without
asking the registry first. `KeyFactory` and `MessageDigest` already called
`canonical_if_unrecognised`; `Signature`, `KeyPairGenerator`, `Mac`,
`KeyGenerator`, `SecretKeyFactory` and `AlgorithmParameters` reach the registry
by another route and needed nothing.

Cipher is the interesting one, and the reason is visible in the data: its
canonicals are mostly full transformations —
`Alg.Alias.Cipher.2.16.840.1.101.3.4.1.42 = AES_256/CBC/NoPadding`. Seeding the
row gets a caller past `check_provider_ownership`, and then
`classify_transformation` is handed `2.16.840.1.101.3.4.1.42`, which does not
parse as a transformation however many registry rows point at it. Three call
sites now resolve the spelling first; after that, **0 of 264 remain refused**.

Deciding which engines needed this by MEASURING rather than by reading nine
`getInstance` paths is the whole reason the change is small: the prediction
"Cipher especially" was right, would still have missed KeyAgreement and KEM,
and would have added dead code to six engines that did not need it.

## The defect the fix exposed

Making 264 names resolve made a second divergence visible on 71 of them:
`Signature.getProvider()` answered `null` where HotSpot names `SunRsaSign` or
`SunEC`. `sig.getProvider().getName()` is what bc-java writes, so each was a
`NullPointerException` one call later.

`signature_name_is_offered` is a disjunction — a name this engine indexes, OR a
name some provider advertises. `sig_get_provider_null` was keyed on the INDEX
alone, so every offered-but-not-indexed name (`MD2withRSA`, every `*withECDSA`
except `SHA256withECDSA`, every OID spelling) fell through to `null`. It now
asks the registry, which is the same table `getInstance` consulted to accept
the name in the first place. A name nothing advertises still answers `null`
rather than a fabricated owner.

## Attribution: the risk that did not materialise

An alias row can move a name from whoever serves it onto the provider that
declares the alias, because `getInstance(name)` with no provider is decided by
chain ORDER. Measured over all 583 names the five providers reach, before and
after:

| | SAME as HotSpot | CratonVM refuses | wrong provider |
|---|---|---|---|
| before | 216 | 352 | 0 |
| after | **483** | 100 | **0** |

Not one name is answered by a different provider than HotSpot answers with, and
no name that matched before differs now. The wrong-provider column read 15
before and 0 after — all fifteen were the null-provider defect above, not a
chain disagreement.

## One row is better than its own canonical

`Cipher.getInstance("TripleDES", "SunJCE")` now returns a working
`DESede/CBC/PKCS5Padding` where it used to throw, while
`Cipher.getInstance("DESede", "SunJCE")` still throws. That incoherence is
pre-existing and is not introduced here: the tree already mapped
`Alg.Alias.Cipher.TripleDES` to the expanded transformation, where HotSpot maps
it to bare `DESede`, and CratonVM does not serve the bare name. Correcting the
alias to HotSpot's spelling would regress `TripleDES` to an exception, so the
row stays until `Cipher.getInstance("DESede")` is serviceable. Recorded rather
than quietly matched.

## Regression

| | result |
|---|---|
| `cargo test -p cratonvm-native-builtins --lib` | **4168 passed, 0 failed** |
| `cargo test -p cratonvm-types` (all targets, every gate) | **583 + 20 passed, 0 failed** |
| `cargo test -p cratonvm-gc -p cratonvm-vm --lib` | 2629 passed, 0 failed |
| `cargo test -p cratonvm-jit --lib` | 2111 passed, 0 failed |
| bc-java `cms.test.AllTests` | **`OK (433 tests)`**, 634 s |
| bc-java `jce.provider.test.AllTests` | **`OK (1 test)`**, 1867 s |
| bc-java `pkix.test.AllTests` | **`OK (19 tests)`**, 385 s |

`every_measured_alias_resolves_to_a_serviceable_canonical` is the ratchet. It
asserts the row resolves to its recorded canonical, and — only where the
canonical is a registered SERVICE — that the alias reaches it through
`get_service_entry`, which is the lookup `check_provider_ownership` performs.
The conditional is not a weakening: `KeyAgreement` and `KEM` are served from
hand-written SPI tables and register no services at all, so an unconditional
service assertion would fail on rows that work.

## What is not claimed

**Not that the 70 blocked rows are fine.** They are refusals that match no
oracle: HotSpot serves all 401. What this change establishes is that the
remaining 70 are missing ALGORITHMS rather than missing spellings, which is a
different and larger piece of work, and the 35 canonicals are listed above so
it can be started from a census rather than a bug report.

**Not that `Provider.keySet()` works.** It still answers empty. Every number
here is from `getInstance`, deliberately.

**Not that alias resolution is complete for third-party providers.** The
measurement covers the five JDK providers. A provider installed by an
application declares its own aliases, and those already resolved through the
same registry — nothing here changes that path, and nothing here tests it.

## The transferable part

**The second column is the fix.** A list of 401 names HotSpot serves and this
VM does not is a to-do list that will lie to you: 70 of them cannot be honoured
and seeding them would move the failure rather than remove it. Asking about the
CANONICAL as well as the alias turned one list into two, and only one of them
was actionable.

**Then measure again before writing the code.** The obvious next step after
seeding was to add alias resolution to every engine. Building the data first
and re-running the probe named the four that needed it and cleared five that
did not, at the cost of one eight-minute build.
