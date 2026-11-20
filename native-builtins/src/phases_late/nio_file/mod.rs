// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.nio.file` natives: Path/Paths/Files, FileSystem providers (default, jar, jrt), file attributes, FileVisitor/WatchService, RandomAccessFile/File/FileChannel.
//!
//! Pure code move out of `phases_late.rs` (no logic, signature or ordering
//! changes). Every registration call site is untouched and the per-phase
//! dispatchers stay in the parent module, so the native registration SEQUENCE
//! is byte-identical to before the split.

use super::*;
// ---------------------------------------------------------------------------
// java.nio.file — Path extras, Files extras, FileVisitResult, OpenOption
// ---------------------------------------------------------------------------

/// The slot map of a synthetic `java/nio/file/Path`.
///
/// **The body lives in `cratonvm_native_api::path_layout`**, because this
/// carrier has two producing crates — this file's `p57_alloc_path` and the
/// jar/jrt walkers beside it, plus `native-io`'s watch-key path rebuild — and a
/// slot map kept here would be a second, drifting copy over there. Read that
/// module for what the two hard-coded constants this replaced were doing wrong
/// and why the earlier by-name attempts through the object were inert.
pub use cratonvm_native_api::path_layout::{slots as p57_path_slots, slots_for as p57_path_slots_for};

/// Resolve (once) and return the Path slot map — the allocator's arm of
/// [`p57_path_slots`].
pub use cratonvm_native_api::path_layout::resolve as p57_path_slots_init;
/// `return` the receiver's real bytecode result when
/// [`yield_to_retired_receiver`] says the receiver's triple is retired.
macro_rules! retired_receiver_yield {
    ($ctx:expr, $args:expr, $m:literal, $d:literal) => {
        if let Some(r) = yield_to_retired_receiver($ctx, $args, $m, $d) {
            return r;
        }
    };
}

/// `r.register(class, method, descriptor, callback, cratonvm_native_api::NativeKind::Bridge)` for a row that takes a
/// `Path` of the JDK's `java.nio.file.$owner`: the callback runs unless
/// [`yield_to_foreign_path`] hands the call to the JDK body.
macro_rules! reg_foreign_aware {
    ($owner:literal, $r:ident . register ( $class:expr, $m:literal, $d:literal, $cb:expr, $kind:expr $(,)? )) => {{
        fn yielding(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
            if let Some(res) = yield_to_foreign_path(ctx, args, $owner, $m, $d) {
                return res;
            }
            let inner: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult = $cb;
            inner(ctx, args)
        }
        $r.register($class, $m, $d, yielding, $kind);
    }};
}
// The registration stays spelled `r.register(..)` INSIDE the braces, so the
// source scanners (`registrar_drift`, `registrar_reachability`) still see it.
macro_rules! reg_files {
    ($($t:tt)*) => {
        reg_foreign_aware!("java/nio/file/Files", $($t)*)
    };
}
macro_rules! reg_path {
    ($($t:tt)*) => {
        reg_foreign_aware!("java/nio/file/Path", $($t)*)
    };
}

#[cfg(test)]
pub mod p57_win_path_tests;

#[cfg(test)]
pub mod p57_name_element_tests;

#[cfg(test)]
#[cfg(not(windows))]
pub mod p57_posix_path_tests;

#[cfg(test)]
pub mod p57_normalize_relativize_tests;
/// `G22-1` §5(a): the three MEASURED divergences between `Paths.get`'s
/// Windows-syntax handling and `sun.nio.fs.WindowsPathParser`, transcribed
/// from `lib/src.zip` (JDK 25.0.3+9-LTS) rather than derived.
#[cfg(test)]
#[cfg(windows)]
mod p57_windows_path_normalize_tests;
#[cfg(test)]
mod r15w6_jni6_path_join_tests;
#[cfg(test)]
mod delete_on_exit_tests;
/// Register one path-only `WindowsNativeDispatcher` native (see
/// [`win32_path_native_body`]). A macro because `register` takes a plain `fn`
/// pointer, so the per-native name/arity/`op` cannot be captured.
#[cfg(windows)]
macro_rules! win32_register_path_native {
    ($r:expr, $name:literal, $desc:literal, $npaths:expr, $op:expr) => {
        $r.register(
            "sun/nio/fs/WindowsNativeDispatcher",
            $name,
            $desc,
            |ctx, args| win32_path_native_body(ctx, args, $name, $npaths, $op),
            cratonvm_native_api::NativeKind::Bridge,
        )
    };
}

#[cfg(test)]
mod file_normalise_tests;
#[cfg(all(test, windows))]
mod registry_content_type_tests;

/// Tests for the shape of the `java.nio.file` exceptions this module raises.
///
/// Each case is anchored to a measured HotSpot answer (JDK 25, `NioExcShape`
/// probe, 2026-08-11) rather than to what the code used to do.
#[cfg(test)]
mod exception_shape_tests;

#[cfg(test)]
mod named_attribute_tests;
/// The enum-constant minting laws this file has already broken once.
///
/// Every one of these drives a real function through `MockNativeContext`; none
/// asserts only that an object is non-null, because a NAMELESS enum constant
/// is non-null and passes every null check — that is the whole failure mode.
#[cfg(test)]
mod e36_enum_constant_tests;
#[cfg(test)]
mod g22_glob_translation_tests;

/// gc-common w18-f: `p61_build_network_interfaces` roots what it builds in
/// handle scopes. The mock never moves an object, so it cannot show a
/// relocation; what it can show is that every interface comes back complete
/// (each fresh object was stored where it belongs) and that the scopes closed,
/// leaving no pin behind.
#[cfg(test)]
mod w18f_network_interface_tests;

#[cfg(test)]
mod w20d_option_relocation_tests;

#[cfg(test)]
mod w21b_read_path_relocation_tests;
#[cfg(test)]
mod gci_i6jd_foreign_path_receiver_tests;

#[cfg(all(test, windows))]
mod access_check_tests;

/// JIT round 22 wave 2, lane sync (nio20 W3L1-4): open options by exact name.
#[cfg(test)]
mod r22_sync_open_options_by_name_tests;

mod register_phase57_natives;
#[allow(unused_imports)]
pub use register_phase57_natives::*;
#[allow(unused_imports)]
use register_phase57_natives::*;
mod register_phase57_nio_file;
#[allow(unused_imports)]
pub use register_phase57_nio_file::*;
#[allow(unused_imports)]
use register_phase57_nio_file::*;
mod p57_line_separator;
#[allow(unused_imports)]
pub use p57_line_separator::*;
#[allow(unused_imports)]
use p57_line_separator::*;
mod p57_resolve_paths;
#[allow(unused_imports)]
pub use p57_resolve_paths::*;
#[allow(unused_imports)]
use p57_resolve_paths::*;
mod fs_list_dir;
#[allow(unused_imports)]
pub use fs_list_dir::*;
#[allow(unused_imports)]
use fs_list_dir::*;
mod win_filetime_from_millis;
#[allow(unused_imports)]
pub use win_filetime_from_millis::*;
#[allow(unused_imports)]
use win_filetime_from_millis::*;
mod register_p70_file_attributes;
#[allow(unused_imports)]
pub use register_p70_file_attributes::*;
#[allow(unused_imports)]
use register_p70_file_attributes::*;
mod register_phase57_nio_file_a;
use register_phase57_nio_file_a::*;
mod register_phase57_nio_file_b;
use register_phase57_nio_file_b::*;
mod register_phase57_nio_file_c;
use register_phase57_nio_file_c::*;
