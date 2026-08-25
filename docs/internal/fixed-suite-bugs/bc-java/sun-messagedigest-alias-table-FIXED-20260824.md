# FIXED — SUN advertised fifteen digests and answered to three spellings

## Status

**FIXED 2026-08-24** on `fix/bcjava-sha1-alias-and-lea-20260824`.

Found by the 53-class bc-java sweep of 2026-08-24, which left exactly two
CratonVM failures. This is one of them; the other is the inline-splice cursor
rewind, on its own page.

| | before | after |
|---|---|---|
| `AliasProbe`, 40 spellings, digest bytes checked | `bad=37 of 40` | **`bad=0 of 40`** |
| the same probe on HotSpot 25 | `bad=0 of 40` | (unchanged) |
| `org.bouncycastle.cms.test.AllTests` | `Tests run: 433, Failures: 0, Errors: 1` | **`OK (433 tests)`**, 480 s |

## The defect

```text
MessageDigest.getInstance("SHA1", "SUN")
  HotSpot 25 -> a working SHA-1
  CratonVM   -> NoSuchAlgorithmException: no such algorithm: SHA1 for provider SUN
```

`SHA-1` — hyphenated — resolved all along, and that is why this survived so
long. The primary name is a seeded SUN service and only the ALIAS spellings
were missing, so nothing that spelled the algorithm the modern way ever saw it.

`check_provider_ownership` gates every NAMED-provider `getInstance` on
`get_service_entry`, which resolves the alias table BEFORE the service map. A
missing alias row is therefore not a cosmetic gap in a properties view: it
refuses the call outright, against a JDK provider, for a digest that provider
demonstrably implements.

## The measurement

`Security.getProvider("SUN").keySet()` on HotSpot 25, filtered to
`Alg.Alias.MessageDigest.*`, is **forty rows**:

* ten short names — `SHA`, `SHA1`, `SHA224`, `SHA256`, `SHA384`, `SHA512`,
  `SHA512/224`, `SHA512/256`, `SHAKE128`, `SHAKE256`;
* fifteen OIDs, each registered twice — bare and `OID.`-prefixed.

`seed_direct_native_engine_services` carried **three** (`SHA256`, `SHAKE128`,
`SHAKE256`). The other thirty-seven were refusals.

The count is not the interesting half — the probe is. `AliasProbe` asks for
each spelling against `"SUN"` and compares the digest bytes to the canonical
name's, so a row that resolves to the WRONG algorithm fails the probe as loudly
as one that does not resolve at all:

| VM | binary | result |
|---|---|---|
| HotSpot 25 | — | `bad=0 of 40` |
| CratonVM | `dev` `e645a7349` | **`bad=37 of 40`** |
| CratonVM | this branch | **`bad=0 of 40`** |

The three that passed before are exactly the three that were seeded.

Reading the provider's own table was tried first and does not work here:
`Provider.keySet()` answers EMPTY on CratonVM, so a properties dump measures
nothing. That is a separate gap and is not touched by this change — which is
also why the probe asks `getInstance` rather than the map.

## The fix

Seed the forty measured rows. Every canonical they name is already a SUN
service (`MD2`, `MD5`, `SHA-1`, the SHA-2 family, the SHA-3 family, the two
SHAKEs), so this widens the SPELLINGS the provider answers to and not the set
of digests it claims.

`sun_message_digest_aliases_all_resolve_to_a_seeded_service` is the ratchet, in
both directions, because the two failure modes are opposite:

* every alias must RESOLVE, and its canonical must be one
  `message_digest::algorithm_supported` accepts — otherwise the alias trades a
  refusal at `getInstance` for a failure one call later;
* no alias may be ADVERTISED. Registering these as services instead would
  satisfy the first half and make `Security.getAlgorithms("MessageDigest")`
  answer 55 where HotSpot answers 15. The test asserts SUN's advertised count
  is still exactly fifteen.

The forty rows are written out in the test independently of the seed, which is
the whole value of having them twice.

## What is not claimed

**Not that the other providers are done.** The same dump says SunJCE declares
196 aliases, SunRsaSign 45, SunEC 51 and SunJSSE 6 — 401 across the five, where
this page fixes SUN's `MessageDigest` corner. Those are the same species and
are NOT audited here: each needs its own canonical-is-serviceable check before
it can be seeded, and `Cipher` in particular resolves transformations through
`classify_transformation` rather than a bare service name, so a blanket copy of
HotSpot's table would be a guess. Measured and left open, not overlooked.

**Not that `Provider.keySet()` is fixed.** It still answers empty. Nothing here
touches the map views; the fix is in the service/alias registry the engines
actually consult.

**Not a bisect.** How long the seed has been three rows is unmeasured.

## The transferable part

**An alias table is a gate, not a view.** It is tempting to read
`Alg.Alias.*` as presentation — something `getProperty` echoes — and to seed
only what some caller was seen asking for. It is the first thing
`get_service_entry` consults, so an absent row is a `NoSuchAlgorithmException`
with the provider's own name in it.

**Enumerate the oracle instead of collecting complaints.** The failing test
needed one spelling. Asking HotSpot for its whole table turned one bug report
into thirty-seven, at no extra cost — and the same dump priced the four
provider tables still outstanding, which no amount of waiting for the next
failure would have done.
