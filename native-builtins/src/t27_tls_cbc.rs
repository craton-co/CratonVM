// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! T-CBC.1: TLS 1.2 CBC-mode cipher suites for CratonVM's rustls-backed TLS
//! engine (`TLS_ECDHE_{RSA,ECDSA}_WITH_AES_{128,256}_CBC_SHA{256,384}`).
//!
//! rustls's `ring`/`aws-lc-rs` crypto providers never implement CBC-mode
//! suites -- see `rustls-cbc-cipher-suites-not-supported.md`
//! for why (rustls's maintainers consider hand-written CBC-then-MAC record
//! processing too easy to get subtly wrong in a way that reintroduces a
//! Lucky13-style timing side channel, so they simply don't ship it at all).
//!
//! This module supplies the missing piece using the same audited RustCrypto
//! primitives (`aes`, `cbc`, `hmac`, `sha2`, `subtle`) this codebase already
//! depends on elsewhere (see `hsm-core`'s `crypto::rustcrypto_backend`)
//! instead of hand-rolling AES or HMAC. The one genuinely novel part is the
//! TLS1.2 CBC *record layer* itself -- explicit-IV framing, MAC-then-pad-
//! then-encrypt on the write side, and constant-time MAC-then-unpad-then-
//! verify on the read side -- which is ported faithfully from Go's
//! `crypto/tls` standard library (`extractPadding`/`tls10MAC` in
//! src/crypto/tls/{conn,cipher_suites}.go), a implementation that has run in
//! production for over a decade without a known Lucky13-class break, rather
//! than improvised from scratch.
//!
//! Making the standard RFC5246 A.6 key-block layout
//! (`client_MAC, server_MAC, client_key, server_key, client_IV, server_IV`)
//! available to these suites required a small, bounded patch to the vendored
//! rustls fork at `native-builtins/vendor/rustls-cbc` (see that crate's
//! `crypto::cipher::KeyBlockShape::mac_key_len` and
//! `tls12::ConnectionSecrets::make_cipher_pair`) -- rustls's
//! `Tls12AeadAlgorithm` plugin trait alone has no way to carry a separate
//! MAC secret, only ever pairing per-side `enc_key`/`fixed_iv` fields, so a
//! real (interoperable) fix needed that fork rather than being pluggable
//! from an external crate alone.

use std::vec::Vec;

use aes::{Aes128, Aes256};
use cbc::cipher::block_padding::NoPadding;
use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use hmac::{Hmac as HmacImpl, Mac};
use sha2::{Digest, Sha256, Sha384};
use subtle::ConstantTimeEq;

use rustls::crypto::cipher::{
    make_tls12_aad, AeadKey, InboundOpaqueMessage, InboundPlainMessage, KeyBlockShape,
    MessageDecrypter, MessageEncrypter, OutboundOpaqueMessage, OutboundPlainMessage,
    PrefixedPayload, Tls12AeadAlgorithm, UnsupportedOperationError,
};
use rustls::crypto::hash::{
    Context as HashContext, Hash as HashTrait, HashAlgorithm, Output as HashOutput,
};
use rustls::crypto::hmac::{Hmac as HmacTrait, Key as HmacKeyTrait, Tag as HmacTag};
use rustls::crypto::tls12::PrfUsingHmac;
use rustls::crypto::KeyExchangeAlgorithm;
use rustls::Tls12CipherSuite;
use rustls::{CipherSuite, CipherSuiteCommon, ConnectionTrafficSecrets, Error, SignatureScheme};

const BLOCK_SIZE: usize = 16;
const EXPLICIT_IV_LEN: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq)]
enum CbcVariant {
    Aes128Sha256,
    Aes256Sha384,
}

impl CbcVariant {
    const fn mac_key_len(self) -> usize {
        match self {
            Self::Aes128Sha256 => 32,
            Self::Aes256Sha384 => 48,
        }
    }

    const fn enc_key_len(self) -> usize {
        match self {
            Self::Aes128Sha256 => 16,
            Self::Aes256Sha384 => 32,
        }
    }
}

// ---------------------------------------------------------------------
// Hash / HMAC providers for the generic TLS1.2 PRF (key derivation,
// Finished verify_data) -- NOT used for the per-record CBC MAC below,
// which is computed directly against the `hmac`/`sha2` crates for full
// control over the Lucky13 timing-equalization trick.
// ---------------------------------------------------------------------

macro_rules! hash_provider {
    ($provider:ident, $ctx:ident, $digest:ty, $alg:expr, $len:expr) => {
        struct $provider;

        impl HashTrait for $provider {
            fn start(&self) -> Box<dyn HashContext> {
                Box::new($ctx(<$digest>::new()))
            }
            fn hash(&self, data: &[u8]) -> HashOutput {
                let mut h = <$digest>::new();
                h.update(data);
                HashOutput::new(&h.finalize())
            }
            fn output_len(&self) -> usize {
                $len
            }
            fn algorithm(&self) -> HashAlgorithm {
                $alg
            }
        }

        struct $ctx($digest);

        impl HashContext for $ctx {
            fn fork_finish(&self) -> HashOutput {
                HashOutput::new(&self.0.clone().finalize())
            }
            fn fork(&self) -> Box<dyn HashContext> {
                Box::new(Self(self.0.clone()))
            }
            fn finish(self: Box<Self>) -> HashOutput {
                HashOutput::new(&self.0.finalize())
            }
            fn update(&mut self, data: &[u8]) {
                self.0.update(data);
            }
        }
    };
}

hash_provider!(
    Sha256HashProvider,
    Sha256Context,
    Sha256,
    HashAlgorithm::SHA256,
    32
);
hash_provider!(
    Sha384HashProvider,
    Sha384Context,
    Sha384,
    HashAlgorithm::SHA384,
    48
);

static SHA256_HASH: Sha256HashProvider = Sha256HashProvider;
static SHA384_HASH: Sha384HashProvider = Sha384HashProvider;

macro_rules! hmac_provider {
    ($provider:ident, $key:ident, $digest:ty, $len:expr) => {
        struct $provider;

        impl HmacTrait for $provider {
            fn with_key(&self, key: &[u8]) -> Box<dyn HmacKeyTrait> {
                Box::new($key(
                    HmacImpl::<$digest>::new_from_slice(key).expect("HMAC accepts any key length"),
                ))
            }
            fn hash_output_len(&self) -> usize {
                $len
            }
        }

        struct $key(HmacImpl<$digest>);

        impl HmacKeyTrait for $key {
            fn sign_concat(&self, first: &[u8], middle: &[&[u8]], last: &[u8]) -> HmacTag {
                let mut mac = self.0.clone();
                mac.update(first);
                for m in middle {
                    mac.update(m);
                }
                mac.update(last);
                HmacTag::new(&mac.finalize().into_bytes())
            }
            fn tag_len(&self) -> usize {
                $len
            }
        }
    };
}

hmac_provider!(HmacSha256Provider, HmacSha256Key, Sha256, 32);
hmac_provider!(HmacSha384Provider, HmacSha384Key, Sha384, 48);

static HMAC_SHA256: HmacSha256Provider = HmacSha256Provider;
static HMAC_SHA384: HmacSha384Provider = HmacSha384Provider;

// ---------------------------------------------------------------------
// The actual CBC record layer.
// ---------------------------------------------------------------------

/// Constant-time PKCS7-style TLS padding validation, ported from Go's
/// `crypto/tls.extractPadding` (BSD-licensed, `src/crypto/tls/conn.go`).
/// Returns `(bytes_to_remove, good)` where `good` is `0xff` if the padding
/// is well-formed and `0x00` otherwise; the number of bytes examined is
/// fixed (min(256, payload.len())), independent of the claimed padding
/// length, and no data-dependent branch is taken -- this is what makes it
/// safe against a Lucky13-style padding-oracle timing attack.
fn extract_padding(payload: &[u8]) -> (usize, u8) {
    if payload.is_empty() {
        return (0, 0);
    }
    let padding_len = payload[payload.len() - 1];
    let t = (payload.len() as i32 - 1) - i32::from(padding_len);
    let mut good = (!t >> 31) as u8;

    let to_check = core::cmp::min(256, payload.len());
    for i in 0..to_check {
        let t = i32::from(padding_len) - i as i32;
        let mask = (!t >> 31) as u8;
        let b = payload[payload.len() - 1 - i];
        good &= !(mask & padding_len ^ mask & b);
    }

    // Collapse "some bits cleared" into a strict all-or-nothing 0x00/0xff.
    good &= good << 4;
    good &= good << 2;
    good &= good << 1;
    good = ((good as i8) >> 7) as u8;

    let padding_len_masked = padding_len & good;
    (padding_len_masked as usize + 1, good)
}

const fn round_up(n: usize, to: usize) -> usize {
    ((n + to - 1) / to) * to
}

/// Computes the CBC record MAC (`seq || header || data`), then -- Lucky13
/// timing-equalization, ported from Go's `tls10MAC` -- continues feeding the
/// hash function with `extra` bytes on a *cloned* copy after the real tag
/// has already been extracted from the original. Since `data.len() +
/// extra.len()` is constant for a given ciphertext length (whatever the
/// claimed padding length turned out to be), the total hashing work done is
/// independent of the (attacker-influenced, pre-validation) padding length.
fn compute_mac(
    variant: CbcVariant,
    key: &[u8],
    header: &[u8],
    data: &[u8],
    extra: &[u8],
) -> Vec<u8> {
    match variant {
        CbcVariant::Aes128Sha256 => {
            let mut mac =
                HmacImpl::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
            mac.update(header);
            mac.update(data);
            let mac_for_extra = mac.clone();
            let tag = mac.finalize().into_bytes().to_vec();
            let _ = mac_for_extra.chain_update(extra).finalize();
            tag
        }
        CbcVariant::Aes256Sha384 => {
            let mut mac =
                HmacImpl::<Sha384>::new_from_slice(key).expect("HMAC accepts any key length");
            mac.update(header);
            mac.update(data);
            let mac_for_extra = mac.clone();
            let tag = mac.finalize().into_bytes().to_vec();
            let _ = mac_for_extra.chain_update(extra).finalize();
            tag
        }
    }
}

fn cbc_encrypt(
    variant: CbcVariant,
    key: &[u8],
    iv: &[u8; EXPLICIT_IV_LEN],
    content: &[u8],
) -> Result<Vec<u8>, Error> {
    match variant {
        CbcVariant::Aes128Sha256 => {
            let enc = cbc::Encryptor::<Aes128>::new_from_slices(key, iv)
                .map_err(|_| Error::EncryptError)?;
            Ok(enc.encrypt_padded_vec_mut::<NoPadding>(content))
        }
        CbcVariant::Aes256Sha384 => {
            let enc = cbc::Encryptor::<Aes256>::new_from_slices(key, iv)
                .map_err(|_| Error::EncryptError)?;
            Ok(enc.encrypt_padded_vec_mut::<NoPadding>(content))
        }
    }
}

fn cbc_decrypt(
    variant: CbcVariant,
    key: &[u8],
    iv: &[u8; EXPLICIT_IV_LEN],
    ciphertext: &[u8],
) -> Result<Vec<u8>, Error> {
    match variant {
        CbcVariant::Aes128Sha256 => {
            let dec = cbc::Decryptor::<Aes128>::new_from_slices(key, iv)
                .map_err(|_| Error::DecryptError)?;
            dec.decrypt_padded_vec_mut::<NoPadding>(ciphertext)
                .map_err(|_| Error::DecryptError)
        }
        CbcVariant::Aes256Sha384 => {
            let dec = cbc::Decryptor::<Aes256>::new_from_slices(key, iv)
                .map_err(|_| Error::DecryptError)?;
            dec.decrypt_padded_vec_mut::<NoPadding>(ciphertext)
                .map_err(|_| Error::DecryptError)
        }
    }
}

struct CbcCipher {
    variant: CbcVariant,
    mac_key: Vec<u8>,
    enc_key: Vec<u8>,
}

impl MessageEncrypter for CbcCipher {
    fn encrypt(
        &mut self,
        msg: OutboundPlainMessage<'_>,
        seq: u64,
    ) -> Result<OutboundOpaqueMessage, Error> {
        let payload = msg.payload.to_vec();
        let mac_size = self.variant.mac_key_len();
        let header = make_tls12_aad(seq, msg.typ, msg.version, payload.len());

        let tag = compute_mac(self.variant, &self.mac_key, &header, &payload, &[]);
        debug_assert_eq!(tag.len(), mac_size);

        let mut content = Vec::with_capacity(payload.len() + tag.len() + BLOCK_SIZE);
        content.extend_from_slice(&payload);
        content.extend_from_slice(&tag);

        let pad_len = (BLOCK_SIZE - 1) - (content.len() % BLOCK_SIZE);
        content.resize(content.len() + pad_len + 1, pad_len as u8);
        debug_assert_eq!(content.len() % BLOCK_SIZE, 0);

        let mut iv = [0u8; EXPLICIT_IV_LEN];
        getrandom::getrandom(&mut iv).map_err(|_| Error::EncryptError)?;

        let ciphertext = cbc_encrypt(self.variant, &self.enc_key, &iv, &content)?;

        let mut out = PrefixedPayload::with_capacity(EXPLICIT_IV_LEN + ciphertext.len());
        out.extend_from_slice(&iv);
        out.extend_from_slice(&ciphertext);

        Ok(OutboundOpaqueMessage::new(msg.typ, msg.version, out))
    }

    fn encrypted_payload_len(&self, payload_len: usize) -> usize {
        let mac_size = self.variant.mac_key_len();
        let content_len = payload_len + mac_size;
        let pad_len = (BLOCK_SIZE - 1) - (content_len % BLOCK_SIZE);
        EXPLICIT_IV_LEN + content_len + pad_len + 1
    }
}

impl MessageDecrypter for CbcCipher {
    fn decrypt<'a>(
        &mut self,
        mut msg: InboundOpaqueMessage<'a>,
        seq: u64,
    ) -> Result<InboundPlainMessage<'a>, Error> {
        let mac_size = self.variant.mac_key_len();
        let total_len = msg.payload.len();

        let min_ciphertext = round_up(mac_size + 1, BLOCK_SIZE);
        if total_len < EXPLICIT_IV_LEN + min_ciphertext
            || (total_len - EXPLICIT_IV_LEN) % BLOCK_SIZE != 0
        {
            return Err(Error::DecryptError);
        }

        let mut iv = [0u8; EXPLICIT_IV_LEN];
        iv.copy_from_slice(&msg.payload[..EXPLICIT_IV_LEN]);

        let plain = cbc_decrypt(
            self.variant,
            &self.enc_key,
            &iv,
            &msg.payload[EXPLICIT_IV_LEN..],
        )?;

        let (to_remove, padding_good) = extract_padding(&plain);

        // n = plain.len() - mac_size - to_remove, clamped to >= 0 without
        // branching on the (attacker-influenced) result -- ported from Go's
        // `subtle.ConstantTimeSelect(int(uint32(n)>>31), 0, n)`.
        let n_signed = plain.len() as i32 - mac_size as i32 - to_remove as i32;
        let neg_mask = n_signed >> 31;
        let n = (n_signed & !neg_mask) as usize;

        // Given the length gate above, plain.len() > mac_size always holds,
        // so `n + mac_size <= plain.len()` regardless of whether `n` was
        // clamped -- no bounds branch needed here either.
        let header = make_tls12_aad(seq, msg.typ, msg.version, n);
        let local_mac = compute_mac(
            self.variant,
            &self.mac_key,
            &header,
            &plain[..n],
            &plain[n + mac_size..],
        );
        let remote_mac = &plain[n..n + mac_size];

        let mac_choice = local_mac.as_slice().ct_eq(remote_mac);
        let good = mac_choice & subtle::Choice::from(padding_good & 1);
        if !bool::from(good) {
            return Err(Error::DecryptError);
        }

        // `cbc_decrypt` returns a freshly-allocated buffer (via
        // `decrypt_padded_vec_mut`), not an in-place decryption of
        // `msg.payload` -- write it back so the range below actually slices
        // decrypted plaintext instead of the original ciphertext bytes.
        msg.payload[EXPLICIT_IV_LEN..].copy_from_slice(&plain);

        Ok(msg.into_plain_message_range(EXPLICIT_IV_LEN..EXPLICIT_IV_LEN + n))
    }
}

struct CbcAlgorithm(CbcVariant);

impl Tls12AeadAlgorithm for CbcAlgorithm {
    fn encrypter(&self, key: AeadKey, _iv: &[u8], _extra: &[u8]) -> Box<dyn MessageEncrypter> {
        let mac_len = self.0.mac_key_len();
        let key_bytes = key.as_ref();
        Box::new(CbcCipher {
            variant: self.0,
            mac_key: key_bytes[..mac_len].to_vec(),
            enc_key: key_bytes[mac_len..].to_vec(),
        })
    }

    fn decrypter(&self, key: AeadKey, _iv: &[u8]) -> Box<dyn MessageDecrypter> {
        let mac_len = self.0.mac_key_len();
        let key_bytes = key.as_ref();
        Box::new(CbcCipher {
            variant: self.0,
            mac_key: key_bytes[..mac_len].to_vec(),
            enc_key: key_bytes[mac_len..].to_vec(),
        })
    }

    fn key_block_shape(&self) -> KeyBlockShape {
        KeyBlockShape {
            mac_key_len: self.0.mac_key_len(),
            enc_key_len: self.0.enc_key_len(),
            fixed_iv_len: 0,
            explicit_nonce_len: 0,
        }
    }

    fn extract_keys(
        &self,
        _key: AeadKey,
        _write_iv: &[u8],
        _explicit: &[u8],
    ) -> Result<ConnectionTrafficSecrets, UnsupportedOperationError> {
        // Key export (SSLKEYLOGFILE-style, KTLS offload) isn't implemented
        // for CBC suites -- nothing in this codebase's usage needs it, and
        // `ConnectionTrafficSecrets` has no CBC-shaped variant to populate.
        Err(UnsupportedOperationError)
    }

    fn fips(&self) -> bool {
        false
    }
}

static AES128_CBC_SHA256: CbcAlgorithm = CbcAlgorithm(CbcVariant::Aes128Sha256);
static AES256_CBC_SHA384: CbcAlgorithm = CbcAlgorithm(CbcVariant::Aes256Sha384);

static TLS12_ECDSA_SCHEMES: &[SignatureScheme] = &[
    SignatureScheme::ED25519,
    SignatureScheme::ECDSA_NISTP521_SHA512,
    SignatureScheme::ECDSA_NISTP384_SHA384,
    SignatureScheme::ECDSA_NISTP256_SHA256,
];

static TLS12_RSA_SCHEMES: &[SignatureScheme] = &[
    SignatureScheme::RSA_PSS_SHA512,
    SignatureScheme::RSA_PSS_SHA384,
    SignatureScheme::RSA_PSS_SHA256,
    SignatureScheme::RSA_PKCS1_SHA512,
    SignatureScheme::RSA_PKCS1_SHA384,
    SignatureScheme::RSA_PKCS1_SHA256,
];

/// `TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA256` -- the suite
/// `rustls-cbc-cipher-suites-not-supported.md`
/// was filed against (`SslConnectorCustomizerTests`).
pub static TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA256: Tls12CipherSuite = Tls12CipherSuite {
    common: CipherSuiteCommon {
        suite: CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA256,
        hash_provider: &SHA256_HASH,
        confidentiality_limit: u64::MAX,
    },
    kx: KeyExchangeAlgorithm::ECDHE,
    sign: TLS12_RSA_SCHEMES,
    aead_alg: &AES128_CBC_SHA256,
    prf_provider: &PrfUsingHmac(&HMAC_SHA256),
};

/// `TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA256`.
pub static TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA256: Tls12CipherSuite = Tls12CipherSuite {
    common: CipherSuiteCommon {
        suite: CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA256,
        hash_provider: &SHA256_HASH,
        confidentiality_limit: u64::MAX,
    },
    kx: KeyExchangeAlgorithm::ECDHE,
    sign: TLS12_ECDSA_SCHEMES,
    aead_alg: &AES128_CBC_SHA256,
    prf_provider: &PrfUsingHmac(&HMAC_SHA256),
};

/// `TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA384`.
pub static TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA384: Tls12CipherSuite = Tls12CipherSuite {
    common: CipherSuiteCommon {
        suite: CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA384,
        hash_provider: &SHA384_HASH,
        confidentiality_limit: u64::MAX,
    },
    kx: KeyExchangeAlgorithm::ECDHE,
    sign: TLS12_RSA_SCHEMES,
    aead_alg: &AES256_CBC_SHA384,
    prf_provider: &PrfUsingHmac(&HMAC_SHA384),
};

/// `TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA384`.
pub static TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA384: Tls12CipherSuite = Tls12CipherSuite {
    common: CipherSuiteCommon {
        suite: CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA384,
        hash_provider: &SHA384_HASH,
        confidentiality_limit: u64::MAX,
    },
    kx: KeyExchangeAlgorithm::ECDHE,
    sign: TLS12_ECDSA_SCHEMES,
    aead_alg: &AES256_CBC_SHA384,
    prf_provider: &PrfUsingHmac(&HMAC_SHA384),
};

/// `TLS_DHE_RSA_WITH_AES_128_GCM_SHA256` (0x009e) — finite-field DHE-RSA with
/// `ring`'s AES-128-GCM AEAD and SHA-256 PRF.
pub static TLS_DHE_RSA_WITH_AES_128_GCM_SHA256: Tls12CipherSuite = Tls12CipherSuite {
    common: CipherSuiteCommon {
        suite: CipherSuite::TLS_DHE_RSA_WITH_AES_128_GCM_SHA256,
        hash_provider: &SHA256_HASH,
        confidentiality_limit: 1 << 23,
    },
    kx: KeyExchangeAlgorithm::DHE,
    sign: TLS12_RSA_SCHEMES,
    aead_alg: match rustls::crypto::ring::cipher_suite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256 {
        rustls::SupportedCipherSuite::Tls12(provider) => provider.aead_alg,
        _ => unreachable!(),
    },
    prf_provider: &PrfUsingHmac(&HMAC_SHA256),
};

/// `TLS_DHE_RSA_WITH_AES_256_GCM_SHA384` (0x009f) — finite-field DHE-RSA with
/// `ring`'s AES-256-GCM AEAD and SHA-384 PRF.
pub static TLS_DHE_RSA_WITH_AES_256_GCM_SHA384: Tls12CipherSuite = Tls12CipherSuite {
    common: CipherSuiteCommon {
        suite: CipherSuite::TLS_DHE_RSA_WITH_AES_256_GCM_SHA384,
        hash_provider: &SHA384_HASH,
        confidentiality_limit: 1 << 23,
    },
    kx: KeyExchangeAlgorithm::DHE,
    sign: TLS12_RSA_SCHEMES,
    aead_alg: match rustls::crypto::ring::cipher_suite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384 {
        rustls::SupportedCipherSuite::Tls12(provider) => provider.aead_alg,
        _ => unreachable!(),
    },
    prf_provider: &PrfUsingHmac(&HMAC_SHA384),
};

/// Finite-field Diffie-Hellman key exchange group (`FFDHE2048` / `FFDHE3072`)
/// for TLS 1.2 `TLS_DHE_RSA_*` cipher suites, with fallback support for custom
/// server-supplied `(dh_p, dh_g)` parameters (such as OpenSSL's RFC 3526 MODP
/// groups configured via `SSL_CTX_set_tmp_dh_callback`).
#[derive(Debug)]
pub struct FfdheKxGroup {
    pub name: rustls::NamedGroup,
    pub group: rustls::ffdhe_groups::FfdheGroup<'static>,
}

pub static FFDHE2048_KX_GROUP: FfdheKxGroup = FfdheKxGroup {
    name: rustls::NamedGroup::FFDHE2048,
    group: rustls::ffdhe_groups::FFDHE2048,
};

pub static FFDHE3072_KX_GROUP: FfdheKxGroup = FfdheKxGroup {
    name: rustls::NamedGroup::FFDHE3072,
    group: rustls::ffdhe_groups::FFDHE3072,
};

struct FfdheActiveKx {
    name: rustls::NamedGroup,
    static_group: Option<rustls::ffdhe_groups::FfdheGroup<'static>>,
    p_bytes: Vec<u8>,
    x: num_bigint::BigUint,
    pub_key: Vec<u8>,
}

impl FfdheKxGroup {
    fn start_with_params(
        name: rustls::NamedGroup,
        static_group: Option<rustls::ffdhe_groups::FfdheGroup<'static>>,
        group: rustls::ffdhe_groups::FfdheGroup<'_>,
    ) -> Result<Box<dyn rustls::crypto::ActiveKeyExchange>, Error> {
        use num_bigint::BigUint;
        if group.p.len() < 128 || group.g.is_empty() {
            return Err(Error::from(rustls::PeerMisbehaved::SelectedUnofferedKxGroup));
        }
        let p = BigUint::from_bytes_be(group.p);
        let g = BigUint::from_bytes_be(group.g);
        let one = BigUint::from(1u8);
        if p <= (&one + &one) || g <= one || g >= (&p - &one) {
            return Err(Error::from(rustls::PeerMisbehaved::SelectedUnofferedKxGroup));
        }
        let mut x_bytes = [0u8; 32];
        getrandom::getrandom(&mut x_bytes)
            .map_err(|_| Error::General("FFDHE getrandom failed".into()))?;
        x_bytes[0] |= 0x80;
        let x = BigUint::from_bytes_be(&x_bytes);
        let pub_val = g.modpow(&x, &p);
        let pub_raw = pub_val.to_bytes_be();
        let p_len = group.p.len();
        let mut pub_key = vec![0u8; p_len.max(pub_raw.len())];
        let offset = pub_key.len() - pub_raw.len();
        pub_key[offset..].copy_from_slice(&pub_raw);
        Ok(Box::new(FfdheActiveKx {
            name,
            static_group,
            p_bytes: group.p.to_vec(),
            x,
            pub_key,
        }))
    }
}

impl rustls::crypto::SupportedKxGroup for FfdheKxGroup {
    fn start(&self) -> Result<Box<dyn rustls::crypto::ActiveKeyExchange>, Error> {
        Self::start_with_params(self.name, Some(self.group), self.group)
    }

    fn start_custom_dh(
        &self,
        group: rustls::ffdhe_groups::FfdheGroup<'_>,
    ) -> Option<Result<Box<dyn rustls::crypto::ActiveKeyExchange>, Error>> {
        Some(Self::start_with_params(self.name, None, group))
    }

    fn ffdhe_group(&self) -> Option<rustls::ffdhe_groups::FfdheGroup<'static>> {
        Some(self.group)
    }

    fn name(&self) -> rustls::NamedGroup {
        self.name
    }

    fn usable_for_version(&self, version: rustls::ProtocolVersion) -> bool {
        version == rustls::ProtocolVersion::TLSv1_2
    }
}

impl rustls::crypto::ActiveKeyExchange for FfdheActiveKx {
    fn complete(
        self: Box<Self>,
        peer_pub_key: &[u8],
    ) -> Result<rustls::crypto::SharedSecret, Error> {
        use num_bigint::BigUint;
        let p = BigUint::from_bytes_be(&self.p_bytes);
        let y = BigUint::from_bytes_be(peer_pub_key);
        let one = BigUint::from(1u8);
        if y <= one || y >= (&p - &one) {
            return Err(Error::from(rustls::PeerMisbehaved::InvalidKeyShare));
        }
        let shared = y.modpow(&self.x, &p);
        let raw = shared.to_bytes_be();
        let p_len = self.p_bytes.len();
        let mut padded = vec![0u8; p_len.max(raw.len())];
        let offset = padded.len() - raw.len();
        padded[offset..].copy_from_slice(&raw);
        Ok(rustls::crypto::SharedSecret::from(padded.as_slice()))
    }

    fn pub_key(&self) -> &[u8] {
        &self.pub_key
    }

    fn ffdhe_group(&self) -> Option<rustls::ffdhe_groups::FfdheGroup<'static>> {
        self.static_group
    }

    fn group(&self) -> rustls::NamedGroup {
        self.name
    }
}

#[cfg(test)]
mod t_cbc_1_tests;
