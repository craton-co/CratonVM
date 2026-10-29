// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The module functions of the C table (interpreter round i2 wave 63, lane
//! L1; the i51-L1 page's "Next step (from wave 61)", item 5): 3
//! `GetAllModules`, 40 `GetNamedModule`, 94-98 `AddModuleReads` /
//! `Exports` / `Opens` / `Uses` / `Provides` and 99 `IsModifiableModule`.
//!
//! As HotSpot's (`JvmtiExport::add_module_*`), the five additions call
//! `jdk.internal.module.Modules`' Java methods, whose bodies reach the VM
//! through `Module`'s natives; an `IllegalArgumentException` they throw is
//! `JVMTI_ERROR_ILLEGAL_ARGUMENT` (MEASURED: a package the module does not
//! hold, a package named with slashes). Every Java call goes through the
//! JNI table on the calling (agent) thread, so every object is a JNI handle
//! across it.

use super::*;

/// `JVMTI_ERROR_INVALID_MODULE`.
const ERR_INVALID_MODULE: JInt = 26;

/// The JNI functions this module calls, by their `jni.h` index.
struct Jni {
    env: JNIEnv,
}

type JMethod = u64;

impl Jni {
    /// The calling thread's `JNIEnv*` in `shared`'s JNI context, restored by
    /// [`Self::done`].
    fn enter(shared: &SharedVm) -> (Self, Option<Arc<SharedVm>>) {
        let prev = jni::replace_jni_context(shared);
        (
            Self {
                env: jni::get_jni_env(),
            },
            prev,
        )
    }

    fn done(prev: Option<Arc<SharedVm>>) {
        jni::restore_jni_context(prev);
    }

    /// Function `n` of the table.
    fn slot(&self, n: usize) -> usize {
        // SAFETY: `env` is a `JNIEnv*`, whose table has `JNI_FUNCTION_COUNT`
        // entries.
        unsafe { *(*self.env).add(n) }
    }

    fn find_class(&self, name: &std::ffi::CStr) -> JObject {
        type F = unsafe extern "C" fn(JNIEnv, *const std::ffi::c_char) -> JObject;
        // SAFETY: slot 6 is `FindClass`.
        unsafe { std::mem::transmute::<usize, F>(self.slot(6))(self.env, name.as_ptr()) }
    }

    fn method(
        &self,
        class: JObject,
        name: &std::ffi::CStr,
        sig: &std::ffi::CStr,
        is_static: bool,
    ) -> JMethod {
        type F = unsafe extern "C" fn(
            JNIEnv,
            JObject,
            *const std::ffi::c_char,
            *const std::ffi::c_char,
        ) -> JMethod;
        if class == 0 {
            return 0;
        }
        // SAFETY: slots 33 / 113 are `GetMethodID` / `GetStaticMethodID`.
        unsafe {
            std::mem::transmute::<usize, F>(self.slot(if is_static { 113 } else { 33 }))(
                self.env,
                class,
                name.as_ptr(),
                sig.as_ptr(),
            )
        }
    }

    fn call_static_object(&self, class: JObject, m: JMethod, args: &[u64]) -> JObject {
        type F = unsafe extern "C" fn(JNIEnv, JObject, JMethod, *const u64) -> JObject;
        if m == 0 {
            return 0;
        }
        // SAFETY: slot 116 is `CallStaticObjectMethodA`.
        unsafe {
            std::mem::transmute::<usize, F>(self.slot(116))(self.env, class, m, args.as_ptr())
        }
    }

    fn call_static_void(&self, class: JObject, m: JMethod, args: &[u64]) {
        type F = unsafe extern "C" fn(JNIEnv, JObject, JMethod, *const u64);
        if m != 0 {
            // SAFETY: slot 143 is `CallStaticVoidMethodA`.
            unsafe {
                std::mem::transmute::<usize, F>(self.slot(143))(self.env, class, m, args.as_ptr())
            }
        }
    }

    fn call_object(&self, obj: JObject, m: JMethod, args: &[u64]) -> JObject {
        type F = unsafe extern "C" fn(JNIEnv, JObject, JMethod, *const u64) -> JObject;
        if m == 0 || obj == 0 {
            return 0;
        }
        // SAFETY: slot 36 is `CallObjectMethodA`.
        unsafe { std::mem::transmute::<usize, F>(self.slot(36))(self.env, obj, m, args.as_ptr()) }
    }

    fn call_bool(&self, obj: JObject, m: JMethod, args: &[u64]) -> bool {
        type F = unsafe extern "C" fn(JNIEnv, JObject, JMethod, *const u64) -> u8;
        if m == 0 || obj == 0 {
            return false;
        }
        // SAFETY: slot 39 is `CallBooleanMethodA`.
        unsafe {
            std::mem::transmute::<usize, F>(self.slot(39))(self.env, obj, m, args.as_ptr()) != 0
        }
    }

    fn is_instance_of(&self, obj: JObject, class: JObject) -> bool {
        type F = unsafe extern "C" fn(JNIEnv, JObject, JObject) -> u8;
        if obj == 0 || class == 0 {
            return false;
        }
        // SAFETY: slot 32 is `IsInstanceOf`.
        unsafe { std::mem::transmute::<usize, F>(self.slot(32))(self.env, obj, class) != 0 }
    }

    fn same(&self, a: JObject, b: JObject) -> bool {
        type F = unsafe extern "C" fn(JNIEnv, JObject, JObject) -> u8;
        // SAFETY: slot 24 is `IsSameObject`.
        unsafe { std::mem::transmute::<usize, F>(self.slot(24))(self.env, a, b) != 0 }
    }

    fn new_string(&self, text: &str) -> JObject {
        type F = unsafe extern "C" fn(JNIEnv, *const std::ffi::c_char) -> JObject;
        let Ok(text) = std::ffi::CString::new(text) else {
            return 0;
        };
        // SAFETY: slot 167 is `NewStringUTF`.
        unsafe { std::mem::transmute::<usize, F>(self.slot(167))(self.env, text.as_ptr()) }
    }

    fn array(&self, array: JObject) -> Vec<JObject> {
        type Len = unsafe extern "C" fn(JNIEnv, JObject) -> JInt;
        type At = unsafe extern "C" fn(JNIEnv, JObject, JInt) -> JObject;
        if array == 0 {
            return Vec::new();
        }
        // SAFETY: slots 171 / 173 are `GetArrayLength` / `GetObjectArrayElement`.
        unsafe {
            let n = std::mem::transmute::<usize, Len>(self.slot(171))(self.env, array);
            let at = std::mem::transmute::<usize, At>(self.slot(173));
            (0..n).map(|i| at(self.env, array, i)).collect()
        }
    }

    /// The pending exception's class name, cleared; `None` without one.
    fn take_exception(&self) -> Option<String> {
        type Occurred = unsafe extern "C" fn(JNIEnv) -> JObject;
        type Clear = unsafe extern "C" fn(JNIEnv);
        // SAFETY: slots 15 / 17 are `ExceptionOccurred` / `ExceptionClear`.
        let thrown = unsafe { std::mem::transmute::<usize, Occurred>(self.slot(15))(self.env) };
        if thrown == 0 {
            return None;
        }
        // SAFETY: as above.
        unsafe { std::mem::transmute::<usize, Clear>(self.slot(17))(self.env) };
        let iae = self.find_class(c"java/lang/IllegalArgumentException");
        let name = if self.is_instance_of(thrown, iae) {
            "java/lang/IllegalArgumentException"
        } else {
            "other"
        };
        Some(name.to_string())
    }
}

/// Is `obj` (a handle) a `java.lang.Module`?
fn is_module(j: &Jni, obj: JObject) -> bool {
    let module = j.find_class(c"java/lang/Module");
    j.is_instance_of(obj, module)
}

/// The VM a module function acts on (live phase).
fn module_vm(env: *mut JvmtiNativeEnv) -> Result<Arc<SharedVm>, JInt> {
    live_phase_vm(env)
}

/// Every `java.lang.Module` this VM knows, as local handles: the boot
/// layer's modules, the modules it made a `Module` for
/// (`ClassRealm::module_mirrors`, which the module system's definitions and
/// `Class.getModule()` fill: a non-boot layer's included), and the unnamed
/// modules of the bootstrap, platform and system loaders, each once.
fn all_modules(shared: &SharedVm, j: &Jni) -> Vec<JObject> {
    let mut out: Vec<JObject> = Vec::new();
    let mut push = |m: JObject, out: &mut Vec<JObject>| {
        if m != 0 && !out.iter().any(|&o| j.same(o, m)) {
            out.push(m);
        }
    };
    let layer_class = j.find_class(c"java/lang/ModuleLayer");
    let boot = j.call_static_object(
        layer_class,
        j.method(layer_class, c"boot", c"()Ljava/lang/ModuleLayer;", true),
        &[],
    );
    let set = j.call_object(
        boot,
        j.method(layer_class, c"modules", c"()Ljava/util/Set;", false),
        &[],
    );
    let set_class = j.find_class(c"java/util/Set");
    let array = j.call_object(
        set,
        j.method(set_class, c"toArray", c"()[Ljava/lang/Object;", false),
        &[],
    );
    for m in j.array(array) {
        push(m, &mut out);
    }
    let mirrors: Vec<ObjectRef> = shared
        .classes
        .module_mirrors
        .read()
        .values()
        .copied()
        .collect();
    {
        let _fx = jni::ForeignJniEntry::enter();
        for m in mirrors {
            push(jni::new_local_handle(m), &mut out);
        }
    }
    let boot_loader = j.find_class(c"jdk/internal/loader/BootLoader");
    push(
        j.call_static_object(
            boot_loader,
            j.method(
                boot_loader,
                c"getUnnamedModule",
                c"()Ljava/lang/Module;",
                true,
            ),
            &[],
        ),
        &mut out,
    );
    let loader_class = j.find_class(c"java/lang/ClassLoader");
    let unnamed = j.method(
        loader_class,
        c"getUnnamedModule",
        c"()Ljava/lang/Module;",
        false,
    );
    for getter in [c"getPlatformClassLoader", c"getSystemClassLoader"] {
        let loader = j.call_static_object(
            loader_class,
            j.method(loader_class, getter, c"()Ljava/lang/ClassLoader;", true),
            &[],
        );
        push(j.call_object(loader, unnamed, &[]), &mut out);
    }
    let _ = j.take_exception();
    out
}

/// Slot 3: `GetAllModules(env, module_count_ptr, modules_ptr)` (live
/// phase): [`all_modules`] (MEASURED: `java.base`, `java.logging` and the
/// application loader's unnamed module among more than twenty).
pub(super) extern "C" fn get_all_modules(
    env: *mut JvmtiNativeEnv,
    count_ptr: *mut JInt,
    modules_ptr: *mut *mut JObject,
) -> JInt {
    let shared = match module_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    if count_ptr.is_null() || modules_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let (j, prev) = Jni::enter(&shared);
    let modules = all_modules(&shared, &j);
    Jni::done(prev);
    write_id_array(count_ptr, modules_ptr, &modules)
}

/// Slot 40: `GetNamedModule(env, class_loader, package_name, module_ptr)`
/// (live phase): the named module of `class_loader` (NULL: the bootstrap
/// loader) holding package `package_name` (`java/lang` form), NULL when
/// none does (MEASURED). A NULL package or out-parameter is `NULL_POINTER`,
/// a loader that is not a `ClassLoader` `ILLEGAL_ARGUMENT`.
pub(super) extern "C" fn get_named_module(
    env: *mut JvmtiNativeEnv,
    class_loader: JObject,
    package_name: *const std::ffi::c_char,
    module_ptr: *mut JObject,
) -> JInt {
    let shared = match module_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    if package_name.is_null() || module_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's NUL-terminated string.
    let package = unsafe { agent_c_string(package_name) }.replace('/', ".");
    let (j, prev) = Jni::enter(&shared);
    let loader_class = j.find_class(c"java/lang/ClassLoader");
    if class_loader != 0 && !j.is_instance_of(class_loader, loader_class) {
        Jni::done(prev);
        return ERR_ILLEGAL_ARGUMENT;
    }
    let module_class = j.find_class(c"java/lang/Module");
    let is_named = j.method(module_class, c"isNamed", c"()Z", false);
    let loader_of = j.method(
        module_class,
        c"getClassLoader",
        c"()Ljava/lang/ClassLoader;",
        false,
    );
    let packages = j.method(module_class, c"getPackages", c"()Ljava/util/Set;", false);
    let set_class = j.find_class(c"java/util/Set");
    let contains = j.method(set_class, c"contains", c"(Ljava/lang/Object;)Z", false);
    let pn = j.new_string(&package);
    let found = all_modules(&shared, &j).into_iter().find(|&m| {
        j.call_bool(m, is_named, &[])
            && j.same(j.call_object(m, loader_of, &[]), class_loader)
            && j.call_bool(j.call_object(m, packages, &[]), contains, &[pn as u64])
    });
    let _ = j.take_exception();
    Jni::done(prev);
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *module_ptr = found.unwrap_or(0) };
    ERR_NONE
}

/// The `Modules` addition `name` with the handles `args`: HotSpot's checks
/// (`modules` handles must be `Module`s: `INVALID_MODULE`; `classes` handles
/// must be classes: `INVALID_CLASS`), then the Java call; an
/// `IllegalArgumentException` it throws is `ILLEGAL_ARGUMENT`, any other
/// `INTERNAL`.
fn add_through_modules(
    env: *mut JvmtiNativeEnv,
    name: &std::ffi::CStr,
    sig: &std::ffi::CStr,
    modules: &[JObject],
    classes: &[JObject],
    package: Option<*const std::ffi::c_char>,
) -> JInt {
    let shared = match module_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    if modules.iter().chain(classes).any(|&h| h == 0) || package.is_some_and(|p| p.is_null()) {
        return ERR_NULL_POINTER;
    }
    let (j, prev) = Jni::enter(&shared);
    let answer = (|| {
        if modules.iter().any(|&m| !is_module(&j, m)) {
            return ERR_INVALID_MODULE;
        }
        let class_class = j.find_class(c"java/lang/Class");
        if classes.iter().any(|&c| !j.is_instance_of(c, class_class)) {
            return ERR_INVALID_CLASS;
        }
        let mut args: Vec<u64> = vec![modules[0] as u64]; // Cast: a `jvalue` holding the handle
        if let Some(package) = package {
            // SAFETY: the agent's NUL-terminated string, checked non-null.
            let text = unsafe { agent_c_string(package) };
            args.push(j.new_string(&text) as u64);
        }
        args.extend(modules[1..].iter().chain(classes).map(|&h| h as u64));
        let class = j.find_class(c"jdk/internal/module/Modules");
        let m = j.method(class, name, sig, true);
        if m == 0 {
            let _ = j.take_exception();
            return ERR_INTERNAL;
        }
        j.call_static_void(class, m, &args);
        match j.take_exception().as_deref() {
            None => ERR_NONE,
            Some("java/lang/IllegalArgumentException") => ERR_ILLEGAL_ARGUMENT,
            Some(_) => ERR_INTERNAL,
        }
    })();
    Jni::done(prev);
    answer
}

/// Slot 94: `AddModuleReads(env, module, to_module)`.
pub(super) extern "C" fn add_module_reads(
    env: *mut JvmtiNativeEnv,
    module: JObject,
    to_module: JObject,
) -> JInt {
    add_through_modules(
        env,
        c"addReads",
        c"(Ljava/lang/Module;Ljava/lang/Module;)V",
        &[module, to_module],
        &[],
        None,
    )
}

/// Slot 95: `AddModuleExports(env, module, pkg_name, to_module)`: the
/// package in `java.lang` form (MEASURED: `jdk/internal/misc` is 103).
pub(super) extern "C" fn add_module_exports(
    env: *mut JvmtiNativeEnv,
    module: JObject,
    pkg_name: *const std::ffi::c_char,
    to_module: JObject,
) -> JInt {
    add_through_modules(
        env,
        c"addExports",
        c"(Ljava/lang/Module;Ljava/lang/String;Ljava/lang/Module;)V",
        &[module, to_module],
        &[],
        Some(pkg_name),
    )
}

/// Slot 96: `AddModuleOpens(env, module, pkg_name, to_module)`.
pub(super) extern "C" fn add_module_opens(
    env: *mut JvmtiNativeEnv,
    module: JObject,
    pkg_name: *const std::ffi::c_char,
    to_module: JObject,
) -> JInt {
    add_through_modules(
        env,
        c"addOpens",
        c"(Ljava/lang/Module;Ljava/lang/String;Ljava/lang/Module;)V",
        &[module, to_module],
        &[],
        Some(pkg_name),
    )
}

/// Slot 97: `AddModuleUses(env, module, service)`.
pub(super) extern "C" fn add_module_uses(
    env: *mut JvmtiNativeEnv,
    module: JObject,
    service: JObject,
) -> JInt {
    add_through_modules(
        env,
        c"addUses",
        c"(Ljava/lang/Module;Ljava/lang/Class;)V",
        &[module],
        &[service],
        None,
    )
}

/// Slot 98: `AddModuleProvides(env, module, service, impl_class)`.
pub(super) extern "C" fn add_module_provides(
    env: *mut JvmtiNativeEnv,
    module: JObject,
    service: JObject,
    impl_class: JObject,
) -> JInt {
    add_through_modules(
        env,
        c"addProvides",
        c"(Ljava/lang/Module;Ljava/lang/Class;Ljava/lang/Class;)V",
        &[module],
        &[service, impl_class],
        None,
    )
}

/// Slot 99: `IsModifiableModule(env, module, is_modifiable_module_ptr)`:
/// true for every module (MEASURED: `java.base` and an unnamed module),
/// `INVALID_MODULE` for an object that is none.
pub(super) extern "C" fn is_modifiable_module(
    env: *mut JvmtiNativeEnv,
    module: JObject,
    out: *mut u8,
) -> JInt {
    let shared = match module_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    if module == 0 || out.is_null() {
        return ERR_NULL_POINTER;
    }
    let (j, prev) = Jni::enter(&shared);
    let valid = is_module(&j, module);
    Jni::done(prev);
    if !valid {
        return ERR_INVALID_MODULE;
    }
    write_jboolean(out, Ok(true))
}
