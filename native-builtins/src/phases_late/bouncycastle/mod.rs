// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Bouncy Castle kernels: EC field/point arithmetic (Fp/F2m/SecT), block and stream ciphers, digests, KDFs and PRNGs.
//!
//! Pure code move out of `phases_late.rs` (no logic, signature or ordering
//! changes). Every registration call site is untouched and the per-phase
//! dispatchers stay in the parent module, so the native registration SEQUENCE
//! is byte-identical to before the split.

use super::*;

use crate::lang_misc::bytecode_on_null_arg;
use crate::lang_misc::bytecode_on_own_refusal;

/// Register a plain BouncyCastle shadow that never sees a null object
/// argument: a call with one runs the library's bytecode instead (which
/// answers with HotSpot's JEP 358 text naming BC's own parameter, BC's own
/// check, or no exception where the bytecode never touches it), so
/// `obj_arg`'s "null object argument" never surfaces
/// (`null_arg_runs_shadowed_bytecode`; interpreter round i5 wave 107, lane L7,
/// `i106-L7-bc-plain-shadows-answer-null-arrays-with-their-own-npe`). Every
/// plain BC registration in this file goes through it; the shadow's kind is
/// the registrar's ambient one (each registrar states `Intrinsic`).
/// `$class`, `$name` and `$desc` must be constants (a literal, a `const`, or a
/// `concat!`), never a `let` binding or a loop variable.
macro_rules! bc_register_null_rerun {
    ($r:expr, $class:expr, $name:expr, $desc:expr, $body:expr $(,)?) => {
        $r.register(
            $class,
            $name,
            $desc,
            bytecode_on_null_arg!($body, $class, $name, $desc), cratonvm_native_api::NativeKind::SyntheticStub,
        )
    };
}

/// Register a BouncyCastle shadow whose OWN refusals (an index check, a null
/// argument) re-run the library's bytecode, which answers with BC's own
/// exception or the JVM's `Index i out of bounds for length n` at the access,
/// and BC's partial effects, as on HotSpot (`own_refusal_runs_shadowed_bytecode`;
/// interpreter round i5 wave 103, lane L7, `i100-L7-native-shadow-npe-texts`).
/// The shadow's kind is the registrar's ambient one. The body must refuse
/// before it writes anything the bytecode reads.
macro_rules! bc_register_rerun {
    ($r:expr, $class:literal, $name:literal, $desc:literal, $body:expr $(,)?) => {
        $r.register(
            $class,
            $name,
            $desc,
            bytecode_on_own_refusal!($body, $class, $name, $desc), cratonvm_native_api::NativeKind::SyntheticStub,
        )
    };
}

macro_rules! define_bc_sect_module {
    ($module:ident, $m:expr, [$($ks:expr),+ $(,)?]) => {
        mod $module {
            use super::*;
            const KS: &[usize] = &[$($ks),+];
            const SPEC: BcSecTFieldSpec = BcSecTFieldSpec { m: $m, ks: KS };

            pub(super) fn add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_add_native(ctx, args, SPEC)
            }
            pub(super) fn add_both_to(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_add_both_to_native(ctx, args, SPEC)
            }
            pub(super) fn add_ext(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_add_ext_native(ctx, args, SPEC)
            }
            pub(super) fn add_one(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_add_one_native(ctx, args, SPEC)
            }
            pub(super) fn multiply(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_multiply_native(ctx, args, SPEC)
            }
            pub(super) fn multiply_add_to_ext(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_multiply_add_to_ext_native(ctx, args, SPEC)
            }
            pub(super) fn reduce(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_reduce_native(ctx, args, SPEC)
            }
            pub(super) fn sqrt(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_sqrt_native(ctx, args, SPEC)
            }
            pub(super) fn square(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_square_native(ctx, args, SPEC)
            }
            pub(super) fn square_add_to_ext(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_square_add_to_ext_native(ctx, args, SPEC)
            }
            pub(super) fn square_n(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_square_n_native(ctx, args, SPEC)
            }
            pub(super) fn invert(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_invert_native(ctx, args, SPEC)
            }
            pub(super) fn half_trace(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_half_trace_native(ctx, args, SPEC)
            }
            pub(super) fn trace(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_trace_native(ctx, args, SPEC)
            }
            pub(super) fn precomp_multiplicand(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_precomp_multiplicand_native(ctx, args, SPEC)
            }
            pub(super) fn multiply_precomp(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_multiply_precomp_native(ctx, args, SPEC)
            }
            pub(super) fn multiply_precomp_add_to_ext(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_multiply_precomp_add_to_ext_native(ctx, args, SPEC)
            }
        }
    };
}


macro_rules! register_bc_sect_module {
    ($r:expr, $class:literal, $module:ident) => {{
        bc_register_rerun!($r, $class, "add", "([J[J[J)V", $module::add);
        bc_register_rerun!($r, $class, "addBothTo", "([J[J[J)V", $module::add_both_to);
        bc_register_rerun!($r, $class, "addExt", "([J[J[J)V", $module::add_ext);
        bc_register_rerun!($r, $class, "addOne", "([J[J)V", $module::add_one);
        bc_register_rerun!($r, $class, "multiply", "([J[J[J)V", $module::multiply);
        bc_register_rerun!(
            $r,
            $class,
            "multiplyAddToExt",
            "([J[J[J)V",
            $module::multiply_add_to_ext,
        );
        bc_register_rerun!($r, $class, "reduce", "([J[J)V", $module::reduce);
        bc_register_rerun!($r, $class, "sqrt", "([J[J)V", $module::sqrt);
        bc_register_rerun!($r, $class, "square", "([J[J)V", $module::square);
        bc_register_rerun!(
            $r,
            $class,
            "squareAddToExt",
            "([J[J)V",
            $module::square_add_to_ext,
        );
        bc_register_rerun!($r, $class, "squareN", "([JI[J)V", $module::square_n);
        bc_register_rerun!($r, $class, "invert", "([J[J)V", $module::invert);
        bc_register_rerun!($r, $class, "halfTrace", "([J[J)V", $module::half_trace);
        bc_register_rerun!($r, $class, "trace", "([J)I", $module::trace);
    }};
}

macro_rules! define_bc_sect_point_module {
    ($module:ident, $class:literal) => {
        mod $module {
            use super::*;

            pub(super) fn twice(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                let spec = bc_sect_point_spec_for_class($class)
                    .ok_or_else(|| bc_fp_bad_state("SecTPoint: missing point spec"))?;
                bc_sect_point_return_twice_with_spec(ctx, args, spec)
            }
        }
    };
}

macro_rules! register_bc_sect_point {
    ($r:expr, $class:literal, $module:ident) => {
        bc_register_null_rerun!(
            $r,
            $class,
            "twice",
            "()Lorg/bouncycastle/math/ec/ECPoint;",
            $module::twice,
        );
    };
}

#[cfg(test)]
pub mod bc_longarray_poly_tests;

mod bc_has_any_small_factors;
#[allow(unused_imports)]
pub use bc_has_any_small_factors::*;
#[allow(unused_imports)]
use bc_has_any_small_factors::*;
mod bc_whirlpool_tables;
#[allow(unused_imports)]
pub use bc_whirlpool_tables::*;
#[allow(unused_imports)]
use bc_whirlpool_tables::*;
mod bc_md5_slots;
#[allow(unused_imports)]
pub use bc_md5_slots::*;
#[allow(unused_imports)]
use bc_md5_slots::*;
