#[allow(unused_imports)]
use super::*;

pub fn register_phase57_nio_file(r: &mut NativeMethodRegistry) {
    // Captured before ANY Path registration (both blocks below, including
    // the later "last-registered (winning)" ones past line 7900) so the
    // mirror after `compareTo` -- see below -- sees the whole family, the
    // same since-before-the-block shape `FILE_STORE_IMPLS`'s mirror uses.
    let __path_rows_before = r.dump_registrations().len();
    let path = "java/nio/file/Path";
    let _paths = "java/nio/file/Paths";
    let files = "java/nio/file/Files";
    let (__fs_rows_before, __fsp_rows_before) = register_phase57_nio_file_a(r);
    register_phase57_nio_file_b(r, __path_rows_before, __fs_rows_before, __fsp_rows_before);
    register_phase57_nio_file_c(r, __path_rows_before, __fs_rows_before, __fsp_rows_before);
    // `p57_alloc_default_filesystem` now mints the DEFAULT FileSystem stamped
    // as the real concrete platform class when
    // `cratonvm_native_api::filesystem_layout` resolves a layout for it (see
    // that function's own doc), so dispatch on a real receiver keys on ITS
    // name first (`H11-1`). Mirror every row either `java/nio/file/FileSystem`
    // block above (`fs_class`, `fsys`) registered under the abstract name onto
    // the concrete one, or a receiver this crate minted would find NEITHER
    // its own registration NOR real bytecode not yet safe to run -- same
    // two-halves rule as every other `alloc_concrete` caller in this file.
    // Harmless when `filesystem_layout::impl_class()` is never the receiver's
    // actual stamp (non-Windows, or a platform/image `resolve` refused): the
    // mirrored rows simply sit on a class name nothing ever mints under.
    cratonvm_native_io::concrete_receiver::mirror_class_registrations(
        r,
        __fs_rows_before,
        "java/nio/file/FileSystem",
        cratonvm_native_api::filesystem_layout::impl_class(),
    );

}

