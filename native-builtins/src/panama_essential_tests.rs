use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn register_essential_includes_symbol_lookup_bridge() {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);
    let symbol_lookup = "java/lang/foreign/SymbolLookup";

    assert!(registry
        .find(
            symbol_lookup,
            "libraryLookup",
            "(Ljava/lang/String;Ljava/lang/foreign/Arena;)Ljava/lang/foreign/SymbolLookup;"
        )
        .is_some());
    assert!(registry
        .find(
            symbol_lookup,
            "loaderLookup",
            "()Ljava/lang/foreign/SymbolLookup;"
        )
        .is_some());
    assert!(registry
        .find(
            symbol_lookup,
            "find",
            "(Ljava/lang/String;)Ljava/util/Optional;"
        )
        .is_some());
    let linker_option = "java/lang/foreign/Linker$Option";
    assert!(registry
        .find(
            linker_option,
            "critical",
            "(Z)Ljava/lang/foreign/Linker$Option;"
        )
        .is_some());
}
