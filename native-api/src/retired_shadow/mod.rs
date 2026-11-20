/// The original linear walk [`compat_family_of_class`] replaced; the
/// equivalence test compares the two over every class name the tables know.
#[cfg(test)]
fn compat_family_of_class_linear(class_name: &str) -> Option<&'static CompatFamily> {
    COMPAT_FAMILIES
        .iter()
        .filter(|f| family_exists_on_this_platform(f.row))
        .find(|f| f.names_class(f.classes, class_name))
}

/// The original linear walk [`compat_row_of_stub_class`] replaced (tests).
#[cfg(test)]
fn compat_row_of_stub_class_linear(class_name: &str) -> Option<cratonvm_types::HotSpotAlignment> {
    COMPAT_FAMILIES
        .iter()
        .filter(|f| family_exists_on_this_platform(f.row))
        .find(|f| f.names_class(f.stub_classes, class_name))
        .map(|f| f.row)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod nio21_w12_l3_jdk_invoke_family_tests;

/// nio21 close: the `JdkArchiveFs` row takes exactly the archive-provider rows
/// and their mirrored `newFileSystem(Path, Map)` copies on the concrete provider
/// classes, and nothing else of `FileSystems` / `FileSystemProvider`.
#[cfg(test)]
mod nio21_close_jdk_archive_fs_tests;

/// JIT round 20 wave 14 (lane cjavac): the `JdkJavacFileManager` row takes
/// exactly the eight `javac` file-manager Bridges, none of which is a retired
/// shadow, and leaves `javac`'s other natives (`StandardLocation`, `Name`)
/// alone.
#[cfg(test)]
mod jr20_cjavac_jdk_javac_file_manager_tests;

/// JIT round 20 wave 17 (lane bridge): the `--real-jdk` yielded-bridge rows are
/// not registered by a strict real-JDK registry as `Bridge`s, and every other
/// shape (another mode, no real JDK, another kind) keeps them.
#[cfg(test)]
mod jr20_bridge_jdk_only_yielded_bridge_tests;

/// JIT round j27 wave 5 (lane VMALIGN): the `JdkVmInitLevel` row takes exactly
/// `VM.initLevel()` and `VM.isBooted()` of the SBR System / L5 tables,
/// leaves the other retired `VM` rows native, and drops both registrations
/// under `--compatible` over a real JDK only.
#[cfg(test)]
mod j27_vmalign_jdk_vm_init_level_tests;

mod retired_shadow_triples;
#[allow(unused_imports)]
pub use retired_shadow_triples::*;
#[allow(unused_imports)]
use retired_shadow_triples::*;
mod is_parallel_capable_loader_shadow;
#[allow(unused_imports)]
pub use is_parallel_capable_loader_shadow::*;
#[allow(unused_imports)]
use is_parallel_capable_loader_shadow::*;
mod is_carrier_thread_local_family_row;
#[allow(unused_imports)]
pub use is_carrier_thread_local_family_row::*;
#[allow(unused_imports)]
use is_carrier_thread_local_family_row::*;
mod l1_triples;
#[allow(unused_imports)]
pub use l1_triples::*;
#[allow(unused_imports)]
use l1_triples::*;
mod retired_triple_is_native_before;
#[allow(unused_imports)]
pub use retired_triple_is_native_before::*;
#[allow(unused_imports)]
use retired_triple_is_native_before::*;
mod l1_lhm_triples;
#[allow(unused_imports)]
pub use l1_lhm_triples::*;
#[allow(unused_imports)]
use l1_lhm_triples::*;
mod sbr_reflect_triples;
#[allow(unused_imports)]
pub use sbr_reflect_triples::*;
#[allow(unused_imports)]
use sbr_reflect_triples::*;
mod sbr_nioreal_windows_triples;
#[allow(unused_imports)]
pub use sbr_nioreal_windows_triples::*;
#[allow(unused_imports)]
use sbr_nioreal_windows_triples::*;
mod r16_nio2_mapped_triples;
#[allow(unused_imports)]
pub use r16_nio2_mapped_triples::*;
#[allow(unused_imports)]
use r16_nio2_mapped_triples::*;
