// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Pure cryptographic compatibility kernels.
//!
//! Java-object marshalling and native registration stay in the
//! `native-builtins` facade. This crate is a separately compiled code-size and
//! incremental-build island with no VM-context dependency.

pub mod bc_aes;
pub mod bc_chacha;
pub mod bc_newhope;
mod bc_newhope_tables;
