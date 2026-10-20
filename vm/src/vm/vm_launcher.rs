// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The launcher's main-method check and `main` invocation, as JDK 25's
//! `sun.launcher.LauncherHelper.validateMainMethod` and `java.c`'s
//! `JavaMain` do them.
//!
//! HotSpot's launcher asks `LauncherHelper.checkAndLoadMain` for the main
//! class: `MethodFinder.findMainMethod` (JEP 512, final in JDK 25) looks for
//! `main(String[])` -- public first, then of any access -- and, when that is
//! missing or not a main method (not `void`, or private), for `main()`; a
//! failure prints a `java.launcher.cls.error*` text from `launcher.properties`
//! and exits 1. `java.c` then reads `isStaticMain` / `noArgMain` and calls
//! `main` statically or on an instance made by the no-argument constructor,
//! with or without the arguments; the JNI method lookup initialises the MAIN
//! class (not only the class declaring an inherited `main`).
//!
//! The VM's launcher used to invoke `main([Ljava/lang/String;)V` by name, so
//! an instance or no-argument `main` did not start and every check failure
//! surfaced as a `NoSuchMethodError` (interpreter round i2 wave 59, lane L5;
//! `docs/internal/fixed-bugs/interpreter-L5-the-launcher-rejects-an-instance-main-and-misreports-main-class-errors-FIXED-20261023.md`,
//! probes `tools/probes/interp/L5/L5W59Launcher*.java`). The search here
//! reads the class metadata (it runs with no JDK too); with
//! `CRATONVM_LAUNCHER_HELPER` the JDK's own `LauncherHelper` answers instead
//! ([`Vm::launcher_check_main_via_helper`]).

use std::sync::Arc;

use crate::classloading::{ClassId, ClassManager};
use crate::error::{MethodCallFailed, MethodCallResult, VmError};
use crate::types::{ObjectRef, Value};

use super::Vm;

const ACC_PUBLIC: u16 = 0x0001;
const ACC_PRIVATE: u16 = 0x0002;
const ACC_STATIC: u16 = 0x0008;
const ACC_INTERFACE: u16 = 0x0200;
const ACC_ABSTRACT: u16 = 0x0400;

/// `java.launcher.cls.error2`'s `{1}`, `LauncherHelper.JAVAFX_APPLICATION_CLASS_NAME`.
const JAVAFX_APPLICATION_CLASS_NAME: &str = "javafx.application.Application";

/// The `main` the launcher starts: `LauncherHelper`'s `isStaticMain` /
/// `noArgMain` and the main class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LauncherMain {
    /// The main class (the class named on the command line or in the
    /// manifest), whose `<clinit>` the launch runs.
    pub class_id: ClassId,
    /// `main` is static (`LauncherHelper.isStaticMain`).
    pub is_static: bool,
    /// `main` takes no parameters (`LauncherHelper.noArgMain`).
    pub no_arg: bool,
}

impl LauncherMain {
    /// The descriptor `java.c` looks `main` up with.
    pub fn descriptor(&self) -> &'static str {
        if self.no_arg {
            "()V"
        } else {
            "([Ljava/lang/String;)V"
        }
    }
}

/// One `main` candidate of [`find_main_method`]: a declared method of
/// `declaring`.
#[derive(Debug, Clone)]
struct MainCandidate {
    declaring: ClassId,
    flags: u16,
    descriptor: Arc<str>,
}

impl MainCandidate {
    fn return_type(&self) -> &str {
        self.descriptor
            .rfind(')')
            .map_or("", |close| &self.descriptor[close + 1..])
    }

    /// `MethodFinder.isValidMainMethod`: returns `void` and is not private.
    fn is_valid_main(&self) -> bool {
        self.return_type() == "V" && self.flags & ACC_PRIVATE == 0
    }
}

fn is_interface(cm: &ClassManager, class_id: ClassId) -> bool {
    cm.get_class(class_id)
        .is_some_and(|c| c.access_flags.bits() & ACC_INTERFACE != 0)
}

/// `a.isAssignableFrom(b)` for two loaded classes.
fn class_assignable_from(cm: &ClassManager, a: ClassId, b: ClassId) -> bool {
    a == b || cm.is_subclass_of(b, a)
}

/// `Class.getMethodsRecursive(name = "main", parameterTypes, includeStatic,
/// publicOnly)` over the loaded metadata: the class's own matching declared
/// methods if it has any, else its superclass's merged with its direct
/// interfaces' (`PublicMethods.MethodList.merge`, statics excluded there).
/// `params` is the descriptor's parameter part (`[Ljava/lang/String;` or
/// empty).
fn methods_recursive(
    cm: &ClassManager,
    class_id: ClassId,
    params: &str,
    include_static: bool,
    public_only: bool,
    depth: usize,
) -> Vec<MainCandidate> {
    let Some(class) = cm.get_class(class_id) else {
        return Vec::new();
    };
    if depth > 64 {
        return Vec::new();
    }
    let declared: Vec<MainCandidate> = class
        .methods
        .iter()
        .filter(|m| {
            let flags = m.access_flags.bits();
            &*m.name == "main"
                && m.descriptor.len() > params.len() + 1
                && m.descriptor.starts_with('(')
                && m.descriptor[1..].starts_with(params)
                && m.descriptor[1 + params.len()..].starts_with(')')
                && (include_static || flags & ACC_STATIC == 0)
                && (!public_only || flags & ACC_PUBLIC != 0)
        })
        .map(|m| MainCandidate {
            declaring: class_id,
            flags: m.access_flags.bits(),
            descriptor: m.descriptor.clone(),
        })
        .collect();
    if !declared.is_empty() {
        return declared;
    }
    let mut res = class.superclass.map_or_else(Vec::new, |sc| {
        methods_recursive(cm, sc, params, include_static, public_only, depth + 1)
    });
    for &intf in &class.interfaces {
        for method in methods_recursive(cm, intf, params, false, public_only, depth + 1) {
            merge_candidate(cm, &mut res, method);
        }
    }
    res
}

/// `PublicMethods.MethodList.merge(head, method)`: keep only the most
/// specific declaration per return type.
fn merge_candidate(cm: &ClassManager, list: &mut Vec<MainCandidate>, method: MainCandidate) {
    let dclass = method.declaring;
    let d_is_interface = is_interface(cm, dclass);
    let mut i = 0;
    while i < list.len() {
        let existing = &list[i];
        if existing.return_type() != method.return_type() {
            i += 1;
            continue;
        }
        let xdclass = existing.declaring;
        let x_is_interface = is_interface(cm, xdclass);
        if d_is_interface == x_is_interface {
            if class_assignable_from(cm, dclass, xdclass) {
                // The existing method is the same or overrides the new one.
                return;
            }
            if class_assignable_from(cm, xdclass, dclass) {
                // The new method overrides the existing one.
                list.remove(i);
                continue;
            }
            i += 1;
        } else if d_is_interface {
            // A class's method beats an interface's.
            return;
        } else {
            list.remove(i);
        }
    }
    list.push(method);
}

/// `Class.findMethod(publicOnly, "main", params)`: the list's most specific
/// method. The return types of `main` candidates are `void` or a primitive
/// in practice, where `getMostSpecific` keeps the first; a reference return
/// type is never a main method, so its specificity does not change the
/// launcher's answer.
fn find_method(
    cm: &ClassManager,
    class_id: ClassId,
    public_only: bool,
    params: &str,
) -> Option<MainCandidate> {
    methods_recursive(cm, class_id, params, true, public_only, 0)
        .into_iter()
        .next()
}

/// `MethodFinder.findMainMethod(cls)`.
fn find_main_method(cm: &ClassManager, class_id: ClassId) -> Option<MainCandidate> {
    const STRING_ARRAY: &str = "[Ljava/lang/String;";
    let mut main = find_method(cm, class_id, true, STRING_ARRAY)
        .or_else(|| find_method(cm, class_id, false, STRING_ARRAY));
    if !main.as_ref().is_some_and(MainCandidate::is_valid_main) {
        main = find_method(cm, class_id, false, "");
    }
    main.filter(MainCandidate::is_valid_main)
}

/// The main class's `Class.getModifiers()` and whether it is a non-static
/// member class (`isMemberClass() && !Modifier.isStatic(getModifiers())`):
/// a member class's modifiers are its `InnerClasses` entry's.
fn main_class_shape(cm: &ClassManager, class_id: ClassId) -> (u16, bool) {
    let Some(class) = cm.get_class(class_id) else {
        return (0, false);
    };
    let own = class
        .inner_classes
        .iter()
        .find(|e| *e.inner_class == *class.name && !e.outer_class.is_empty());
    match own {
        Some(entry) => (entry.access_flags, entry.access_flags & ACC_STATIC == 0),
        None => (class.access_flags.bits(), false),
    }
}

fn binary_name(cm: &ClassManager, class_id: ClassId) -> String {
    cm.get_class(class_id)
        .map(|c| c.name.replace('/', "."))
        .unwrap_or_else(|| "<unknown class>".to_string())
}

/// `LauncherHelper.validateMainMethod(mainClass)` over the loaded metadata:
/// the `main` to start, or the `java.launcher.cls.error*` text the launcher
/// prints (to stderr, then exit 1). The JavaFX check is not made (no JavaFX
/// class can be on this VM's boot path).
pub fn validate_main_method(cm: &ClassManager, class_id: ClassId) -> Result<LauncherMain, String> {
    let class_name = binary_name(cm, class_id);
    let Some(main) = find_main_method(cm, class_id) else {
        return Err(format!(
            "Error: Main method not found in class {class_name}, please define the main method as:\n   \
             public static void main(String[] args)\n\
             or a JavaFX application class must extend {JAVAFX_APPLICATION_CLASS_NAME}"
        ));
    };
    let is_static = main.flags & ACC_STATIC != 0;
    let no_arg = main.descriptor.starts_with("()");
    if !is_static {
        // The messages name the class DECLARING `main` (an inherited one names
        // its superclass or interface), as `validateMainMethod` does.
        let declaring = binary_name(cm, main.declaring);
        let (modifiers, non_static_member) = main_class_shape(cm, class_id);
        if modifiers & ACC_ABSTRACT != 0 {
            return Err(format!(
                "Error: abstract class {declaring} can not be instantiated\n\
                 please use a concrete class"
            ));
        }
        if non_static_member {
            // `launcher.properties` ends the first line with one space.
            return Err(format!(
                "Error: non-static inner class {declaring} constructor can not be invoked \n\
                 make inner class static or move inner class out to separate source file"
            ));
        }
        let ctor_ok = cm.get_class(class_id).is_some_and(|c| {
            c.methods.iter().any(|m| {
                &*m.name == "<init>"
                    && &*m.descriptor == "()V"
                    && m.access_flags.bits() & ACC_PRIVATE == 0
            })
        });
        if !ctor_ok {
            return Err(format!(
                "Error: no non-private zero argument constructor found in class {declaring}\n\
                 remove private from existing constructor or define as:\n   \
                 public {declaring}()"
            ));
        }
    }
    Ok(LauncherMain {
        class_id,
        is_static,
        no_arg,
    })
}

/// `java.launcher.cls.error1` / `error4` for a main class that failed to
/// load, from the throwable `Class.forName(name, false, systemLoader)` threw:
/// `NoClassDefFoundError` and `ClassNotFoundException` are error1, any other
/// `LinkageError` error4 (`LauncherHelper.loadMainClass`).
pub fn main_class_load_failure_text(
    class_name_dotted: &str,
    thrown_class_dotted: &str,
    message: &str,
    is_linkage_error: bool,
) -> String {
    if thrown_class_dotted == "java.lang.ClassNotFoundException"
        || thrown_class_dotted == "java.lang.NoClassDefFoundError"
        || !is_linkage_error
    {
        format!(
            "Error: Could not find or load main class {class_name_dotted}\n\
             Caused by: {thrown_class_dotted}: {message}"
        )
    } else {
        format!(
            "Error: LinkageError occurred while loading main class {class_name_dotted}\n\
             \t{thrown_class_dotted}: {message}"
        )
    }
}

impl Vm {
    /// The loaded main class `class_name` (internal form): the application
    /// loader's, else the by-name answer (a VM with no JDK).
    pub fn launcher_main_class_id(&self, class_name: &str) -> Option<ClassId> {
        let by_loader = self
            .shared
            .classes
            .class_manager
            .read()
            .find_class_by_name_for_loader(class_name, cratonvm_types::ClassLoaderId::Application);
        by_loader.or_else(|| self.shared.load_class_concurrent(class_name).ok())
    }

    /// [`validate_main_method`] for the loaded main class `class_name`.
    pub fn launcher_check_main(&self, class_name: &str) -> Result<LauncherMain, String> {
        let Some(class_id) = self.launcher_main_class_id(class_name) else {
            return Err(format!(
                "Error: Could not find or load main class {}\n\
                 Caused by: java.lang.ClassNotFoundException: {}",
                class_name.replace('/', "."),
                class_name.replace('/', ".")
            ));
        };
        let cm = self.shared.classes.class_manager.read();
        validate_main_method(&cm, class_id)
    }

    /// The text `LauncherHelper.loadMainClass` prints for a main class the
    /// VM's own load of `class_name` (internal form) refused: over a real JDK
    /// the system loader's `Class.forName(name, false, loader)` is asked
    /// again and the throwable it raises is rendered (error1 / error4);
    /// without one, or when that load succeeds after all, `None` (the caller
    /// keeps its own text).
    pub fn launcher_load_failure_text(&mut self, class_name: &str) -> Option<String> {
        use cratonvm_native_api::{NativeHeapAccess as _, NativeInvokeAccess as _};
        if self.shared.config.java_home.is_none() {
            return None;
        }
        let dotted = class_name.replace('/', ".");
        let thrown = {
            let mut ctx = crate::vm::vm_exec::NativeContextImpl {
                shared: &self.shared,
                thread: &mut self.main_thread,
            };
            let loader = match ctx.invoke_static(
                "java/lang/ClassLoader",
                "getSystemClassLoader",
                "()Ljava/lang/ClassLoader;",
                &[],
            ) {
                Ok(Some(Value::Object(Some(loader)))) => loader,
                _ => return None,
            };
            let pin = ctx.pin_native_root(loader);
            let name = ctx.create_string_uninterned(&dotted);
            let loader = ctx.read_native_pin(pin, loader);
            ctx.unpin_native_roots(pin);
            match ctx.invoke_static(
                "java/lang/Class",
                "forName",
                "(Ljava/lang/String;ZLjava/lang/ClassLoader;)Ljava/lang/Class;",
                &[
                    Value::Object(Some(name)),
                    Value::Int(0),
                    Value::Object(Some(loader)),
                ],
            ) {
                Err(MethodCallFailed::ExceptionThrown(exc)) => exc,
                _ => return None,
            }
        };
        let thrown_class = self.shared.mem.heap.class_id_of(thrown);
        let (thrown_name, is_linkage_error) = {
            let cm = self.shared.classes.class_manager.read();
            (
                binary_name(&cm, thrown_class),
                cm.is_subclass_of_by_name(thrown_class, "java/lang/LinkageError"),
            )
        };
        let message = self
            .exception_detail_message(thrown)
            .unwrap_or_else(|| "null".to_string());
        Some(main_class_load_failure_text(
            &dotted,
            &thrown_name,
            &message,
            is_linkage_error,
        ))
    }

    /// Whether the launcher asks the JDK's own `sun.launcher.LauncherHelper`
    /// for the main class (`CRATONVM_LAUNCHER_HELPER`, read once per VM;
    /// over a real JDK only).
    pub fn launcher_runs_launcher_helper(&self) -> bool {
        self.shared.config.launcher_helper && self.shared.config.java_home.is_some()
    }

    /// Whether a `-jar` launch hands the JAR to the helper (`LM_JAR`):
    /// `CRATONVM_LAUNCHER_HELPER_JAR` with the helper itself on.
    pub fn launcher_runs_helper_jar(&self) -> bool {
        self.launcher_runs_launcher_helper() && self.shared.config.launcher_helper_jar
    }

    /// Starts the JDK's common `Cleaner` (its `Common-Cleaner` thread) when
    /// the main class `class_id` came from the application class path, over
    /// a real JDK. HotSpot's application loader reads that class's bytes
    /// through a `FileInputStream` (a JAR's through `ZipFile`), whose
    /// `FileCleanable.register` initializes `CleanerFactory` before `main`
    /// runs; this VM defines the class from Rust, so nothing did, and the
    /// thread started at the program's first file open instead -- listed
    /// after the program's own threads in a thread dump, or (on Linux, where
    /// `jcmd`'s process launch opens nothing through it) not at all
    /// (interpreter round i3 wave 83; page
    /// `i83-orch-compatible-thread-dump-lacks-common-cleaner`;
    /// `tools/probes/interp/L6/L6W81ThreadDumpShape.java`). Best effort: a
    /// JDK without the class leaves the lazy start, as before.
    pub fn launcher_start_common_cleaner(&mut self, class_id: crate::classloading::ClassId) {
        if self.shared.config.java_home.is_none() {
            return;
        }
        let from_class_path = self
            .shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .is_some_and(|c| {
                matches!(
                    c.origin,
                    cratonvm_classloading::ClassOrigin::ApplicationClassPath { .. }
                )
            });
        if !from_class_path {
            return;
        }
        self.launcher_run_cleaner_factory();
    }

    /// [`Self::launcher_start_common_cleaner`] for a `-jar` launch whose
    /// pre-load was skipped (`CRATONVM_LAUNCHER_HELPER_JAR_NO_PRELOAD`), so
    /// there is no main class id yet. HotSpot's `LauncherHelper` opens the jar
    /// (`getMainClassFromJar` -> `new JarFile`) before it starts the
    /// `Launcher-Agent-Class` or loads the main class, and `ZipFile`'s
    /// `CleanableResource` registers with `CleanerFactory.cleaner()` there:
    /// the `Common-Cleaner` thread exists before any agent or program thread.
    /// The helper's own `JarFile` does the same here when the JDK's archive
    /// bodies run (`--jdk-only`, and `--compatible` with the default
    /// `JdkJarFileBodies` row); a `--compatible` VM with that row removed
    /// (`CRATONVM_COMPAT_ALIGN=-jdk-jar-file-bodies`) builds a native-backed
    /// `JarFile` with no `CleanableResource`, and would then start the thread
    /// only at the program's first file open (the `i83-orch` dump order).
    /// Calling it first is the same answer in every arm: the factory is
    /// idempotent, and nothing between this call and the jar open starts a
    /// thread. Interpreter round i6 wave 133, lane L7b
    /// (`i58-L5-proposal-the-launcher-runs-launcherhelper`).
    pub fn launcher_start_common_cleaner_for_jar(&mut self) {
        if self.shared.config.java_home.is_none() {
            return;
        }
        self.launcher_run_cleaner_factory();
    }

    /// `CleanerFactory.cleaner()` on the main thread, best effort (the two
    /// launcher callers above decide whether it should run).
    fn launcher_run_cleaner_factory(&mut self) {
        use cratonvm_native_api::NativeInvokeAccess as _;
        // gci i18/na: not under `safe_native_call_impl`; a refused allocation
        // leaves the lazy start (as any other failure here) instead of the
        // infallible arm's abort.
        let shared: &crate::vm::SharedVm = &self.shared;
        let started =
            crate::runtime::native_oom::catch_alloc_oom_on(&mut self.main_thread, |thread| {
            let mut ctx = crate::vm::vm_exec::NativeContextImpl { shared, thread };
            ctx.invoke_static(
                "jdk/internal/ref/CleanerFactory",
                "cleaner",
                "()Ljava/lang/ref/Cleaner;",
                &[],
            )
        });
        match started {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                tracing::debug!("CleanerFactory.cleaner() before main did not run: {e:?}");
            }
            Err(oom) => {
                tracing::debug!(
                    "CleanerFactory.cleaner() before main did not run: java.lang.OutOfMemoryError: {}",
                    oom.java_message()
                );
            }
        }
    }

    /// `LauncherHelper.checkAndLoadMain(true, LM_CLASS, name)` over the real
    /// JDK, as `java.c`'s `LoadMainClass` calls it, then the helper's
    /// `isStaticMain` / `noArgMain` (`JavaMain` reads them the same way).
    /// The helper loads the class through the system loader (the VM's own
    /// load before it, which offers the bytes to the agents, has defined it
    /// already), validates `main` with `MethodFinder.findMainMethod`, and on
    /// any failure prints its `java.launcher.*` text and calls
    /// `System.exit(1)`, which does not return here.
    ///
    /// `Err(ExceptionThrown)` when the helper threw (the launcher reports it
    /// as `main`'s uncaught exception, as `java.c`'s
    /// `CHECK_EXCEPTION_NULL_LEAVE` does); `Err(InternalError)` when the VM
    /// could not run it (the caller falls back to [`Self::launcher_check_main`]).
    ///
    /// A `-jar` launch (`jar`: the path as given after `-jar`) passes the JAR
    /// with `LM_JAR` under `CRATONVM_LAUNCHER_HELPER_JAR`
    /// ([`Self::launcher_runs_helper_jar`]), as `java.c` does: the helper's
    /// `getMainClassFromJar` reads `Main-Class` and acts on
    /// `Launcher-Agent-Class`, `Add-Opens` / `Add-Exports` and
    /// `Enable-Native-Access`, none of which the VM's own `-jar` path reads
    /// (interpreter round i2 wave 60, lane L5). Without the switch the
    /// manifest's class goes with `LM_CLASS`. Interpreter round i2 wave 59,
    /// lane L5
    /// (`docs/internal/fixed-bugs/interpreter-i58-L5-proposal-the-launcher-runs-launcherhelper-KEPT-20261224.md`).
    pub fn launcher_check_main_via_helper(
        &mut self,
        class_name: &str,
        jar: Option<&str>,
    ) -> Result<LauncherMain, MethodCallFailed> {
        use cratonvm_native_api::{
            NativeClassAccess as _, NativeHeapAccess as _, NativeInvokeAccess as _,
            NativeSystemAccess as _,
        };
        const HELPER: &str = "sun/launcher/LauncherHelper";
        const LM_CLASS: i32 = 1;
        const LM_JAR: i32 = 2;
        const LM_MODULE: i32 = 3;
        let started = std::time::Instant::now();
        let dotted = class_name.replace('/', ".");
        // `-m <module>[/<class>]` (interpreter round i6 wave 117, lane L5;
        // `i58-L5-proposal-the-launcher-runs-launcherhelper`): the launcher
        // names the main module as `-Djdk.module.main=<m>`, as `java.c` does,
        // and `java.c` then calls the helper with `LM_MODULE`, whose
        // `loadModuleMainClass` answers `Class.forName(module, name)` and prints
        // the `java.launcher.module.error2` / `error3` texts (and sets
        // `jdk.module.main.class`). The property set by hand on a class-path
        // launch is not a module launch: a main class loaded into ANOTHER module
        // (or the unnamed one) keeps `LM_CLASS`.
        let module_launch_on = self.shared.config.launcher_helper_module
            && (self.shared.config.is_jdk_only()
                || self.shared.config.launcher_helper_module_compatible);
        let module_what = if jar.is_none() && module_launch_on {
            let main_module = {
                use cratonvm_native_api::NativeSystemAccess as _;
                let ctx = crate::vm::vm_exec::NativeContextImpl {
                    shared: &self.shared,
                    thread: &mut self.main_thread,
                };
                ctx.get_system_property("jdk.module.main")
            };
            main_module.filter(|m| !m.is_empty()).and_then(|m| {
                let cm = self.shared.classes.class_manager.read();
                let class_module = cm
                    .get_loaded_class_id(class_name)
                    .and_then(|id| cm.get_class(id))
                    .map(|c| c.module_name.clone());
                match class_module {
                    // Not loaded (the helper's own `Class.forName(module, ..)`
                    // answers null: `module.error2`), or loaded into this module.
                    None => Some(format!("{m}/{dotted}")),
                    Some(Some(own)) if own == m => Some(format!("{m}/{dotted}")),
                    Some(_) => None,
                }
            })
        } else {
            None
        };
        let (mode, what) = match (jar, module_what.as_deref()) {
            (Some(jar), _) if self.launcher_runs_helper_jar() => (LM_JAR, jar),
            (None, Some(what)) => (LM_MODULE, what),
            _ => (LM_CLASS, dotted.as_str()),
        };
        // gci i18/na: not under `safe_native_call_impl`; a refused allocation
        // (the name `String`, Rust-side work on the way into the helper) is
        // an `InternalError` of this call -- the caller falls back to
        // `launcher_check_main` -- instead of the infallible arm's abort.
        let shared: &crate::vm::SharedVm = &self.shared;
        let checked =
            crate::runtime::native_oom::catch_alloc_oom_on(&mut self.main_thread, |thread| {
            let mut ctx = crate::vm::vm_exec::NativeContextImpl { shared, thread };
            let what = ctx.create_string_uninterned(what);
            ctx.invoke_static(
                HELPER,
                "checkAndLoadMain",
                "(ZILjava/lang/String;)Ljava/lang/Class;",
                &[Value::Int(1), Value::Int(mode), Value::Object(Some(what))],
            )
        })
        .map_err(|oom| MethodCallFailed::from(oom.into_runtime_error()))?;
        let mirror = match checked? {
            Some(Value::Object(Some(mirror))) => mirror,
            _ => {
                return Err(MethodCallFailed::InternalError(VmError::Internal {
                    message: "LauncherHelper.checkAndLoadMain returned no class".to_string(),
                }))
            }
        };
        let class_id = super::class_id_from_mirror(&self.shared, mirror).ok_or_else(|| {
            MethodCallFailed::InternalError(VmError::Internal {
                message: "LauncherHelper.checkAndLoadMain returned an unknown mirror".to_string(),
            })
        })?;
        let mut ctx = crate::vm::vm_exec::NativeContextImpl {
            shared: &self.shared,
            thread: &mut self.main_thread,
        };
        let helper_id = ctx.ensure_class_initialized(HELPER)?;
        let mut flag = |name: &str| -> Result<bool, MethodCallFailed> {
            let index = ctx
                .static_field_index_by_name(helper_id, name)
                .ok_or_else(|| {
                    MethodCallFailed::InternalError(VmError::Internal {
                        message: format!("LauncherHelper has no static field {name}"),
                    })
                })?;
            Ok(matches!(ctx.get_static_field(helper_id, index), Value::Int(v) if v != 0))
        };
        let is_static = flag("isStaticMain")?;
        let no_arg = flag("noArgMain")?;
        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_LAUNCHER_HELPER") {
            eprintln!(
                "[launcher-helper] {dotted}: checkAndLoadMain ran (mode={} isStaticMain={is_static} \
                 noArgMain={no_arg}) in {} us",
                match mode {
                    LM_JAR => "LM_JAR",
                    LM_MODULE => "LM_MODULE",
                    _ => "LM_CLASS",
                },
                started.elapsed().as_micros()
            );
        }
        Ok(LauncherMain {
            class_id,
            is_static,
            no_arg,
        })
    }

    /// `java.c`'s steps before the `main` call: the JNI lookup initialises
    /// the main class, and an instance `main` gets its receiver from the
    /// no-argument constructor. The receiver is returned PINNED: the index of
    /// its `native_pin_roots` slot, which [`Self::launcher_invoke_main`]
    /// reads and releases. The launcher roots the argument array before this
    /// initialization/constructor step, matching `CreateApplicationArgs`.
    pub fn launcher_prepare_main(
        &mut self,
        main: &LauncherMain,
    ) -> Result<Option<usize>, MethodCallFailed> {
        self.ensure_class_initialized(main.class_id)?;
        if main.is_static {
            return Ok(None);
        }
        let num_fields = self
            .shared
            .classes
            .class_manager
            .read()
            .get_class(main.class_id)
            .map(|c| c.num_total_fields)
            .unwrap_or(0);
        // No references live in Rust locals across this collecting ladder.
        let class_id = main.class_id;
        let receiver = crate::runtime::interpreter::native_alloc_collecting(
            &self.shared,
            &mut self.main_thread,
            "launcher main receiver",
            |shared, _| {
                shared
                    .mem
                    .heap
                    .try_alloc_object_full(class_id, num_fields)
                    .unwrap_or_else(|| crate::runtime::native_oom::raise(num_fields, "object"))
            },
        )
        .map_err(|e| {
            // zs s1/nd (`zgc-zr4ab-infallible-residuals-20261012.md` item 7b):
            // HotSpot reports this as `main`'s uncaught throwable
            // (`Exception in thread "main" java.lang.OutOfMemoryError: Java
            // heap space`, rc 1). Materialise it (`throw_runtime_error`
            // falls back to the preallocated OutOfMemoryError on a full
            // heap) so vm-cli's uncaught renderer prints HotSpot's line
            // instead of `[cratonvm] main-vm run() returned Err: ...`.
            // `CRATONVM_ZGC_LAUNCHER_OOME_THROWABLE=0`: the internal error.
            if cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_ZGC_LAUNCHER_OOME_THROWABLE",
            ) {
                crate::runtime::exceptions::throw_runtime_error(
                    &self.shared,
                    &mut self.main_thread,
                    e,
                )
            } else {
                MethodCallFailed::InternalError(VmError::Runtime(e))
            }
        })?;
        let pin = self.main_thread.native_pin_roots.len();
        self.main_thread.native_pin_roots.push(receiver);
        let made = self.invoke_on_class(
            main.class_id,
            "<init>",
            "()V",
            &[Value::Object(Some(receiver))],
        );
        if let Err(e) = made {
            self.main_thread.native_pin_roots.truncate(pin);
            return Err(e);
        }
        Ok(Some(pin))
    }

    /// Build launcher arguments with the rooted collect/retry/OOME ladder.
    /// The caller pins the result across main-class initialization and receiver
    /// construction, which happen after `CreateApplicationArgs` in `java.c`.
    pub fn launcher_main_arguments(
        &mut self,
        args: &[String],
    ) -> Result<ObjectRef, MethodCallFailed> {
        (|| {
            let class = self.shared.load_class_concurrent("java/lang/String")?;
            crate::runtime::interpreter::native_alloc_collecting(
                &self.shared,
                &mut self.main_thread,
                "launcher arguments",
                |shared, _| {
                    let array = shared
                        .mem
                        .heap
                        .try_alloc_array_full(
                            class,
                            crate::memory::heap::ArrayElementType::Reference,
                            args.len(),
                        )
                        .unwrap_or_else(|| {
                            crate::runtime::native_oom::raise(args.len(), "reference array")
                        });
                    // Both fallible factories are collection-free. Partial
                    // arrays stay private and can be discarded before retry.
                    for (i, text) in args.iter().enumerate() {
                        let value =
                            super::try_create_java_string_uninterned(shared, text).unwrap_or_else(|| {
                                crate::runtime::native_oom::raise(text.len(), "java/lang/String")
                            });
                        let _ =
                            shared
                                .mem
                                .heap
                                .set_array_element(array, i, Value::Object(Some(value)));
                    }
                    array
                },
            )
            .map_err(|error| {
                crate::runtime::exceptions::throw_runtime_error(
                    &self.shared,
                    &mut self.main_thread,
                    error,
                )
            })
        })()
    }

    /// `java.c` describes a constructor/lookup exception through JNI; it does
    /// not dispatch the application's uncaught handler. A failure while printing
    /// is ignored, retaining the partial output HotSpot produces at exhaustion.
    pub fn describe_launcher_exception(&mut self, exc: ObjectRef) {
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
        let name = self.main_thread.name.clone();
        eprint!("Exception in thread \"{name}\" ");
        let _ = std::io::stderr().flush();
        let pin = self.main_thread.native_pin_roots.len();
        self.main_thread.native_pin_roots.push(exc);
        let shared = &self.shared;
        let _ = crate::runtime::native_oom::catch_alloc_oom_on(&mut self.main_thread, |thread| {
            let exc = thread.native_pin_roots[pin];
            let class = shared.mem.heap.class_id_of(exc);
            super::invoke_on_class_shared(
                shared,
                thread,
                class,
                "printStackTrace",
                "()V",
                &[Value::Object(Some(exc))],
            )
        });
        self.main_thread.native_pin_roots.truncate(pin);
        self.main_thread.native_pending_return = None;
        let _ = std::io::stderr().flush();
    }

    /// Call `main` as `java.c` does: statically on the main class, or on the
    /// receiver [`Self::launcher_prepare_main`] pinned at `receiver_pin`;
    /// with `args` unless `main` takes none. Releases the receiver's pin.
    pub fn launcher_invoke_main(
        &mut self,
        main: &LauncherMain,
        receiver_pin: Option<usize>,
        args: ObjectRef,
    ) -> MethodCallResult {
        let receiver =
            receiver_pin.and_then(|pin| self.main_thread.native_pin_roots.get(pin).copied());
        if let Some(pin) = receiver_pin {
            self.main_thread.native_pin_roots.truncate(pin);
        }
        self.launcher_invoke_main_with(main, receiver, args)
    }

    /// zs s4/nd4 (`s3-rv2-report.md` finding 8): build `main`'s `String[]`
    /// ([`Self::launcher_main_arguments`]) BEFORE the main class is
    /// initialized, as HotSpot's `java.c` does (`CreateApplicationArgs`
    /// precedes the JNI lookup that runs `<clinit>`). Built after, a
    /// `<clinit>` that fills the heap made CratonVM fail before `main`
    /// (`ExceptionDescribe`'s 27 bytes) where HotSpot enters `main` (probe
    /// `Zs4Nd4StaticArgs`). Every collector since the merge with dev's
    /// launcher allocation fix; on ZGC `CRATONVM_ZGC_LAUNCHER_ARGS_FIRST=0`
    /// restores the old order (the launcher then builds them after the
    /// receiver).
    pub fn launcher_args_before_init(&self) -> bool {
        !super::vm_object::heap_is_zgc(&self.shared)
            || cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_LAUNCHER_ARGS_FIRST")
    }

    fn launcher_invoke_main_with(
        &mut self,
        main: &LauncherMain,
        receiver: Option<ObjectRef>,
        args: ObjectRef,
    ) -> MethodCallResult {
        let mut call_args: Vec<Value> = Vec::with_capacity(2);
        if !main.is_static {
            let Some(receiver) = receiver else {
                return Err(MethodCallFailed::InternalError(VmError::Internal {
                    message: "launcher: the instance main's receiver was not pinned".to_string(),
                }));
            };
            call_args.push(Value::Object(Some(receiver)));
        }
        if !main.no_arg {
            call_args.push(Value::Object(Some(args)));
        }
        self.invoke_on_class(main.class_id, "main", main.descriptor(), &call_args)
    }
}

#[cfg(test)]
mod tests {
    use super::main_class_load_failure_text;

    /// zs s4/nd4: the argument array, pinned as the launcher pins it, is the
    /// `String[]` of the arguments, sits in its own `native_pin_roots` slot
    /// (the receiver's pin goes above it), survives a collection at its
    /// current address, and releasing the pin leaves the pin stack where it
    /// found it. Runs on Generational and on ZGC.
    #[test]
    fn zs4nd4_pinned_main_args_hold_the_arguments_across_a_collection() {
        use crate::config::{GcAlgorithm, VmConfig};
        use cratonvm_native_api::NativeHeapAccess as _;
        for gc in [GcAlgorithm::Generational, GcAlgorithm::Zgc] {
            let mut vm = crate::vm::Vm::new(VmConfig {
                gc_algorithm: gc,
                max_heap_size: 64 << 20,
                ..VmConfig::default()
            });
            let base = vm.main_thread.native_pin_roots.len();
            let args = vec!["alpha".to_string(), "b\u{e9}ta".to_string()];
            let array = vm
                .launcher_main_arguments(&args)
                .expect("the argument array is built on an empty heap");
            let pin = vm.main_thread.native_pin_roots.len();
            vm.main_thread.native_pin_roots.push(array);
            assert_eq!(pin, base, "{gc:?}: the array takes the next pin slot");
            assert_eq!(vm.main_thread.native_pin_roots.len(), base + 1);
            crate::runtime::interpreter::force_gc_for_vm(&vm.shared, &mut vm.main_thread);
            let array = vm.main_thread.native_pin_roots[pin];
            let mut ctx = crate::vm::vm_exec::NativeContextImpl {
                shared: &vm.shared,
                thread: &mut vm.main_thread,
            };
            assert_eq!(ctx.array_length(array), 2, "{gc:?}");
            let read: Vec<Option<String>> = (0..2)
                .map(|i| match ctx.get_array_element(array, i) {
                    crate::types::Value::Object(Some(s)) => ctx.read_string(s),
                    _ => None,
                })
                .collect();
            assert_eq!(
                read,
                vec![Some("alpha".to_string()), Some("b\u{e9}ta".to_string())],
                "{gc:?}: the pinned array still names the argument strings"
            );
            vm.main_thread.native_pin_roots.truncate(pin);
            assert_eq!(vm.main_thread.native_pin_roots.len(), base, "{gc:?}");
        }
    }

    #[test]
    fn launcher_arguments_are_fresh_strings_outside_the_intern_pool() {
        use crate::types::Value;
        let mut vm = crate::vm::Vm::new(crate::config::VmConfig::default());
        let _ = crate::vm::create_java_string(&vm.shared, "duplicate");
        let args = vm
            .launcher_main_arguments(&["duplicate".into(), "duplicate".into(), "ñ🦀".into()])
            .expect("argument construction");
        let literal = crate::vm::create_java_string(&vm.shared, "duplicate");
        let first = vm.shared.mem.heap.get_array_element(args, 0).unwrap();
        let second = vm.shared.mem.heap.get_array_element(args, 1).unwrap();
        assert_ne!(first, second);
        assert_ne!(first, Value::Object(Some(literal)));
        assert_ne!(second, Value::Object(Some(literal)));
        assert!(matches!(
            vm.shared.mem.heap.get_array_element(args, 2),
            Ok(Value::Object(Some(_)))
        ));
    }

    #[test]
    fn load_failure_texts_follow_launcher_properties() {
        assert_eq!(
            main_class_load_failure_text("a.B", "java.lang.ClassNotFoundException", "a.B", true),
            "Error: Could not find or load main class a.B\nCaused by: java.lang.ClassNotFoundException: a.B"
        );
        assert_eq!(
            main_class_load_failure_text("a.B", "java.lang.NoClassDefFoundError", "a/C", true),
            "Error: Could not find or load main class a.B\nCaused by: java.lang.NoClassDefFoundError: a/C"
        );
        assert_eq!(
            main_class_load_failure_text("a.B", "java.lang.ClassFormatError", "bad", true),
            "Error: LinkageError occurred while loading main class a.B\n\tjava.lang.ClassFormatError: bad"
        );
    }
}
