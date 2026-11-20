use super::stackless_ancestor_native_admitted;
use crate::vm::SharedVm;
use cratonvm_classloading::{ClassLoaderId, DefineClassOptions};
use cratonvm_native_api::{NativeContext, NativeKind};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ClassId, Value};

const CODE_BRIDGE: &str = "gciju/CodeBridgeOwner";
const CODE_INTRINSIC: &str = "gciju/CodeIntrinsicOwner";
const NATIVE_BRIDGE: &str = "gciju/NativeBridgeOwner";
const NEVER_LOADED: &str = "gciju/NeverLoadedOwner";

fn dummy_native(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// A version-52 class `name extends java/lang/Object` with one method
/// `public static value()I`: `ldc #8; ireturn` (`#8` the integer 1), or,
/// with `native`, `public static native value()I` and no `Code`. The same
/// shape as `dispatch_virtual.rs`'s `owner_scoped_eviction_tests`.
fn class_file(name: &str, native: bool) -> Vec<u8> {
    fn utf8(out: &mut Vec<u8>, s: &str) {
        out.push(1);
        out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    b.extend_from_slice(&9u16.to_be_bytes()); // constant_pool_count
    utf8(&mut b, name); // #1
    b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
    utf8(&mut b, "java/lang/Object"); // #3
    b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
    utf8(&mut b, "value"); // #5
    utf8(&mut b, "()I"); // #6
    utf8(&mut b, "Code"); // #7
    b.push(3); // #8 Integer
    b.extend_from_slice(&1i32.to_be_bytes());
    b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
    b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
    b.extend_from_slice(&[0, 0, 0, 0]); // interfaces, fields
    b.extend_from_slice(&[0, 1]); // methods_count
    if native {
        // public static native value()I, no attributes
        b.extend_from_slice(&[0x01, 0x09, 0, 5, 0, 6, 0, 0]);
        b.extend_from_slice(&[0, 0]); // no class attributes
        return b;
    }
    b.extend_from_slice(&[0x00, 0x09, 0, 5, 0, 6, 0, 1]); // public static value()I
    let code = [0x12, 8, 0xac]; // ldc #8; ireturn
    b.extend_from_slice(&[0, 7]); // "Code"
    b.extend_from_slice(&(12 + code.len() as u32).to_be_bytes());
    b.extend_from_slice(&[0, 1, 0, 0]); // max_stack 1, max_locals 0
    b.extend_from_slice(&(code.len() as u32).to_be_bytes());
    b.extend_from_slice(&code);
    b.extend_from_slice(&[0, 0, 0, 0]); // no handlers, no attributes
    b.extend_from_slice(&[0, 0]); // no class attributes
    b
}

fn define(shared: &SharedVm, name: &str, native: bool) -> Option<ClassId> {
    shared
        .classes
        .class_manager
        .write()
        .define_class_with_options(
            name,
            &class_file(name, native),
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
        .ok()
}

/// A `Bridge` registered on an ancestor whose method has `Code` is refused
/// (bytecode first, as the populate walk decides); a reviewed `Intrinsic`
/// over the same shape, a `Bridge` over an `ACC_NATIVE` method, and a
/// registration whose owner is not loaded keep their native.
#[test]
fn gci_ju_stackless_walk_admits_only_what_resolve_dispatch_admits() {
    let mut shared = SharedVm::new(crate::config::VmConfig::default());
    if shared.config.compatibility_mode.is_jdk_only() {
        // The admission is policy-independent for these shapes, but the
        // fixture is written for the embedding default.
        return;
    }
    for (owner, kind) in [
        (CODE_BRIDGE, NativeKind::Bridge),
        (CODE_INTRINSIC, NativeKind::Intrinsic),
        (NATIVE_BRIDGE, NativeKind::Bridge),
        (NEVER_LOADED, NativeKind::Bridge),
    ] {
        shared
            .natives
            .native_methods
            .register(owner, "value", "()I", dummy_native, kind);
    }
    let defined = [
        define(&shared, CODE_BRIDGE, false),
        define(&shared, CODE_INTRINSIC, false),
        define(&shared, NATIVE_BRIDGE, true),
    ];
    if defined.iter().any(Option::is_none) {
        // The stripped test VM could not define them: nothing to run.
        return;
    }
    let cm = shared.classes.class_manager.read();
    let admitted =
        |owner: &str| stackless_ancestor_native_admitted(&shared, &cm, owner, "value", "()I");
    assert!(
        !admitted(CODE_BRIDGE),
        "a Bridge in front of the ancestor's own bytecode ran on the cold walk only"
    );
    assert!(admitted(CODE_INTRINSIC), "a reviewed Intrinsic may shadow bytecode");
    assert!(admitted(NATIVE_BRIDGE), "an ACC_NATIVE method's registration is its body");
    assert!(admitted(NEVER_LOADED), "an owner that is not loaded keeps the legacy answer");
}

/// Both arms of the stackless walk (the shared helper and the
/// `CRATONVM_ANCESTOR_WALK_HELPER=0` loop) ask the admission. The needle is
/// built at run time so this module's own text never matches.
#[test]
fn gci_ju_both_stackless_walk_arms_ask_the_admission() {
    let src = include_str!("../invoke.rs");
    let needle = format!("&& {}(", "stackless_ancestor_native_admitted");
    let alt = format!(") && {}(", "stackless_ancestor_native_admitted");
    let sites = src
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("//") && (t.starts_with(&needle) || t.starts_with(&alt))
        })
        .count();
    assert_eq!(sites, 2, "expected the helper arm and the loop arm, found {sites}");
}
