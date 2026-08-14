# `KeyPairGenerator`: three short `getEncoded()` public keys, and one algorithm name HotSpot refuses

**Status:** OPEN (2026-08-14). Four rows of `probes/KpgEndToEnd.java` that
differ from HotSpot JDK 25 and are **not** the missing-algorithm species the
JCA engine residuals page closed the same day. Split out from it rather than
left in the closing note: each was present on pristine `origin/dev` before that
work started (verified against a control binary built from it), and each has a
different cause.

## The rows

```text
                     HotSpot JDK 25                     CratonVM
RSA                  generateKeyPair=OK len=422         OK len=294
EC                   generateKeyPair=OK len=120         OK len=91
RSASSA-PSS           generateKeyPair=OK len=420         OK len=294
ECDSA                getInstance=NoSuchAlgorithmException  getInstance=OK
```

## 1–3. The short `getEncoded()`

`getPublic().getEncoded()` must be a complete DER `SubjectPublicKeyInfo`. 294
bytes for a 2048-bit RSA public key is about the size of the bare
`RSAPublicKey` `SEQUENCE { n, e }` — 270-odd bytes of modulus and exponent —
without the `AlgorithmIdentifier` wrapper and `BIT STRING` around it, and 91 vs
120 for P-256 has the same shape. So the likely reading is that these keys
encode the key MATERIAL and not the SPKI envelope, which would make every
consumer that re-parses the bytes (`X509EncodedKeySpec`, a CSR builder, a JWK
serialiser) reject them.

That is a reading, not a measurement: **nobody has diffed the two DERs.** The
first step is `openssl asn1parse` on both, not a code change — the three rows
may be one defect or three, and RSASSA-PSS sharing RSA's 294 suggests they
share a producer.

`ML-DSA`, `ML-KEM`, `Ed25519`, `Ed448`, `X25519`, `X448`, `XDH`, `DSA` and `DH`
all match HotSpot's lengths exactly, and all nine of those are driven through
the real JDK SPI. Only the three algorithms CratonVM encodes ITSELF are short,
which is the strongest available hint about where to look.

## 4. `ECDSA` is accepted where HotSpot refuses

`key_factory::algo_idx` maps `"EC" | "ECDSA" => ALGO_EC`, so
`KeyPairGenerator.getInstance("ECDSA")` hands back a working P-256 generator.
SunEC registers `KeyPairGenerator.EC` and no `ECDSA` alias for it, so HotSpot
answers `NoSuchAlgorithmException`.

This is an OVER-acceptance, and it is the same species as the defect
`keypairgenerator-getinstance-accepts-any-algorithm-FIXED-20260813.md` closed —
`getInstance` is the SELECTION step, and a caller's
`catch (GeneralSecurityException) { … BouncyCastle … }` fallback is dead code
for any name this VM accepts and HotSpot does not. That sweep fixed the
accept-EVERYTHING direction; this is the one surviving name in the
accept-slightly-too-much direction, found by `KpgEndToEnd` while closing the
residuals page.

**It is deliberately not fixed here.** Refusing `ECDSA` is a behaviour change
for existing CratonVM callers that no measurement in this workstream asked for,
and unlike the missing algorithms it makes something stop working rather than
start. Whoever takes it should check the in-tree fixtures for the literal name
first; the `KeyFactory` side is separate and unaffected (`kf_algo_idx` inherits
the same arm, and HotSpot's `KeyFactory.getInstance("ECDSA")` also refuses).

## Repro

```bash
javac -d /tmp/p probes/KpgEndToEnd.java
java -cp /tmp/p KpgEndToEnd > /tmp/hs.txt
cratonvm --java-home <jdk25> -cp /tmp/p KpgEndToEnd | diff /tmp/hs.txt -
```
