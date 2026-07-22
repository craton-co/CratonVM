# BUG-Q — `KeyFactory.generatePrivate` couldn't import an RSA private key (PEMFile)

**Test:** `org.apache.tomcat.util.net.jsse.TestPEMFile`. HotSpot: PASS.
**Status: ✅ FULLY FIXED — `OK (27 tests)` == HotSpot (dev commit 86a931b8).**
Unencrypted RSA keys fixed earlier (27→21); the remaining 21 encrypted-key
failures are now fixed too — see "## Encrypted keys — FIXED" below.

## Symptom

`PEMFile` parsing an unencrypted RSA private key (PKCS#1 / PKCS#8) failed with
`InvalidKeyException: Unable to parse the key`. The PEM `<init>` calls
`new PEMFile(path, password, passwordFile, null)` (keyAlgorithm = null), so
`toPrivateKey` tries `KeyFactory.getInstance(alg).generatePrivate(keySpec)` for
RSA/DSA/EC/ML-DSA; each threw `InvalidKeySpecException` and the wrapper rethrew.

## Root cause

CratonVM's synthetic `KeyFactory.generatePrivate` **fail-closed for RSA**
(`kf_generate_private` in `native-builtins/src/jca/key_factory.rs` threw
`InvalidKeySpecException` — only EC/PQC had real import paths). So even though
`parsePKCS1` produced a valid `RSAPrivateCrtKeySpec` carrying the real
modulus/exponents/CRT factors, no importer turned it into a key.

## Fix (unencrypted)

Route RSA `generatePrivate` to the **real** SunRsaSign
`sun.security.rsa.RSAKeyFactory$Legacy` SPI (verified the JDK 25 provider maps
`KeyFactory.RSA` to that class with a no-arg constructor), mirroring the existing
EC→`ECKeyFactory` and PQC routing. Because the spec already carries the real key
material, this is a genuine key import — not a synthetic stub. Falls back to the
prior `InvalidKeySpecException` if the real SPI is unavailable
(synthetic-JDK mode), so the fail-closed contract is preserved there. Verified:
the unencrypted-key PEM cases pass (`TestPEMFile` 27→21 failures); helps any
caller importing RSA keys (SSL cert/key loading).

## Encrypted keys — FIXED (dev commit 86a931b8)

The remaining 21 encrypted-key failures needed a PBE crypto stack. Two real
implementations (no synthetic stubs):

1. **Cipher CBC/DESede/DES → real SunJCE SPI.** The synthetic `Cipher` did only
   AES-GCM/ECB and had no DES. `jca/cipher.rs::cipher_do_final_impl` (the active
   dispatch — it overrides the `phases_early.rs` one) now routes
   `AES/CBC`, `DESede/CBC`, `DES/CBC` to the genuine
   `com.sun.crypto.provider.{AESCipher$General,DESedeCipher,DESCipher}` SPI,
   driving `engineSetMode/Padding/Init/DoFinal` — byte-identical to HotSpot
   (proven). Done before the AES-only key-length check so 8-byte DES keys aren't
   rejected. (`PEMFile` PKCS#1 keys derive via `MessageDigest.MD5`, which already
   worked; only the Cipher was missing.)
2. **PBKDF2 `SecretKeyFactory` → native.** Routing to the real
   `com.sun.crypto.provider.PBKDF2Core$HmacSHA*` SPI tripped a
   `ByteBuffer.get(byte[])`/`ScopedMemoryAccess.copyMemory` bug in
   `PBEUtil.encodePassword`, so PBKDF2 is computed natively via HMAC-SHA1/224/256
   over the `sha1`/`sha2` crates (RFC-2898). The PRF code is held in an
   identity-hash-keyed side-table because `SecretKeyFactory`'s real slot 0 is an
   `Object` (`spi`) and an `Int` written there collapsed every PRF to the SHA-256
   default (initially made `PBKDF2WithHmacSHA1` produce the SHA-256 key).

**Verified:** `TestPEMFile` → `OK (27 tests)` == HotSpot; PBKDF2WithHmacSHA1
=`ee81c73c…`, SHA256=`95606b5e…` (both == HotSpot); AES-GCM round-trip still PASS.

**Separate VM bug surfaced — ✅ FIXED (dev commit 453cdb61):**
`ByteBuffer.get(byte[])` → `ScopedMemoryAccess.copyMemory` threw AIOOBE.
Root cause: `CharsetEncoder.encode`/`Charset.decode` (`charset.rs`
`alloc_byte_buffer`/`alloc_char_buffer`) returned the ABSTRACT
`java/nio/{Byte,Char}Buffer` and never set `Buffer.address`, so the inherited
bulk-get computed `srcOffset = address(0)+pos(0) = 0 < arrayBaseOffset(16)` →
the `Unsafe.copyMemory` decode failed. Fixed by allocating the CONCRETE
`HeapByteBuffer`/`HeapCharBuffer` (also resolves `isDirect()`/`isReadOnly()`
AbstractMethodErrors) and setting `address=16`. (Driving the real PBKDF2 SPI
*still* can't complete — it then needs `Mac.getInstance` via the broken provider
list — so the native PBKDF2 above stays; but `ByteBuffer.get(byte[])` is now
correct generally.)
