# Fixed: AES Key Wrap `Cipher.unwrap()` on a fresh Cipher

Status: resolved

Observed: 2026-07-10/11
Resolved: 2026-07-11

## Symptom

`crypto/elytron :: ElytronCryptoJWETest::externalJweAes128KeyWrapTest` failed while decoding an externally issued JWE: a newly created `Cipher` initialised only with `Cipher.UNWRAP_MODE` threw `IllegalStateException: Cipher not initialized` at `Cipher.unwrap()`.

## Root cause

CratonVM's native `Cipher.init` correctly recorded `UNWRAP_MODE` in its GC-stable side table, but `Cipher.unwrap(byte[], String, int)` had no native dispatch. The call therefore fell through to the real JDK `Cipher` bytecode, whose private SPI and `initialized` fields are deliberately not populated by the synthetic native initialisation path.

## Fix

Native Cipher dispatch now implements RFC 3394 AES Key Wrap for both the Keycloak/SunJCE `AESWrap` alias and canonical `AES/KW/NoPadding`. It natively handles `Cipher.WRAP_MODE`, fresh `Cipher.UNWRAP_MODE`, `Cipher.SECRET_KEY` reconstruction as `SecretKeySpec`, and rejects malformed or tampered wrapped keys with `InvalidKeyException`.

## Verification

- Remote host `20.83.144.174`: `cargo test -p cratonvm-native-builtins aes_key --lib` passed 7/7, including an RFC 3394 known-answer vector and tamper rejection.
- Unique optimized binary `/data/data/cratonvm-cipher-unwrap-aeswrap-20260711` passed a standalone probe in both `--nojit` and JIT modes. The probe verifies RFC ciphertext output, `AESWrap` wrap, a fresh unwrap-only Cipher, canonical `AES/KW/NoPadding`, and tamper rejection.
