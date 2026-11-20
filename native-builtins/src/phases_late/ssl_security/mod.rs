// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! `javax.net.ssl`, `javax.crypto.Mac` and `java.security.cert` natives, plus the DER/ASN.1 helper used for X.509 extraction.
//!
//! Pure code move out of `phases_late.rs` (no logic, signature or ordering
//! changes). Every registration call site is untouched and the per-phase
//! dispatchers stay in the parent module, so the native registration SEQUENCE
//! is byte-identical to before the split.

use super::*;
/// Define an RFC 2104 HMAC over one `digest` hash, with the block size taken
/// from the hash type rather than written down.
///
/// This is `hmac_sha224` above generalised, and it exists because the same
/// reasoning applies six more times. Each of these has a block size that is
/// neither its digest size nor a family default:
///
/// * the SHA-512 truncations keep SHA-512's 128-byte block, NOT the 64 their
///   224/256-bit output would suggest;
/// * SHA-3's block IS its sponge rate — 144/136/104/72 for 224/256/384/512 —
///   which SHRINKS as the digest grows, the opposite of every other family here.
///
/// Every one of those is a number a person would plausibly get wrong, and a
/// wrong HMAC block size yields a MAC that is perfectly self-consistent and
/// interoperates with nothing. `hmac::Hmac<D>` reads it from `D::BlockSize`, so
/// none of them is written down anywhere in this file.
/// `hmac_extended_matches_hotspot` still pins all seven against measured
/// HotSpot 25 output, including a 200-byte key — the case that exercises the
/// "key longer than the block gets hashed first" branch, which is where a wrong
/// block size first shows up.
macro_rules! hmac_over_digest {
    ($name:ident, $digest:ty) => {
        fn $name(key: &[u8], data: &[u8]) -> Vec<u8> {
            use hmac::Mac as _;
            // `new_from_slice` is infallible for HMAC (any key length is legal),
            // exactly as in `hmac_sha224`.
            let mut mac = hmac::Hmac::<$digest>::new_from_slice(key)
                .expect("HMAC accepts a key of any length");
            mac.update(data);
            mac.finalize().into_bytes().to_vec()
        }
    };
}

// =============================================================================
// NEW-13 regression tests: validate that the real javax.net.ssl path is
// registered with the expected signatures and that the unified s2_registry
// TLS helpers behave correctly for error cases.
// =============================================================================

#[cfg(test)]
pub mod new13_tests;

/// gc-common w11-a: the `getKeyManagers()` cache is keyed per VM and goes
/// with its VM's factory rows.
#[cfg(test)]
mod w11a_key_manager_cache_tests;

/// gc-common w13-a: the p68 custom-trust-anchor rows are keyed per VM and go
/// with their context / factory; a factory's death drops its identity rows
/// and offers the state they named. (gc-common w27-a: by weak lock keys.)
#[cfg(test)]
mod w13a_factory_row_tests;

#[cfg(test)]
mod w14b_parked_socket_tests;

/// gc-common w15-d: references held across GC points (a moving collection is
/// modelled by the mock's `remap_native_pin_addr_for_test`, applied from an
/// `invoke_virtual` hook -- the only GC point the mock can express), and the
/// identity tables re-keyed by `(VM, identity hash)`.
#[cfg(test)]
mod w15d_stale_reference_tests;

/// gc-common w16-d (`common-w15d-unclosed-ssl-socket-rows-outlive-the-socket-FIXED-20260928`):
/// an `SSLSocket` that dies without `close()` takes its staged trust scope,
/// its parked connection (closed) and its listener rows with it, and its
/// listeners stop being global roots at the VM's next socket native. Also the
/// per-VM `Mac` eviction. Each test has its own VM identities and a
/// `Teardown` guard (Drop).
#[cfg(test)]
mod w16d_unclosed_socket_tests;

/// gc-common w27-a (`common-w26b-tls-sibling-tables-keyed-by-vm-folded-identity-hash`):
/// every per-object table of this module is keyed by `(VM, weak lock key)`.
/// The mock's identity hash is the address truncated to `i32`, so two
/// addresses 4 GiB apart share a hash -- two LIVE colliders of one VM. Each
/// test has its own VM identities, and its guard forgets only those (TLS
/// state first, then the lock keys, as VM teardown does).
#[cfg(test)]
mod w27a_ssl_object_key_tests;

/// gcd d1/e (`common-w15d-unclosed-ssl-socket-rows-outlive-the-socket-FIXED-20260928`,
/// option 2): a socket built by `alloc_ssl_socket` keeps its
/// `HandshakeCompletedListener`s in its own last slot, so no global root is
/// taken and a listener that captures its socket no longer keeps the socket
/// alive. A socket without the slot keeps the side table (the two tests above
/// pin that arm).
#[cfg(test)]
mod gcd_d1e_listener_slot_tests;

mod p68_signature_failure;
#[allow(unused_imports)]
pub use p68_signature_failure::*;
#[allow(unused_imports)]
use p68_signature_failure::*;
mod dead_socket_listener_roots;
#[allow(unused_imports)]
pub use dead_socket_listener_roots::*;
#[allow(unused_imports)]
use dead_socket_listener_roots::*;
mod register_p68_ssl;
#[allow(unused_imports)]
pub use register_p68_ssl::*;
#[allow(unused_imports)]
use register_p68_ssl::*;
mod jdk_body_for_engine_subclass;
#[allow(unused_imports)]
pub use jdk_body_for_engine_subclass::*;
#[allow(unused_imports)]
use jdk_body_for_engine_subclass::*;
