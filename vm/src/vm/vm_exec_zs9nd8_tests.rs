// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! zs s9/nd8 tests (ZGC round zs, wave s9, lane nd8), a child module of
//! `vm_exec` (`#[path]`).
//!
//! `zgc-zs5nd5-proxy-loader-leak-and-upcall-abort-residuals-SUPERSEDED-20261019.md` §5 /
//! `zgc-zs7nd6-native-door-residuals-FIXED-20261019.md` §4: `Class.getModule()` of
//! a generated proxy class that a USER loader defined answers ONE module,
//! filed per loader (`record_layer_module_packages`) and never in the VM's
//! permanent by-name module table, whose strong root kept every such loader
//! alive (nd5 F2, `CRATONVM_PROXY_MODULE_PER_LOADER`).

use super::*;
use crate::config::{GcAlgorithm, VmConfig};

const DEFINE1_DESC: &str = "(Ljava/lang/ClassLoader;Ljava/lang/String;[BIILjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;";

fn utf8(b: &mut Vec<u8>, text: &str) {
    b.push(1);
    b.extend_from_slice(&(text.len() as u16).to_be_bytes());
    b.extend_from_slice(text.as_bytes());
}

/// `<access> class <name> extends <super> {}` (version 52).
fn class_bytes(name: &str, super_name: &str, access: u16) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52]);
    b.extend_from_slice(&5u16.to_be_bytes()); // cp 1..4
    b.push(7);
    b.extend_from_slice(&2u16.to_be_bytes());
    utf8(&mut b, name);
    b.push(7);
    b.extend_from_slice(&4u16.to_be_bytes());
    utf8(&mut b, super_name);
    b.extend_from_slice(&access.to_be_bytes());
    b.extend_from_slice(&1u16.to_be_bytes());
    b.extend_from_slice(&3u16.to_be_bytes());
    b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
    b
}

/// What one run answered: the two `getModule()` results were one object,
/// and whether the by-name module table holds the proxy's module name.
struct Answer {
    same: bool,
    by_name: bool,
}

/// A user loader (`zs9nd8/Ldr extends java/lang/ClassLoader`, instance
/// allocated without a constructor), a `jdk/proxy1/$Proxy0` extending the
/// proxy super defined by it through `ClassLoader.defineClass1`, then
/// `Class.getModule()` on its mirror twice.
fn run(kill: bool) -> Answer {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Zgc,
        max_heap_size: 64 << 20,
        ..VmConfig::default()
    });
    let _off = kill.then(|| {
        cratonvm_types::flags::override_thread(cratonvm_types::flags::VmFlags::from_env_with_edits(
            &[("CRATONVM_PROXY_MODULE_PER_LOADER", Some("0"))],
        ))
    });
    let mut thread = JvmThread::new(ThreadId(3911), "zs9nd8-proxy-module");
    let proxy_super = if cratonvm_native_builtins::reflect_annotations::real_proxy_super() {
        "java/lang/reflect/Proxy"
    } else {
        "java/lang/reflect/Proxy$Instance"
    };
    let (loader, mirror_cb, define_cb) = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        ctx.ensure_class_initialized(proxy_super)
            .unwrap_or_else(|e| panic!("{proxy_super} loads in the unit VM: {e:?}"));
        let ldr = ctx
            .define_class_from_bytes(
                "zs9nd8/Ldr",
                &class_bytes("zs9nd8/Ldr", "java/lang/ClassLoader", 0x0021),
            )
            .expect("a ClassLoader subclass defines in the unit VM");
        let fields = ctx.class_num_total_fields(ldr);
        let loader = ctx.alloc_object(ldr, fields);
        let define_cb = shared
            .natives
            .native_methods
            .find("java/lang/ClassLoader", "defineClass1", DEFINE1_DESC)
            .expect("defineClass1 is registered");
        let mirror_cb = shared
            .natives
            .native_methods
            .find("java/lang/Class", "getModule", "()Ljava/lang/Module;")
            .expect("Class.getModule is registered");
        (loader, mirror_cb, define_cb)
    };
    let base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(loader);
    let bytes = class_bytes("jdk/proxy1/$Proxy0", proxy_super, 0x0031);
    let (name_obj, array) = {
        let mut ctx = NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        let name_obj = ctx.create_string("jdk.proxy1.$Proxy0");
        let array = ctx.new_array(ArrayElementType::Byte, bytes.len());
        assert!(ctx.write_byte_array_from(array, 0, &bytes));
        (name_obj, array)
    };
    thread.native_pin_roots.push(name_obj);
    thread.native_pin_roots.push(array);
    let loader = thread.native_pin_roots[base];
    let args = [
        Value::Object(Some(loader)),
        Value::Object(Some(name_obj)),
        Value::Object(Some(array)),
        Value::Int(0),
        Value::Int(bytes.len() as i32),
        Value::Object(None),
        Value::Object(None),
    ];
    let mirror = match safe_native_call(&shared, &mut thread, define_cb, &args) {
        Ok(Some(Value::Object(Some(m)))) => m,
        other => panic!("the proxy class defines for the user loader: {other:?}"),
    };
    thread.native_pin_roots.push(mirror);
    let mirror_slot = thread.native_pin_roots.len() - 1;
    let module = |thread: &mut JvmThread| {
        let mirror = thread.native_pin_roots[mirror_slot];
        match safe_native_call(&shared, thread, mirror_cb, &[Value::Object(Some(mirror))]) {
            Ok(Some(Value::Object(Some(m)))) => m,
            other => panic!("getModule answers a Module: {other:?}"),
        }
    };
    let first = module(&mut thread);
    thread.native_pin_roots.push(first);
    let first_slot = thread.native_pin_roots.len() - 1;
    let second = module(&mut thread);
    let first = thread.native_pin_roots[first_slot];
    let ctx = NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    let by_name = ctx.get_cached_module_mirror(Some("jdk.proxy1")).is_some();
    drop(ctx);
    thread.native_pin_roots.truncate(base);
    eprintln!(
        "zs9nd8 proxy getModule kill={kill}: same={} by_name={by_name}",
        first == second
    );
    Answer {
        same: first == second,
        by_name,
    }
}

#[test]
fn zs9nd8_a_user_loaders_proxy_module_is_one_object_filed_per_loader() {
    let a = run(false);
    assert!(a.same, "getModule() answers one Module for the proxy class");
    assert!(
        !a.by_name,
        "the proxy's module is not in the permanent by-name table"
    );
}

/// Kill control (`CRATONVM_PROXY_MODULE_PER_LOADER=0`, the pre-nd5 mint):
/// the module goes into the by-name table, whose strong root kept the loader.
#[test]
fn zs9nd8_the_kill_switch_caches_the_proxy_module_by_name() {
    let a = run(true);
    assert!(a.same);
    assert!(a.by_name, "kill arm: cached by name (the leak's root)");
}

/// `zgc-zs9rv6-wave-s8-review-residuals-FIXED-20261019.md` item 4: unit rows for the
/// `--compatible` definers nd7 converted. Each row: a clean define, then a
/// define with the whole ladder spent (every fallible door refuses), then the
/// same name again with room -- `OutOfMemoryError` with nothing defined, then
/// the class -- and no pin left. Row: the defensive
/// `jdk.internal.misc.Unsafe.defineClass` (`unsafe_define_class_defensive`),
/// registered in the unit VM (`Compatible`). The public
/// `ClassLoader.defineClass(String, byte[], int, int)` (`cl_define_class_basic`)
/// is not registered there; its panic arm is pinned in native-builtins
/// (`classloader::zs9nd8_define_panic_pin_tests`).
mod compatible_definer_rows {
    use super::*;
    use crate::runtime::native_oom::forced_try_refuse_all;

    #[derive(Clone, Copy, Debug)]
    enum Body {
        UnsafeDefensive,
    }

    impl Body {
        fn find(self, shared: &SharedVm) -> NativeCallback {
            let (class, method, desc) = match self {
                Body::UnsafeDefensive => (
                    "jdk/internal/misc/Unsafe",
                    "defineClass",
                    "(Ljava/lang/String;[BIILjava/lang/ClassLoader;Ljava/security/ProtectionDomain;)Ljava/lang/Class;",
                ),
            };
            shared
                .natives
                .native_methods
                .find(class, method, desc)
                .unwrap_or_else(|| panic!("{class}.{method}{desc} is registered in the unit VM"))
        }
    }

    /// One define of `name` through `body`; `loader_slot` is the pinned
    /// loader's slot. Answers the class name, `OOME`, or the failure.
    fn define(
        shared: &SharedVm,
        thread: &mut JvmThread,
        body: Body,
        loader_slot: usize,
        name: &str,
        spent: bool,
    ) -> String {
        let cb = body.find(shared);
        let bytes = class_bytes(name, "java/lang/Object", 0x0021);
        let base = thread.native_pin_roots.len();
        let (name_obj, array) = {
            let mut ctx = NativeContextImpl {
                shared,
                thread: &mut *thread,
            };
            let name_obj = ctx.create_string(&name.replace('/', "."));
            let array = ctx.new_array(ArrayElementType::Byte, bytes.len());
            assert!(ctx.write_byte_array_from(array, 0, &bytes));
            (name_obj, array)
        };
        thread.native_pin_roots.push(name_obj);
        thread.native_pin_roots.push(array);
        let loader = thread.native_pin_roots[loader_slot];
        let len = Value::Int(bytes.len() as i32);
        let args = match body {
            Body::UnsafeDefensive => vec![
                Value::Object(None),
                Value::Object(Some(name_obj)),
                Value::Object(Some(array)),
                Value::Int(0),
                len,
                Value::Object(Some(loader)),
                Value::Object(None),
            ],
        };
        forced_try_refuse_all(spent);
        let out = safe_native_call(shared, thread, cb, &args);
        forced_try_refuse_all(false);
        thread.native_pin_roots.truncate(base);
        let ctx = NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        match out {
            Ok(Some(Value::Object(Some(mirror)))) => ctx
                .class_id_from_mirror(mirror)
                .and_then(|c| ctx.class_name_of_id(c))
                .unwrap_or_else(|| "<no class>".to_string()),
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::OutOfMemoryError { .. },
            ))) => "OOME".to_string(),
            other => format!("{other:?}"),
        }
    }

    /// `(spent answer, retry answer)` for `body`; `kill` =
    /// `CRATONVM_ZGC_DEFINE_MIRROR_RESERVED_EVERY_DEFINER=0` from the spent
    /// define on.
    fn row(body: Body, tag: &str, kill: bool) -> (String, String) {
        let shared = SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Zgc,
            max_heap_size: 64 << 20,
            ..VmConfig::default()
        });
        let mut thread = JvmThread::new(ThreadId(3921), "zs9nd8-compat-definer");
        let loader = {
            let mut ctx = NativeContextImpl {
                shared: &shared,
                thread: &mut thread,
            };
            let ldr = ctx
                .define_class_from_bytes(
                    "zs9nd8/CompatLdr",
                    &class_bytes("zs9nd8/CompatLdr", "java/lang/ClassLoader", 0x0021),
                )
                .expect("a ClassLoader subclass defines in the unit VM");
            let fields = ctx.class_num_total_fields(ldr);
            ctx.alloc_object(ldr, fields)
        };
        thread.native_pin_roots.push(loader);
        let slot = thread.native_pin_roots.len() - 1;
        let clean = format!("zs9nd8/{tag}C");
        assert_eq!(
            define(&shared, &mut thread, body, slot, &clean, false),
            clean,
            "{body:?}: a clean define serves"
        );
        let _off = kill.then(|| {
            cratonvm_types::flags::override_thread(
                cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                    "CRATONVM_ZGC_DEFINE_MIRROR_RESERVED_EVERY_DEFINER",
                    Some("0"),
                )]),
            )
        });
        let name = format!("zs9nd8/{tag}R");
        let spent = define(&shared, &mut thread, body, slot, &name, true);
        let retry = define(&shared, &mut thread, body, slot, &name, false);
        eprintln!("zs9nd8 {body:?} kill={kill}: spent={spent} retry={retry}");
        assert_eq!(
            thread.native_pin_roots.len(),
            slot + 1,
            "{body:?}: no pin left"
        );
        (spent, retry)
    }

    #[test]
    fn zs9nd8_unsafe_define_class_defensive_fails_before_it_publishes() {
        let (spent, retry) = row(Body::UnsafeDefensive, "U0", false);
        assert_eq!(spent, "OOME", "a spent ladder is the define's OOME");
        assert_eq!(
            retry, "zs9nd8/U0R",
            "nothing was defined: the retry defines it"
        );
    }

    /// Kill control (`..._RESERVED_EVERY_DEFINER=0`): no reservation, so the
    /// spent ladder is met only by the mint after the define, whose
    /// one-attempt door still serves on this roomy heap: the "spent" define
    /// publishes its class (it is no `OutOfMemoryError`).
    #[test]
    fn zs9nd8_the_every_definer_kill_publishes_under_a_spent_ladder() {
        let (spent, _retry) = row(Body::UnsafeDefensive, "UK0", true);
        assert_eq!(
            spent, "zs9nd8/UK0R",
            "kill arm: defined before the (spent) mint"
        );
    }
}
