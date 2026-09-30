// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Pure cryptographic compatibility kernels.
//!
//! Java-object marshalling and native registration stay in the
//! `native-builtins` facade. This crate is a separately compiled code-size and
//! incremental-build island with no VM-context dependency.

// JDK-ONLY-CLASSIFY: not applicable — this crate contains ZERO calls to
// `NativeMethodRegistry::register` and ZERO `set_category`/`with_category`
// calls. It exports pure kernels that `native-builtins` marshals and registers
// on its behalf, so the ambient-category footgun cannot bite here. The
// classification of anything backed by these kernels is decided at the
// `native-builtins` call site, not here.
// See jdk-only-ambient-category-audit.md.
pub mod bc_aes;
pub mod bc_chacha;
pub mod bc_digest;
pub mod bc_newhope;
mod bc_newhope_tables;
/// The fail-loud error type shared by every kernel in this crate. See
/// `docs/security/crypto-failure-contract.md` for the rule it enforces:
/// a cryptographic kernel never encodes failure as ordinary output.
pub mod failure;
pub mod signature;
