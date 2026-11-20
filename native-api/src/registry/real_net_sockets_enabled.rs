#[allow(unused_imports)]
use super::*;


/// NIO-SERVER-SOCKET (route 1): cached check of the `CRATONVM_REAL_NET_SOCKETS`
/// env var. When set, the native registry drops all synthetic
/// `java/net/Socket` / `java/net/ServerSocket` registrations so real JDK
/// bytecode drives the `sun/nio/ch/Net` path. Cached in a `OnceLock` because
/// `register()` is called thousands of times at startup.
pub(in crate::registry) fn real_net_sockets_enabled() -> bool {
    cratonvm_types::flags::flags().io.real_net_sockets
}

/// REAL-FORKJOINPOOL (opt-in `CRATONVM_REAL_FORKJOINPOOL`): when set, the
/// registry drops the synthetic `java/util/concurrent/ForkJoinPool` natives
/// **except `execute`** so the real JDK pool bytecode runs (proper init,
/// parallelism = cpus-1, real `ForkJoinWorkerThread`s; work-stealing degrades to
/// caller-runs). The synthetic pool's `commonPool()` returns an uninitialised
/// real-class instance (no queues), so real `invokeAll`/`submit` throws
/// `RejectedExecutionException` — dropping the natives fixes that and enables
/// Weld's real concurrent CDI bootstrap (`ConcurrentBeanDeployer`).
///
/// `execute` is KEPT synthetic (eager-inline, run on the caller) so that
/// `CompletableFuture.*Async` (which schedules every stage via `execute`) keeps
/// working under the real pool — see below.
///
/// **Opt-in, NOT a safe global default.** CratonVM's cross-worker memory
/// ordering doesn't reliably publish an object-reference field written by one
/// worker to a task on another worker. The eager-inline `execute` masks this for
/// `CompletableFuture` (everything runs caller-side), but `parallel-stream` /
/// fork-join work uses `ForkJoinTask.fork`/`invoke` (not `execute`), runs on
/// real workers, and can read stale-null cross-worker state — observed as a
/// regression in `PersistenceXmlParserTest` (4/4 → 2/4) when this was forced on
/// globally. So the synthetic pool stays the default; this gate is for
/// concurrent-CDI workloads that don't lean on parallel-stream result passing.
/// HIB-CV-20.
pub(in crate::registry) fn real_forkjoinpool_enabled() -> bool {
    cratonvm_types::flags::flags().natives.real_forkjoinpool
}

/// One event of the nested-jar rows (the `URLClassPath$Loader` walk index and
/// the two `Resource.getBytes` bytes indexes), for the `CRATONVM_DBG=access`
/// LOADER census ([`NativeContext::note_nested_jar_row`]; interpreter round
/// i2 wave 69, lane L5,
/// `i68-L5-proposal-count-nested-jar-row-rebuilds-and-refusals`). Diagnostic
/// only: no answer depends on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NestedJarRowEvent {
    /// A miss built a row (`nested-jar-rows-built=`).
    Built,
    /// A built row whose file changed was rebuilt (`nested-jar-rows-rebuilt=`).
    Rebuilt,
    /// A build, rebuild or retry refused, or a changed row was turned into a
    /// refusal (`nested-jar-rows-refused=`).
    Refused,
    /// A stamped refusal was rebuilt after its file changed
    /// (`nested-jar-refusals-retried=`).
    RefusalRetried,
    /// A refused row answered `None` without a build
    /// (`nested-jar-refusals-kept=`).
    RefusalKept,
    /// A row was built from the application class path's own parse of its
    /// fat jar (`CRATONVM_NESTED_JAR_CLASS_PATH_OUTER`;
    /// `nested-jar-class-path-lent=`).
    ClassPathLent,
    /// A directory-only row's read went through a kept outer handle
    /// (`CRATONVM_NESTED_JAR_OUTER_HANDLE_REUSE`;
    /// `nested-jar-outer-handle-reused=`).
    OuterHandleReused,
    /// A jar-loader bytes row was built from the application class path's
    /// own archive (`CRATONVM_JAR_LOADER_CLASS_PATH_ARCHIVE`;
    /// `jar-loader-class-path-lent=`; interpreter round i2 wave 70, lane L5).
    JarLoaderClassPathLent,
    /// A jar-loader resource's bytes were served straight from the
    /// application class path's own entry, with no row
    /// (`CRATONVM_JAR_LOADER_CLASS_PATH_DIRECT`;
    /// `jar-loader-class-path-direct=`; interpreter round i3 wave 71, lane L5).
    JarLoaderClassPathDirect,
    /// The class path did not hold the jar unchanged, so the direct ask fell
    /// back to the row path (`CRATONVM_JAR_LOADER_CLASS_PATH_DIRECT`;
    /// `jar-loader-class-path-direct-fallback=`; wave 71, lane L5).
    JarLoaderClassPathDirectFallback,
    /// A nested bytes row's hit skipped its `stat` because the pooled read
    /// that follows checks the outer file itself
    /// (`CRATONVM_NESTED_JAR_POOLED_STAT`; `nested-jar-pooled-stat-skipped=`;
    /// interpreter round i3 wave 71, lane L5).
    PooledStatSkipped,
}

/// What the application class path answers for one entry of a jar it holds
/// ([`NativeClassAccess::class_path_jar_entry_bytes`]; interpreter round i3
/// wave 71, lane L5, `CRATONVM_JAR_LOADER_CLASS_PATH_DIRECT`;
/// `i70-L5-proposal-serve-class-path-jar-bytes-without-a-row`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClassPathJarBytes {
    /// No archive of the application class path is the file at that path
    /// with its load-time stamp (not on the class path, rewritten since, no
    /// stamp kept): the caller asks as it would without the class path.
    NotHeld,
    /// The class path holds the file unchanged, but the entry is not served:
    /// no plain view (Multi-Release, signed, duplicate names), a directory or
    /// absent name, a central-directory CRC-32 or size other than the one
    /// asked for, or an entry that did not read cleanly. A row built over the
    /// same archive would answer the same, so the caller runs the body.
    Body,
    /// The entry's bytes, CRC-checked, of exactly the asked size.
    Bytes(Vec<u8>),
}

/// What a blocking region stands for, so the thread inside it reads the state
/// HotSpot reports for the same Java code (interpreter round i2 wave 59, lane
/// L6; stage 1 of `docs/feature-designs/jvmti-delivery-threading.md`).
/// Passed to [`NativeContext::begin_blocking_region_as`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    /// A Java-level wait with no deadline (`LockSupport.park`, `Object.wait`,
    /// `Thread.join`, a blocking queue's `take`): `WAITING`. What a plain
    /// [`NativeContext::begin_blocking_region`] reports.
    Waiting,
    /// A Java-level wait with a deadline (`Thread.sleep`, `parkNanos`, a
    /// timed `Object.wait`): `TIMED_WAITING`, as
    /// [`NativeContext::begin_timed_blocking_region`].
    TimedWaiting,
    /// A registered native standing in for a JDK `native` method that blocks
    /// in C (a socket `accept` / `read` / `write` / `connect`, a file or pipe
    /// read, a selector's poll, a process wait): HotSpot runs it in
    /// `_thread_in_native`, so `Thread.getState()` stays `RUNNABLE`, JVMTI
    /// adds `IN_NATIVE`, JMX's `ThreadInfo.isInNative()` is true, JDWP says
    /// `RUNNING`, and another thread's `getStackTrace()` lists the native on
    /// top. Only with `CRATONVM_BLOCKED_IN_NATIVE_RUNNABLE`; otherwise (and in
    /// a context that does not tell the kinds apart) it is [`Self::Waiting`].
    InNative,
    /// `Thread.sleep` (every door to `JVM_SleepNanos`: `sleep(long)`,
    /// `sleep(long, int)`, `sleepNanos0`, `TimeUnit.sleep`): `TIMED_WAITING`,
    /// as [`Self::TimedWaiting`], and with `CRATONVM_PUBLISHED_WAIT_KIND` the
    /// region also publishes that it is a sleep, so JVMTI reads `SLEEPING`
    /// and JDWP `ThreadStatus.SLEEPING` whatever frame the sleep was entered
    /// from (interpreter round i2 wave 63, lane L1; stage 2 of
    /// `docs/feature-designs/jvmti-delivery-threading.md`).
    /// In a context that does not tell the kinds apart it is
    /// [`Self::TimedWaiting`].
    Sleeping,
    /// A region standing for a JDK `Object.wait` loop with no deadline that
    /// the native does not run as one (the served `Thread.join()`, whose JDK
    /// body is `while (isAlive()) wait(0)`): `WAITING`, as [`Self::Waiting`],
    /// and with `CRATONVM_PUBLISHED_ALL_WAIT_KINDS` the region also publishes
    /// JVMTI's `IN_OBJECT_WAIT` (interpreter round i2 wave 64, lane L1;
    /// `docs/feature-designs/native-dispatch-architecture.md`).
    /// In a context that does not tell the kinds apart it is [`Self::Waiting`].
    ObjectWait,
    /// [`Self::ObjectWait`] with a deadline (the served `Thread.join(ms)`, a
    /// `wait(delay)` loop): `TIMED_WAITING`, as [`Self::TimedWaiting`].
    TimedObjectWait,
    /// A region standing for JDK code that parks with no deadline (the served
    /// `Thread.join()` of a `java.lang.VirtualThread`, whose JDK body parks in
    /// `joinNanos`): `WAITING`, and with `CRATONVM_PUBLISHED_ALL_WAIT_KINDS`
    /// JVMTI's `PARKED` (wave 64, lane L1). In a context that does not tell
    /// the kinds apart it is [`Self::Waiting`].
    Parked,
    /// [`Self::Parked`] with a deadline: `TIMED_WAITING`.
    TimedParked,
}

/// gci i28/dg (`gci-i14mp-memory-pool-and-gc-mxbean-review-findings-FIXED-20261016`
/// item 11b): one diagnostic command as HotSpot's `jmm_GetDiagnosticCommandInfo`
/// describes it (`dcmdInfo`), for the four
/// `com.sun.management.internal.DiagnosticCommandImpl` natives. The strings
/// are the `DiagnosticCommandInfo` constructor's, in its order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DcmdInfo {
    pub name: String,
    pub description: String,
    /// HotSpot's impact text (`Low`, `Medium: ...`, `High: ...`).
    pub impact: String,
    /// `java.lang.management.ManagementPermission` or `None`.
    pub permission_class: Option<String>,
    /// `monitor` / `control` or `None`.
    pub permission_name: Option<String>,
    pub permission_action: Option<String>,
    pub enabled: bool,
    pub arguments: Vec<DcmdArgInfo>,
}

/// gci i28/dg: one argument of a [`DcmdInfo`] (HotSpot's `dcmdArgInfo`, the
/// `DiagnosticCommandArgumentInfo` constructor's order).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DcmdArgInfo {
    pub name: String,
    pub description: String,
    /// HotSpot's type name: `STRING`, `BOOLEAN`, `JLONG`, ...
    pub arg_type: String,
    pub default_value: Option<String>,
    pub mandatory: bool,
    /// `true` for a `-name=value` option, `false` for a positional argument.
    pub option: bool,
    pub multiple: bool,
    /// The positional index, `-1` for an option.
    pub position: i32,
}

/// VM-owned data needed to materialize a truthful JMX `ThreadInfo` object.
///
/// The object references are strong, GC-remapped registry roots for the short
/// interval in which a thread owns, waits on, or contends for a lock.  Native
/// JMX code pins them before doing any allocating work.
#[derive(Clone, Debug, Default)]
pub struct ThreadJmxSnapshot {
    pub thread_object: Option<ObjectRef>,
    pub thread_id: i64,
    pub thread_name: String,
    /// JMM/JVMTI thread-status bits reserved for consumers that use the
    /// encoded state rather than the JDK 25 `Thread.State` field.
    pub thread_status: i32,
    pub stack_trace: Vec<StackTraceEntry>,
    pub lock: Option<ObjectRef>,
    /// Logical JMM class name for `lock` when a VM shim deliberately models
    /// the backing synchronizer without materializing its private JDK object.
    pub lock_class_name: Option<String>,
    pub lock_owner_id: i64,
    pub lock_owner_name: Option<String>,
    pub locked_monitors: Vec<ObjectRef>,
    /// Parallel to `locked_monitors` when non-empty: the `stack_trace` depth
    /// (0 = innermost) of the frame that locked each monitor, `-1` for one
    /// held by no interpreted frame (compiled code, JNI). Empty when the VM
    /// could not attribute them, in which case every monitor is reported at
    /// the innermost frame. (Interpreter round i1 wave 24, lane L7.)
    pub locked_monitor_depths: Vec<i32>,
    pub locked_synchronizers: Vec<ObjectRef>,
    /// Cumulative monitor-enter blocking, measured only while contention
    /// monitoring is enabled. Durations are milliseconds per the JMM API.
    pub blocked_time_ms: i64,
    pub blocked_count: i64,
    /// Cumulative Object.wait parking, also in JMM milliseconds/counts.
    pub waited_time_ms: i64,
    pub waited_count: i64,
    /// JIT round 25 (`r25-locked-frame-depth-misses-the-park-frame-FIXED-20261024`;
    /// with W6-F's held read): the thread's state AT THE INSTANT `stack_trace`
    /// was taken, as `NativeContext::thread_run_state` codes it (1 RUNNABLE,
    /// 2 TERMINATED, 3 WAITING, 4 BLOCKED, 5 TIMED_WAITING), when the VM
    /// knows it: always for another thread's `ThreadInfo` read inside one
    /// frame-trace pause (`vm_exec` `HeldThreadRead`); otherwise for a trace a
    /// pause or handshake made a running thread publish at a Java poll, or
    /// one standing in a park / wait / sleep native. `None`: the door asks the
    /// thread's current state, as before. HotSpot reads both at one
    /// safepoint, so a `ThreadInfo` never pairs a running stack with
    /// `TIMED_WAITING`.
    pub thread_state_at_trace: Option<u8>,
}

pub(in crate::registry) fn value_matches_primitive_array(element_type: ArrayElementType, value: Value) -> bool {
    match element_type {
        ArrayElementType::Boolean
        | ArrayElementType::Byte
        | ArrayElementType::Char
        | ArrayElementType::Short
        | ArrayElementType::Int => matches!(value, Value::Int(_)),
        ArrayElementType::Long => matches!(value, Value::Long(_)),
        ArrayElementType::Float => matches!(value, Value::Float(_)),
        ArrayElementType::Double => matches!(value, Value::Double(_)),
        ArrayElementType::Reference => false,
    }
}

// ---------------------------------------------------------------------------
// Reflection metadata types
// ---------------------------------------------------------------------------

/// Metadata for a field declared in a class, used by reflection.
pub struct FieldMetadata {
    pub name: String,
    pub descriptor: String,
    pub access_flags: u16,
    /// Absolute heap field index (accounts for inherited instance fields).
    pub slot_index: usize,
    pub declaring_class_id: ClassId,
    pub is_static: bool,
}

/// Metadata for a method declared in a class, used by reflection.
#[derive(Clone, Debug)]
pub struct MethodMetadata {
    pub name: String,
    pub descriptor: String,
    pub access_flags: u16,
    pub declaring_class_id: ClassId,
    /// Internal names of exception classes the method declares to
    /// throw (JVMS §4.7.5 `Exceptions` attribute). Empty for methods
    /// without a `throws` clause. Populated by the VM's
    /// `declared_methods` impl from `Attribute::Exceptions`.
    ///
    /// Used by `native_builtins::build_proxy_spec_for` (WP2.5 v3 item 3)
    /// to thread the declared exception set through to the generated
    /// proxy class's `<clinit>` so that `wrap_undeclared_throwable`
    /// (WP2.5 v3 item 6) can match thrown exceptions against it.
    pub exceptions: Vec<String>,
    /// The JVMS §4.7.9 `Signature` attribute of this method, i.e. its
    /// generic signature, or `None` when the method is not generic.
    ///
    /// Carried here for the same reason as `exceptions`: whoever produced
    /// this metadata already walked the declaring class's method table, and
    /// `create_method_object` would otherwise search that table AGAIN, by
    /// name+descriptor, once per mirror. In a `Class.getDeclaredMethods()`
    /// call that loop already runs once per method, so the search made the
    /// cost per method grow with the method count — 766 us of a 2440 us call
    /// on a 1000-method class went to the exceptions lookup and 659 us to
    /// this one. A fabricated method (a synthetic stub, a lambda-proxy SAM,
    /// a shim) is not generic and correctly reports `None`.
    pub signature: Option<String>,
}

/// What [`NativeContext::try_reserve_class_mirror`] answers (zs s7/nd6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MirrorReservation {
    /// This context does not reserve mirrors: define, then mint as before.
    Unsupported,
    /// The heap refused the reservation without collecting.
    Refused,
    /// The mirror object, unpublished; pin it across the define.
    Reserved(ObjectRef),
}

/// WP2.3 — defineClass options carried through `NativeContext::define_class_full`.
///
/// Mirrors `cratonvm_classloading::DefineClassOptions` but without the
/// crate dependency, so native-builtins can construct it without
/// importing classloading directly. The VM's `NativeContext`
/// implementation translates this into a `DefineClassOptions` and
/// delegates to `ClassManager::define_class_with_options`.
#[derive(Debug, Clone, Default)]
pub struct DefineClassFull {
    /// Optional override of the registered name (for hidden / mangled).
    pub override_name: Option<String>,
    /// Mark the new class as hidden (JEP 371).
    pub hidden: bool,
    /// Skip bytecode verification on these bytes.
    pub skip_verification: bool,
    /// Optional URL for the resulting `CodeSource` (e.g. `"file:/foo.jar"`).
    pub code_source_url: Option<String>,
    /// DER-encoded signer certificates to attach as the `CodeSource`.
    pub code_source_certificates: Vec<Vec<u8>>,
    /// Allow redefinition (replaces in place if class exists). Used by WP2.4.
    pub allow_redefine: bool,
    /// Nest-host attribution (NESTMATE). Internal class name.
    pub nest_host_class_name: Option<String>,
    /// gci i11/cl: the LOOKUP class (internal name) of a NESTMATE hidden
    /// define -- `nest_host_class_name` is that class's nest host, which
    /// differs when the lookup class is a nest member. Only the
    /// `-Xlog:class+load` `source:` reads it. Mirrors
    /// `cratonvm_classloading::DefineClassOptions::lookup_class_name`.
    pub lookup_class_name: Option<String>,
    /// If `true`, run `<clinit>` on the new class before returning.
    pub initialize: bool,
    /// Privileged define: the bytes come from a trusted JVM-internal
    /// code-generation path (`sun.misc.Unsafe.defineClass` /
    /// `jdk.internal.misc.Unsafe.defineClass0`) that, on HotSpot, bypasses
    /// `ClassLoader.preDefineClass`'s "Prohibited package name: java.*"
    /// guard. Set by the Unsafe.defineClass natives so toolchains like
    /// ByteBuddy can inject a privileged accessor (e.g.
    /// `java.lang.ClassLoader$ByteBuddyAccessor$V1`) into a protected
    /// platform package, exactly as they do on a real JVM. Off by default;
    /// the ordinary `ClassLoader.defineClass` path leaves it false so the
    /// spoofing guard still applies there.
    pub privileged_define: bool,
    /// Force loader-faithful supertype/interface linking for this define —
    /// mirrors `cratonvm_classloading::DefineClassOptions::force_loader_faithful_linking`.
    /// Set by generated `$ProxyN` class definitions
    /// (`native_builtins::define_or_get_proxy_class`) so the proxy links
    /// against the EXACT interface `ClassId` it was generated for, rather
    /// than the loader-agnostic global `load_class(name)` fallback that
    /// `CRATONVM_LOADER_AWARE_RESOLUTION` (default off) otherwise gates.
    pub force_loader_faithful_linking: bool,
    /// Exact superclass identity already resolved through the defining
    /// lookup/loader.
    pub superclass_id_override: Option<ClassId>,
    /// Exact interface identities, in class-file declaration order.
    pub interface_id_overrides: Option<Vec<ClassId>>,
    /// JEP 371: this `hidden` class was defined WITHOUT
    /// `MethodHandles.Lookup.ClassOption.STRONG` (`defineClass0`'s
    /// `STRONG_LOADER_LINK` bit clear), so it may unload as soon as its
    /// mirror and its instances are unreachable, while its defining loader
    /// lives. Ignored unless `hidden` is set.
    ///
    /// `false` (the default) is the strong, loader-lifetime behaviour every
    /// define had before gc-common w18-d, so the `..Default::default()`
    /// literals across the tree (`service_loader.rs`, `cglib_enhancer.rs`,
    /// ...) are unchanged. The define natives that honour it
    /// (`classloader::cl_define_class0`, `lang_system::native_classloader_define_class0`,
    /// the `Lookup.defineHiddenClass*` natives) read it back after the define
    /// and register the class with `classloader::register_non_strong_hidden_class`.
    /// See `docs/feature-designs/class-resolution-and-caching.md`.
    pub non_strong_hidden: bool,
    /// The `ProtectionDomain` the `defineClass*` native received, handed to
    /// the load-time `ClassFileTransformer`s as their `protectionDomain`
    /// argument, as HotSpot's `JvmtiClassFileLoadHookPoster` does. `None`
    /// passes `null`. Set only by the `--real-jdk` `defineClass1` /
    /// `defineClass2` natives, which hold it pinned across the define; the
    /// definer pins it again before anything allocates (interpreter round i1
    /// wave 28; `docs/feature-designs/native-dispatch-architecture.md`).
    pub protection_domain: Option<ObjectRef>,
    /// Define in the BOOTSTRAP loader's namespace. The `loader_id` argument
    /// cannot say so: its `0` is the "unspecified, use the application
    /// namespace" sentinel. Set only by the `--real-jdk` `defineClass0` native
    /// for a `Lookup.defineClass` on a lookup class of the bootstrap loader
    /// (the null loader), where HotSpot's `JVM_LookupDefineClass` defines the
    /// class in that loader (interpreter round i1 wave 37, lane L5; probe
    /// `tools/probes/interp/L5/L5W37BootstrapLookupDefine.java`).
    pub bootstrap_namespace: bool,
    /// The class file of these exact bytes, already parsed by the caller
    /// (a `cratonvm_reader::ClassFile`, type-erased because this crate does
    /// not depend on the reader). The definer uses it instead of parsing the
    /// bytes again when it can take sole ownership of it and no load-time
    /// transformer changed the bytes; anything else (another type, a shared
    /// `Arc`, transformed bytes) parses as before. Set only by the
    /// `defineClass1` / `defineClass2` natives under
    /// `CRATONVM_DEFINE_ONE_PARSE` (interpreter round i3 wave 72, lane L5).
    /// `None` (the default) parses.
    pub preparsed: Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>,
    /// The define door's loader is a user-defined loader (or a bare
    /// `URLClassLoader` running the JDK's bodies) whatever namespace the
    /// door chose: with `CRATONVM_LOADER_AWARE_RESOLUTION=0` such a loader's
    /// define of a fresh name goes to namespace `0`, which the class manager
    /// cannot tell from an application-loader define. Carried into
    /// `DefineClassOptions::user_loader_define` (interpreter round i4 wave
    /// 96, lane L5). `false` (the default) decides from the namespace as
    /// before.
    pub user_loader_define: bool,
    /// The defining loader's object, recorded in the defining-loader table
    /// INSIDE the define, before the class becomes visible to other threads.
    /// Registering it after `define_class_full` returns leaves a window in
    /// which a second thread resolves the new class (by its namespace row),
    /// finds no defining loader, and either reads a mirror whose
    /// `classLoader` is null (`Class.getPackage()` -> NPE) or resolves the
    /// class's own references through the loader-blind global route (a second
    /// copy in the application namespace: `IllegalAccessError` /
    /// `ClassCastException` across the runtime-package split). Set by
    /// `defineClass1`; `None` (the default) registers after the define only.
    pub defining_loader: Option<ObjectRef>,
    /// zt t16/df6: the caller makes the `ClassLoader.addClass` upcall after
    /// this define and then settles the name with
    /// [`NativeContext::bind_unbound_define`] (the upcall returned) or
    /// [`NativeContext::withdraw_unbound_define`] (it threw), as HotSpot's
    /// `define_instance_class` binds the name only after the upcall. Mirrors
    /// `cratonvm_classloading::DefineClassOptions::bind_name_after_add_class`,
    /// which honours it only for a fresh, named class of a user-defined
    /// namespace. Never with `initialize` (the initializer may resolve the
    /// class by its name). `false` (the default) binds at the define.
    pub bind_name_after_add_class: bool,
}

impl DefineClassFull {
    /// `hidden` and not `STRONG`: the define [`Self::non_strong_hidden`]
    /// describes.
    #[inline]
    pub fn unloads_with_its_mirror(&self) -> bool {
        self.hidden && self.non_strong_hidden
    }
}

/// What a lambda proxy class declares, from the VM's one record of it
/// (`vm::runtime::lambda_proxy::LambdaProxyFacts`, which the JVMTI and JNI
/// readers answer from): its direct superinterfaces in
/// `InnerClassLambdaMetafactory`'s order and its declared methods in the
/// spun class file's order, constructor first. Interpreter round i6 wave 118,
/// lane L3 (`NativeContext::lambda_proxy_record`; proposal
/// `i55-L1-proposal-lambda-proxies-answer-the-tool-interfaces-from-one-record`).
#[derive(Clone, Debug, Default)]
pub struct LambdaProxyRecord {
    pub interfaces: Vec<ClassId>,
    pub methods: Vec<MethodMetadata>,
}

/// The lambda-call-site metadata needed to round-trip a serializable lambda.
///
/// Mirrors the fields of `java.lang.invoke.SerializedLambda` that matter for
/// reconstruction: the functional-interface SAM, the implementation method
/// handle, the instantiated (specialized) descriptor, and the capture-value
/// types. Produced by [`NativeContext::lambda_proxy_serial_metadata`] for a
/// synthetic lambda-proxy class and consumed (alongside the captured field
/// values) by the deserialization path, which feeds it straight into
/// [`NativeContext::register_lambda_proxy`].
#[derive(Clone, Debug)]
pub struct LambdaSerialMetadata {
    pub functional_interface: String,
    pub sam_method_name: String,
    pub sam_descriptor: String,
    pub impl_class: String,
    pub impl_member: String,
    pub impl_descriptor: String,
    /// JVMS `reference_kind` byte (1..=9) of the implementation method handle.
    pub impl_ref_kind: u8,
    pub instantiated_descriptor: String,
    /// One type char per captured value (`'L'`, `'I'`, `'J'`, ...), in
    /// factory-argument / proxy-field order.
    pub capture_types: String,
    /// `altMetafactory` `FLAG_MARKERS` interfaces (internal names), in
    /// bootstrap-argument order: the spun class implements them after its
    /// functional interface. Empty for a plain `metafactory` lambda.
    pub marker_interfaces: Vec<String>,
    /// `altMetafactory` `FLAG_BRIDGES` descriptors of the SAM name: the spun
    /// class declares one public forwarding method per descriptor. Empty in
    /// the usual case.
    pub bridges: Vec<String>,
}

/// Why (or whether) a `LambdaMetafactory`-spun proxy is `java.io.Serializable`.
///
/// The JDK decides this in `AbstractValidatingLambdaMetafactory`: a lambda is
/// serializable when its call site passed `FLAG_SERIALIZABLE` **or** its
/// functional interface already extends `Serializable`, and the spun class gets
/// `Serializable` *added* to its interface list only in the first case. Each arm
/// drives a different reflective surface, so the distinction is modelled here
/// rather than flattened to a bool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LambdaSerializability {
    /// Not serializable: no `writeReplace()`, no `Serializable` in
    /// `getInterfaces()`, and `(Serializable) lambda` throws
    /// ClassCastException. This is the common case -- every plain
    /// `metafactory` lambda over a non-`Serializable` interface, e.g.
    /// `Supplier<String> s = () -> "x"`.
    NotSerializable,
    /// Serializable because the functional interface itself extends
    /// `Serializable`. Gets a `writeReplace()`, but `getInterfaces()` still
    /// reports only the functional interface -- `Serializable` is already
    /// reachable through it, so the metafactory adds no marker.
    ByInheritance,
    /// Serializable because the call site passed `FLAG_SERIALIZABLE` -- an
    /// `(Iface & Serializable)` intersection cast, which is how every JDK
    /// `Comparator.comparing*` factory is written. Gets a `writeReplace()`
    /// AND `java.io.Serializable` appended to `getInterfaces()` /
    /// `getGenericInterfaces()`.
    ByFlag,
    /// Interpreter round i4 wave 86 (lane L4): the functional interface extends
    /// `Serializable` but the call site did NOT pass `FLAG_SERIALIZABLE` (a
    /// plain `metafactory` site over a `Serializable` interface, a reflective
    /// `LambdaMetafactory.metafactory` call). JDK 25's
    /// `InnerClassLambdaMetafactory` calls this `accidentallySerializable`:
    /// the instance IS a `Serializable` (through its interface), but the spun
    /// class gets no `writeReplace()`; it gets the serialization-HOSTILE
    /// `private final void writeObject(ObjectOutputStream)` /
    /// `readObject(ObjectInputStream)` that throw
    /// `NotSerializableException("Non-serializable lambda")`.
    AccidentallySerializable,
}

/// Compute a fast 128-bit hash key for a native method triple.
///
/// Returns a `(u64, u64)` pair. The two halves are produced by **two
/// genuinely different hash functions**, not the same FNV-1a function
/// re-seeded:
///
///  * the first half uses the standard 64-bit FNV-1a prime
///    (`0x100000001b3`), and
///  * the second half uses a *different* odd multiplier
///    (`0x880355f21e6d1965`, the well-known `fasthash`/`xxhash`-family
///    mixing constant) so the two passes are not just affine variants
///    of one another.
///
/// Both halves then get a splitmix64-style finalization avalanche, so
/// any residual structural correlation between the two passes is
/// destroyed before the values are used as a key.
///
/// With two effectively-independent 64-bit hashes the composite key is
/// ~128-bit; the birthday-collision probability for the few thousand
/// native methods we register is negligibly small (and `register`
/// still carries a `debug_assert!` collision check as a backstop).
#[inline]
pub(crate) fn native_method_hash(class: &str, method: &str, descriptor: &str) -> (u64, u64) {
    native_method_hash_from(native_class_hash(class), method, descriptor)
}

/// FNV prime for the first pass of the native-method digest.
pub(in crate::registry) const FNV_PRIME: u64 = 0x100000001b3;
/// A distinct odd multiplier for the second pass — different bit pattern
/// *and* different magnitude, so the second hash is not a re-seeded copy of
/// the first.
pub(in crate::registry) const ALT_PRIME: u64 = 0x880355f21e6d1965;

/// The **unfinalized** accumulator pair after hashing only the class-name
/// component of a native-method triple.
///
/// This is the prefix state [`native_method_hash`] would hold halfway through,
/// exposed so a lookup can (a) test whether the class registers ANY native at
/// all before paying for the rest of the digest, and (b) finish the digest
/// from here without re-walking the class name. Deliberately NOT passed
/// through [`fmix64`]: it is a resumable state, not a key.
///
/// PERF (H2 `TestFileSystem.testConcurrent`, 2026-07-26). `slot_for_exact` is
/// on the interpreter's every-invoke path via `invoke_or_native`, and it was
/// the single largest entry in a live 30s/999Hz profile of that test —
/// **8.75%** of CPU across its two real threads, plus a large share of the
/// `__memcmp_evex_movbe` time spent name-verifying the digest hit. The digest
/// is a byte-at-a-time walk of all three strings (~60-100 bytes, two
/// accumulators), and the overwhelming majority of the calls come from
/// application classes — `org/h2/mvstore/...`, `org/h2/store/...` — that
/// register no natives whatsoever, so all of that work produced a miss. The
/// class name alone is ~25 of those bytes and answers "miss" for every one of
/// them.
#[inline]
pub(in crate::registry) fn native_class_hash(class: &str) -> (u64, u64) {
    let mut h1 = 0xcbf29ce484222325;
    let mut h2 = 0x9e3779b97f4a7c15;
    hash_component_pair(&mut h1, &mut h2, class, FNV_PRIME, ALT_PRIME);
    (h1, h2)
}

/// Ways of the per-thread [`native_class_hash`] memo (a power of two).
pub(in crate::registry) const CLASS_HASH_MEMO_BITS: u32 = 6;
pub(in crate::registry) const CLASS_HASH_MEMO_WAYS: usize = 1 << CLASS_HASH_MEMO_BITS;
/// Longest class name the memo holds (bytes). Longer names are hashed every
/// time, exactly as before.
pub(in crate::registry) const CLASS_HASH_MEMO_NAME_MAX: usize = 80;

/// One remembered [`native_class_hash`] answer: the class name's bytes, held
/// inline (no allocation, no destructor), and the prefix state they hash to.
#[derive(Clone, Copy)]
pub(in crate::registry) struct ClassHashMemoEntry {
    pub(in crate::registry) len: u8,
    pub(in crate::registry) name: [u8; CLASS_HASH_MEMO_NAME_MAX],
    pub(in crate::registry) state: (u64, u64),
}

impl ClassHashMemoEntry {
    /// `len == 0` never matches: an empty name is not memoised.
    pub(in crate::registry) const EMPTY: Self = Self {
        len: 0,
        name: [0; CLASS_HASH_MEMO_NAME_MAX],
        state: (0, 0),
    };
}

thread_local! {
    /// nio21 wave 17 L1: this thread's [`native_class_hash`] answers, direct
    /// mapped by [`class_hash_memo_slot`]. Plain data with no destructor, so a
    /// read never fails during thread teardown (it is still asked through
    /// `try_with`, and a refusal hashes).
    pub(in crate::registry) static CLASS_HASH_MEMO: std::cell::RefCell<[ClassHashMemoEntry; CLASS_HASH_MEMO_WAYS]> =
        const { std::cell::RefCell::new([ClassHashMemoEntry::EMPTY; CLASS_HASH_MEMO_WAYS]) };
}

/// The memo way for `class`: a multiplicative mix of its length and its first
/// and last eight bytes (the package prefix and the simple-name tail, which
/// is where JDK class names differ). Any answer is correct; the way only
/// decides which name a lookup is compared against.
#[inline]
pub(in crate::registry) fn class_hash_memo_slot(class: &[u8]) -> usize {
    let n = class.len();
    let word = |bytes: &[u8]| -> u64 {
        let mut buf = [0u8; 8];
        let take = bytes.len().min(8);
        buf[..take].copy_from_slice(&bytes[..take]);
        u64::from_le_bytes(buf)
    };
    let head = word(class);
    let tail = if n > 8 { word(&class[n - 8..]) } else { 0 };
    // Cast: the length goes into the mix; the top bits index the ways.
    let mix = (head ^ tail.rotate_left(23) ^ (n as u64)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (mix >> (64 - CLASS_HASH_MEMO_BITS)) as usize
}

/// [`native_class_hash`], answered from this thread's memo when it last
/// hashed the same class name in the same way (nio21 wave 17 L1,
/// `CRATONVM_NATIVE_CLASS_HASH_MEMO`, default ON; the registry reads the
/// switch once at construction).
///
/// WHY: every by-name registry lookup (`find`, `find_with_kind_and_id`,
/// `resolve_id`, `slot_for_exact`) opens with [`NativeMethodRegistry::
/// class_prefilter`], which walks the class name a byte at a time through two
/// serially dependent multiplies (~4 cycles a byte). A compiled call that
/// reaches the by-name dispatch asks it three to five times per call for the
/// same class name: 4.6% of the w16 `rlock` profile was `hash_byte_pair`
/// under those four functions.
///
/// SOUND: the answer is a pure function of the name's bytes, and a hit
/// compares ALL of them (the length and an exact byte compare) before it
/// answers, so a hit returns exactly what the walk would. Nothing about the
/// registry's contents is remembered: the `classes_with_natives` probe still
/// runs on every lookup, so a registration after the fill is seen at once.
#[inline]
pub(in crate::registry) fn native_class_hash_memoized(class: &str) -> (u64, u64) {
    let bytes = class.as_bytes();
    let n = bytes.len();
    if n == 0 || n > CLASS_HASH_MEMO_NAME_MAX {
        return native_class_hash(class);
    }
    let slot = class_hash_memo_slot(bytes);
    let hit = CLASS_HASH_MEMO
        .try_with(|memo| {
            let memo = memo.try_borrow().ok()?;
            let entry = &memo[slot];
            (usize::from(entry.len) == n && entry.name[..n] == *bytes).then_some(entry.state)
        })
        .ok()
        .flatten();
    if let Some(state) = hit {
        return state;
    }
    let state = native_class_hash(class);
    let _ = CLASS_HASH_MEMO.try_with(|memo| {
        if let Ok(mut memo) = memo.try_borrow_mut() {
            let entry = &mut memo[slot];
            entry.len = n as u8; // Cast: n <= CLASS_HASH_MEMO_NAME_MAX < 256
            entry.name[..n].copy_from_slice(bytes);
            entry.state = state;
        }
    });
    state
}


/// Finish a native-method digest from a [`native_class_hash`] prefix state.
#[inline]
pub(in crate::registry) fn native_method_hash_from(class_state: (u64, u64), method: &str, descriptor: &str) -> (u64, u64) {
    let (mut h1, mut h2) = class_state;
    hash_byte_pair(&mut h1, &mut h2, b'.', FNV_PRIME, ALT_PRIME);
    hash_component_pair(&mut h1, &mut h2, method, FNV_PRIME, ALT_PRIME);
    hash_byte_pair(&mut h1, &mut h2, b'.', FNV_PRIME, ALT_PRIME);
    hash_component_pair(&mut h1, &mut h2, descriptor, FNV_PRIME, ALT_PRIME);
    (fmix64(h1), fmix64(h2))
}

/// Update both independent native-method hash accumulators for one byte.
#[inline]
pub(in crate::registry) fn hash_byte_pair(h1: &mut u64, h2: &mut u64, byte: u8, prime1: u64, prime2: u64) {
    *h1 ^= byte as u64;
    *h1 = h1.wrapping_mul(prime1);
    *h2 ^= byte as u64;
    *h2 = h2.wrapping_mul(prime2);
}

/// One scan over a component, updating the two statistically independent
/// FNV-style accumulators in parallel. This preserves the previous key format
/// while avoiding two full byte walks for every hot-path registry lookup.
#[inline]
pub(in crate::registry) fn hash_component_pair(h1: &mut u64, h2: &mut u64, component: &str, prime1: u64, prime2: u64) {
    for byte in component.bytes() {
        hash_byte_pair(h1, h2, byte, prime1, prime2);
    }
}

/// splitmix64 finalizer — an avalanche mix that spreads every input bit
/// across the whole 64-bit output. Applied to each hash half so the two
/// halves of the composite key have no shared low-order structure.
#[inline]
pub(in crate::registry) fn fmix64(mut h: u64) -> u64 {
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58476d1ce4e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d049bb133111eb);
    h ^= h >> 31;
    h
}

/// GC/STW hooks for VM-registered native carrier threads.
///
/// Some native subsystems spawn host OS threads that are registered in the VM
/// thread list, but do not own a full [`NativeContext`] while they sit in an
/// OS wait primitive. Those threads still need to publish their OS tid for
/// cross-thread diagnostics/takeover and must enter the GC-blocked population
/// before parking so a stop-the-world pause does not wait for a thread that
/// cannot reach an interpreter safepoint.
pub trait NativeThreadBlocker: Send + Sync {
    /// Publish the current OS thread id for this VM thread.
    fn publish_os_tid(&self);

    /// Enter a GC-blocked native wait region.
    fn enter_blocked(&self);

    /// Leave a GC-blocked native wait region.
    fn leave_blocked(&self);

    /// gcj j2/cm2: the thread's loop has ended -- retire it from the VM now, on
    /// this thread, through the blocked-region protocol a dying platform thread
    /// uses, instead of leaving it counted by every stop-the-world census until
    /// some later native call drains its death queue
    /// (`NativeContext::unregister_native_thread`). Called once, by the thread
    /// itself, after its last [`Self::leave_blocked`]: from here it runs no
    /// Java and reaches no safepoint.
    ///
    /// `true`: the VM marked the thread dead (gcj j3/rt3 residual 5: on every
    /// backend; this is the only death path of such a thread -- the
    /// process-wide dead queues are deleted). The default does nothing and
    /// answers `false`.
    fn retire(&self) -> bool {
        false
    }
}

/// Transport-only mirror of `vm::runtime::offload::SerializedResult`'s
/// scalar variants, returned by [`NativeContext::gpu_future_take_result`].
///
/// `native-api` cannot depend on `vm` (the dependency runs the other
/// way — `vm`'s `NativeContextImpl` implements this crate's
/// `NativeContext` trait), so a completed GPU submission's
/// `SerializedResult` can't cross the trait boundary as-is. This enum is
/// the narrow subset `gpu_future_take_result` needs to hand back: the
/// four scalar-reduction shapes a `)I`/`)J`/`)F`/`)D`-returning kernel
/// produces, plus `Void` for a kernel with no return value (or one that
/// wrote its result into a caller-owned array instead — see the trait
/// method's doc comment). Primitive-array *future results* are
/// deliberately not represented here: today `finalize_submission` never
/// stamps a `SerializedResult::PrimitiveArray*` into a completed
/// submission (array outputs are delivered via writeback into the
/// caller's own array), so there is nothing for this enum to carry for
/// that case.
/// Element kind for a GPU-resident [`GpuFutureResult::DeviceArray`] result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceArrayElem {
    I32,
    I64,
    F32,
    F64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GpuFutureResult {
    /// The kernel had a void return, or wrote its result into a
    /// caller-owned output array via writeback — nothing new to hand
    /// back through the future's result slot.
    Void,
    /// Scalar-return accumulator readback (`)I` descriptor).
    ScalarI32(i32),
    /// Scalar-return accumulator readback (`)J` descriptor).
    ScalarI64(i64),
    /// Scalar-return accumulator readback (`)F` descriptor).
    ScalarF32(f32),
    /// Scalar-return accumulator readback (`)D` descriptor).
    ScalarF64(f64),
    /// Device-resident array output produced by a GPU submission.
    DeviceArray {
        array_handle: u64,
        elem: DeviceArrayElem,
        len: usize,
    },
}

/// Why a GPU submission failed, recorded at the point of failure.
///
/// `craton.gpu` documents three `GpuException` subclasses for targeted
/// `catch` clauses, and until this enum existed the only way to pick one
/// was to match substrings against the driver's error string on the Java
/// side — a heuristic that misfires on any wording nobody anticipated.
/// Stamping the category where the failure actually happens makes the
/// classification exact.
///
/// The discriminants are wire values: they are returned verbatim by
/// `Native.futureErrorKind` and MUST stay in step with the
/// `NativeBridge.ERROR_KIND_*` constants on the Java side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(i32)]
pub enum GpuErrorKind {
    /// Not classified, or a failure none of the categories below fit.
    /// The Java side falls back to message matching on this value.
    #[default]
    Unknown = 0,
    /// The kernel could not be produced or loaded: the class would not
    /// resolve, the method was rejected by the analyzer, PTX would not
    /// compile, or the module loaded without the expected entry point.
    Compile = 1,
    /// A device-side allocation failed.
    OutOfMemory = 2,
    /// The kernel was launched, or a device operation was attempted, and
    /// the driver reported a failure.
    Launch = 3,
}

impl GpuErrorKind {
    /// The wire value handed to Java by `Native.futureErrorKind`.
    #[must_use]
    pub fn as_i32(self) -> i32 {
        self as i32
    }

    /// Recover a kind from its wire value; anything unrecognised
    /// degrades to [`GpuErrorKind::Unknown`] rather than panicking, so a
    /// newer Java side talking to an older VM still gets a usable answer.
    #[must_use]
    pub fn from_i32(v: i32) -> Self {
        match v {
            1 => Self::Compile,
            2 => Self::OutOfMemory,
            3 => Self::Launch,
            _ => Self::Unknown,
        }
    }
}

/// [`NativeContext::gpu_future_await`] result: this VM has no timed wait.
///
/// Zero, deliberately — it is the value a context that does not override
/// the method returns, and it tells the caller to fall back to polling.
/// Mirrors `NativeBridge.AWAIT_UNSUPPORTED` on the Java side.
pub const GPU_AWAIT_UNSUPPORTED: i32 = 0;
/// [`NativeContext::gpu_future_await`] result: the submission completed.
pub const GPU_AWAIT_COMPLETED: i32 = 1;
/// [`NativeContext::gpu_future_await`] result: the timeout elapsed first.
pub const GPU_AWAIT_TIMED_OUT: i32 = 2;

/// zt pl4 item 5: the exception HotSpot's `PerfMemory::attach` throws for a
/// foreign VM it cannot map ([`NativeContext::attach_foreign_perf_data_hotspot`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForeignPerfAttachFailure {
    /// `IllegalArgumentException` with this message.
    IllegalArgument(String),
    /// `IOException` with this message.
    Io(String),
    /// `OutOfMemoryError` with this message.
    OutOfMemory(String),
}

/// One `GarbageCollectorMXBean` as the running collector describes it, for
/// [`NativeContext::gc_collectors`]. gen r4w2/obs (2026-09-23).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcCollectorInfo {
    /// `getName()` — e.g. HotSpot Serial's `Copy` / `MarkSweepCompact`.
    pub name: &'static str,
    /// `getCollectionCount()`.
    pub count: u64,
    /// `getCollectionTime()`, milliseconds.
    pub time_ms: u64,
    /// `getMemoryPoolNames()`; each is a [`GcMemoryPoolInfo::name`].
    pub pool_names: &'static [&'static str],
}

/// One heap `MemoryPoolMXBean` as the running collector describes it, for
/// [`NativeContext::gc_memory_pools`]. Byte counts; `-1` is the JMM's
/// "undefined". gen r4w2/obs (2026-09-23).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcMemoryPoolInfo {
    /// `getName()` — e.g. `Eden Space`.
    pub name: &'static str,
    /// `getUsage()`: `init`, `used`, `committed`, `max`.
    pub init: i64,
    pub used: i64,
    pub committed: i64,
    pub max: i64,
    /// `getCollectionUsage().getUsed()` — occupancy right after the most recent
    /// collection; `-1` before the first one.
    pub collection_used: i64,
    /// `getCollectionUsage().getCommitted()` -- the pool's committed size at
    /// the end of the last collection that AFFECTED it (its own manager's);
    /// `-1` before any such collection, when HotSpot's last collection usage
    /// is still `(init, 0, 0, max)`. gci i14/mp finding 3.
    pub collection_committed: i64,
    /// `getMemoryManagerNames()`; each is a [`GcCollectorInfo::name`].
    pub manager_names: &'static [&'static str],
}

/// HotSpot 25's NON-heap `MemoryPoolMXBean`s, in its `MemoryService::_pools_list`
/// order (the code heaps are added by `codeCache_init`, the two metaspace pools
/// by `universe_post_init`, both before the heap's pools). gci i15/nh
/// (`gci-i14mp-memory-pool-and-gc-mxbean-review-findings-FIXED-20261016` item 7): the
/// JMX natives report them ahead of the collector's heap pools, and the VM keeps
/// their `getPeakUsage()` under these names.
pub const JMX_NON_HEAP_POOL_NAMES: &[&str] = &[
    "CodeHeap 'non-nmethods'",
    "CodeHeap 'profiled nmethods'",
    "CodeHeap 'non-profiled nmethods'",
    "Metaspace",
    "Compressed Class Space",
];

/// zt t30/nh: HotSpot 25's non-heap pools with an unsegmented code cache
/// (`-XX:-SegmentedCodeCache`, or `ReservedCodeCacheSize` below 240M): ONE
/// code heap, `CodeCache`, then the two metaspace pools.
pub const JMX_UNSEGMENTED_NON_HEAP_POOL_NAMES: &[&str] =
    &["CodeCache", "Metaspace", "Compressed Class Space"];

/// zt t30/nh: every name a non-heap pool bean can have (either layout), for
/// lookups by a bean's own name.
pub const JMX_ANY_NON_HEAP_POOL_NAMES: &[&str] = &[
    "CodeHeap 'non-nmethods'",
    "CodeHeap 'profiled nmethods'",
    "CodeHeap 'non-profiled nmethods'",
    "Metaspace",
    "Compressed Class Space",
    "CodeCache",
];

/// zt t30/nh: the non-heap pools a VM lists, in `_pools_list` order:
/// [`JMX_NON_HEAP_POOL_NAMES`], or [`JMX_UNSEGMENTED_NON_HEAP_POOL_NAMES`]
/// when `layout` is an unsegmented code cache.
pub fn jmx_non_heap_pool_names(
    layout: Option<&cratonvm_types::code_cache_layout::CodeCacheLayout>,
) -> &'static [&'static str] {
    match layout {
        Some(l) if !l.segmented => JMX_UNSEGMENTED_NON_HEAP_POOL_NAMES,
        _ => JMX_NON_HEAP_POOL_NAMES,
    }
}

/// HotSpot 25's non-collecting `MemoryManagerMXBean`s, in `_managers_list`
/// order (ahead of the collectors): the code cache's and the metaspace's.
/// gci i15/nh.
pub const JMX_NON_HEAP_MANAGER_NAMES: &[&str] = &["CodeCacheManager", "Metaspace Manager"];

/// One pool's `MemoryUsage` at an edge of a collection (a `GcInfo`'s
/// `getMemoryUsageBeforeGc()` / `getMemoryUsageAfterGc()` entry). Byte counts;
/// `-1` is the JMM's "undefined". gen r4w3/obs2 (2026-09-23).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcPoolUsageSnapshot {
    /// A [`GcMemoryPoolInfo::name`].
    pub name: &'static str,
    pub init: i64,
    pub used: i64,
    pub committed: i64,
    pub max: i64,
}

/// One completed collection as a `GarbageCollectorMXBean` reports it: the
/// `com.sun.management.GcInfo` plus the fields of the
/// `GarbageCollectionNotificationInfo` HotSpot sends for it.
/// gen r4w3/obs2 (2026-09-23); see [`NativeSystemAccess::gc_last_collection`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcNotificationInfo {
    /// `getGcName()` — a [`GcCollectorInfo::name`].
    pub gc_name: &'static str,
    /// `getGcAction()` — `end of minor GC` / `end of major GC`.
    pub gc_action: &'static str,
    /// `getGcCause()` — HotSpot's `GCCause` spelling.
    pub gc_cause: &'static str,
    /// `GcInfo.getId()` — the bean's collection count including this one.
    pub id: u64,
    /// `GcInfo.getStartTime()` / `getEndTime()` are milliseconds since VM
    /// start; the context converts from these with the same origin
    /// `RuntimeMXBean.getUptime()` uses.
    pub start: std::time::Instant,
    pub end: std::time::Instant,
    /// `Notification.getTimeStamp()` — wall-clock milliseconds.
    pub timestamp_ms: i64,
    pub before: Vec<GcPoolUsageSnapshot>,
    pub after: Vec<GcPoolUsageSnapshot>,
}

/// zs s2/ob: what ZGC's `jdk.GCConfiguration` / `jdk.GCHeapConfiguration`
/// periodic events state on HotSpot 25 that the VM knows exactly (see
/// [`NativeSystemAccess::jfr_zgc_configuration`]). Byte sizes; the thread
/// counts are `None` unless the command line gave them (HotSpot's ergonomic
/// figures depend on its own worker pools, which this VM does not have).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JfrZgcConfiguration {
    /// `MinHeapSize` (`-Xms`, else `min(8M, InitialHeapSize)`).
    pub min_heap: i64,
    /// `InitialHeapSize`.
    pub initial_heap: i64,
    /// `MaxHeapSize`.
    pub max_heap: i64,
    /// An explicit `-XX:ParallelGCThreads`.
    pub parallel_gc_threads: Option<u32>,
    /// An explicit `-XX:ConcGCThreads`.
    pub conc_gc_threads: Option<u32>,
    /// `UseDynamicNumberOfGCThreads` (HotSpot's default `true`).
    pub dynamic_gc_threads: bool,
    /// `-XX:+DisableExplicitGC`.
    pub explicit_gc_disabled: bool,
    /// `-XX:+ExplicitGCInvokesConcurrent`.
    pub explicit_gc_concurrent: bool,
}

/// Opaque reference to a GC-updated slot owned by a [`NativeHandleScope`].
///
/// The fallback is intentionally private. Lightweight mock contexts do not
/// implement a moving heap and therefore use it when their default
/// [`NativeContext::handle_get`] returns `None`; production VM contexts always
/// read the current address through `slot`. Native implementations cannot
/// extract either value, so they cannot accidentally keep using a pre-GC raw
/// reference or reinterpret a slot index as a heap address.
#[derive(Debug)]
pub struct NativeHandle {
    pub(in crate::registry) slot: u32,
    pub(in crate::registry) fallback: ObjectRef,
}

/// Early-return- and panic-safe native root scope.
///
/// Construct this before retaining any object across an allocating or
/// re-entrant VM call. Every object rooted through [`Self::root`] remains in
/// the executing thread's collector-visible handle table until this guard is
/// dropped. [`Drop`] closes the scope on every Rust exit path, eliminating the
/// manually paired `handle_scope_push`/`handle_scope_pop` discipline.
///
/// `DerefMut<Target = dyn NativeContext>` lets existing native code call VM
/// capabilities through the scope while the roots are active:
///
/// ```ignore
/// let mut scope = NativeHandleScope::new(ctx);
/// let receiver = scope.root(receiver);
/// let array = scope.new_array(ArrayElementType::Char, len); // may collect
/// let receiver = scope.get(&receiver); // always the current address
/// scope.set_field(receiver, 0, Value::Object(Some(array)));
/// // scope closes automatically, including on `?` or `return`.
/// ```
pub struct NativeHandleScope<'a> {
    pub(in crate::registry) context: &'a mut dyn NativeContext,
}

impl<'a> NativeHandleScope<'a> {
    /// Open a nested scope on `context`.
    pub fn new(context: &'a mut dyn NativeContext) -> Self {
        context.handle_scope_push();
        Self { context }
    }

    /// Root `object` and return an opaque handle that can only be resolved
    /// through this scope.
    pub fn root(&mut self, object: ObjectRef) -> NativeHandle {
        NativeHandle {
            slot: self.context.handle_root(object),
            fallback: object,
        }
    }

    /// Resolve `handle` to its current post-GC address.
    pub fn get(&self, handle: &NativeHandle) -> ObjectRef {
        self.context
            .handle_get(handle.slot)
            .unwrap_or(handle.fallback)
    }
}

impl<'a> std::ops::Deref for NativeHandleScope<'a> {
    type Target = dyn NativeContext + 'a;

    fn deref(&self) -> &Self::Target {
        self.context
    }
}

impl<'a> std::ops::DerefMut for NativeHandleScope<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.context
    }
}

impl Drop for NativeHandleScope<'_> {
    fn drop(&mut self) {
        self.context.handle_scope_pop();
    }
}

/// A JVMTI redefinition the VM refused, as `java.lang.instrument` reports it
/// (interpreter round i1 wave 27, lane L3): the throwable libinstrument raises
/// for the refusal on HotSpot, and the VM's own account of it for the log.
/// See [`NativeClassAccess::check_class_redefinition`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstrumentRefusal {
    /// Internal name of the throwable's class, e.g.
    /// `java/lang/UnsupportedOperationException`.
    pub throwable: &'static str,
    /// Its message; `None` constructs it without one.
    pub message: Option<&'static str>,
    /// Why the VM refused, in its own words (not shown to Java).
    pub detail: String,
}

/// Trait providing VM capabilities needed by native method implementations.
///
/// The `Vm` struct implements this trait. Using a trait here avoids circular
/// module dependencies between `native` and `vm`.
pub trait NativeClassAccess {
    /// Capability boundary: Class identity, metadata, loading, resources, and modules.

    /// Load a class by name. Returns the ClassId.
    fn load_class(&mut self, name: &str) -> MethodCallResult;

    /// [`Self::load_class`] for the flat-store step of the base
    /// `ClassLoader.loadClass` native (`classloader_real::load_class_visible_to`),
    /// whose receiver IS the loader being asked. The VM's `load_class` drives
    /// the calling frame's defining loader after a global miss (a native's
    /// "calling class", right for `Class.forName` / JNI `FindClass`); HotSpot's
    /// `loadClass` asks only its receiver and the receiver's parents, so this
    /// lookup does not (`ClassRealm::load_class_asks_receiver_only`; interpreter
    /// round i2 wave 53, lane L5). Default: [`Self::load_class`].
    fn load_class_for_loader_receiver(&mut self, name: &str) -> MethodCallResult {
        self.load_class(name)
    }

    /// Get the class name for a ClassId.
    ///
    /// Allocates. Prefer [`Self::class_name_arc_of_id`] unless an owned
    /// `String` is genuinely what the caller needs.
    fn class_name_of_id(&self, class_id: ClassId) -> Option<String>;

    /// Whether an application/library workaround of `kind` is registered for
    /// `class_name` in this VM's [`NativeMethodRegistry`].
    fn has_app_workaround(&self, _kind: AppWorkaroundKind, _class_name: &str) -> bool {
        false
    }

    /// HotSpot's Class-mirror source-file cache for stack-trace decoding.
    /// A present source initializes the cache only once; an unavailable old
    /// version clears it. This does not alter JVMTI's current class metadata.
    fn stack_trace_source_file(&self, _class_id: ClassId, captured: Option<&str>)
        -> Option<std::sync::Arc<str>> {
        captured.map(std::sync::Arc::from)
    }

    /// Retain the interned decoded filename as the Class mirror does. The
    /// caller holds a native handle over `string`; this call cannot allocate
    /// Java objects. Implementations must trace/forward it and forget it when
    /// the cache is cleared or the declaring class unloads.
    fn retain_stack_trace_source_file(&self, _class_id: ClassId, _text: &str, _string: ObjectRef) {}

    /// The `SourceFile` attribute of `class_id`, or `None` when the class
    /// declares none (or is not loaded).
    ///
    /// Added so a `StackWalker` frame carrier can store a `ClassId` and build
    /// its file name only if someone asks: `StackFrame.getFileName()` is read
    /// for roughly one frame of a `filter(..).findFirst()` walk, and building
    /// it eagerly cost a Java `String` allocation for every frame on the stack.
    /// Default `None` so the many test `NativeContext` mocks are unaffected —
    /// a carrier that gets `None` here answers `null`, exactly as one built
    /// from a frame with no source file always did.
    fn class_source_file(&self, class_id: ClassId) -> Option<String> {
        let _ = class_id;
        None
    }

    /// The class name for a `ClassId`, **without allocating**.
    ///
    /// The VM stores a class's name as an `Arc<str>` and
    /// [`Self::class_name_of_id`] copies it into a fresh `String` on every
    /// call. That copy is pure waste at the overwhelmingly common shape —
    /// `ctx.class_name_arc_of_id(cid).as_deref() == Some("java/util/HashMap")`
    /// — where the name is compared and dropped. This hands back a clone of the
    /// `Arc` instead: one refcount increment, no allocation, no copy. And
    /// `Option<Arc<str>>::as_deref()` yields exactly the `Option<&str>` the old
    /// shape did, so converting a call site is a drop-in the compiler checks.
    ///
    /// `native-collections` measured what the `String` costs: its receiver
    /// classification paid 5-6 of these per `HashMap.put`, which is part of the
    /// 21.2x row against JDK 25 C2 that its `RECEIVER_FACTS` / `RECEIVER_NAMES`
    /// memos exist to remove. Those memos also remove the `class_manager` read
    /// lock, which this does NOT — a caller on a path hot enough to care about
    /// the lock still wants a memo, and this is not a substitute for one.
    ///
    /// Default-implemented in terms of `class_name_of_id` so every existing
    /// `NativeContext` (six test mocks among them) keeps compiling unchanged;
    /// the VM overrides it with an `Arc::clone`.
    fn class_name_arc_of_id(&self, class_id: ClassId) -> Option<Arc<str>> {
        self.class_name_of_id(class_id).map(Arc::from)
    }

    /// Get the class id of a heap object.
    fn class_id_of_object(&self, obj: ObjectRef) -> ClassId;

    /// True when the named class is loaded as a synthetic stub (no real
    /// `.class` bytes). Used to branch native helpers that must mirror JDK
    /// behaviour without registering natives that would override real JDK
    /// bytecode once the stub upgrades.
    /// Would a name-based load of `name` be answered with a *fabricated
    /// synthetic stub* (a class with no `Code` on any method, minted only so
    /// enterprise bytecode can link) rather than a real class?
    ///
    /// Non-destructive: unlike [`Self::is_class_synthetic_stub`] it does not
    /// load anything, so it can be used to decide whether to consult a real
    /// `ClassLoader` *before* the stub is minted and registered globally.
    /// Default `false` for contexts with no class manager.
    fn would_fabricate_synthetic_stub(&self, name: &str) -> bool {
        let _ = name;
        false
    }

    fn is_class_synthetic_stub(&self, class_name: &str) -> bool {
        false
    }

    /// Did real class bytes back `class_id` (`ClassOrigin::has_real_bytes`:
    /// boot image, class path, a user loader's `defineClass`, a hidden
    /// class)? `false` for the VM-created and fabricated origins (arrays,
    /// generated lambdas / proxies / accessors, `VmInternal`,
    /// `CompatibilityStub`). Decided by provenance, never by name. A lookup
    /// that never loads. Default `true` for contexts with no class store (the
    /// mocks model classes defined from bytes). JIT round 21 wave 14, lane
    /// innerclasses (the spoofed-member check asks it).
    fn class_has_real_bytes(&self, class_id: ClassId) -> bool {
        let _ = class_id;
        true
    }

    /// Check if a method exists in a class (searches the class hierarchy).
    /// Returns `true` if the method is found.
    fn method_exists(&self, class_name: &str, method_name: &str, descriptor: &str) -> bool;

    /// True iff `class_id` ITSELF declares (not merely inherits) a method with
    /// the given name+descriptor. Unlike [`method_exists`], this does NOT walk
    /// superclasses — it answers "does this exact class override the method?".
    /// Used by the `ClassLoader.getResources` native to distinguish a custom
    /// loader that overrides `findResources` (delegate to it) from one that
    /// merely inherits the default (fall back to parent-delegated scan).
    /// Default returns `false` so mock/test contexts compile unchanged.
    fn class_declares_method(&self, _class_id: ClassId, _name: &str, _descriptor: &str) -> bool {
        false
    }

    /// The access flags of the method `class_id` ITSELF declares with this
    /// name and descriptor, `None` when it declares none (superclasses are not
    /// walked: constructors are not inherited). `Class.newInstance`'s
    /// nullary-constructor lookup and access check (round 15 wave 4, lane
    /// jdkonly6; JO5-5). The default reads [`Self::declared_methods`]; the VM
    /// answers from the method table without copying it.
    fn declared_method_access_flags(
        &self,
        class_id: ClassId,
        name: &str,
        descriptor: &str,
    ) -> Option<u16> {
        self.declared_methods(class_id)
            .into_iter()
            .find(|m| m.name == name && m.descriptor == descriptor)
            .map(|m| m.access_flags)
    }

    /// Ensure a class is loaded and initialized. Returns the ClassId.
    fn ensure_class_initialized(
        &mut self,
        name: &str,
    ) -> Result<ClassId, cratonvm_types::error::MethodCallFailed>;

    /// Ensure the exact already-resolved class id is initialized without
    /// re-resolving its binary name through the global loader map.
    fn ensure_class_initialized_with_class_id(
        &mut self,
        class_id: ClassId,
    ) -> Result<(), cratonvm_types::error::MethodCallFailed> {
        if let Some(name) = self.class_name_of_id(class_id) {
            self.ensure_class_initialized(&name)?;
        }
        Ok(())
    }

    /// Link `class_id` before reflection answers for it, where the VM's
    /// policy says so (HotSpot's `JVM_GetClassDeclaredMethods` / `Fields` /
    /// `Constructors` call `link_class` first; `vm::link_class_for_reflection`,
    /// interpreter round i1 wave 17). Returns the class's linkage error.
    /// Default: nothing, for contexts with no class manager.
    ///
    /// The VM may COLLECT here when the heap refuses the class's preparation
    /// (gci i22/oo): call it only while holding no heap reference you read
    /// again afterwards, the `reclaim_before_alloc_retry` precondition.
    fn link_class_for_reflection(
        &mut self,
        class_id: ClassId,
    ) -> Result<(), cratonvm_types::error::MethodCallFailed> {
        let _ = class_id;
        Ok(())
    }

    /// Run the JDK's `defineOrCheckPackage` step for an application
    /// class-path class a door hands out without linking it
    /// (`Class.forName(n, false, loader)`): HotSpot runs it at the class's
    /// define, so a sealing violation is the load's `SecurityException`
    /// (`vm::check_class_path_package_unlinked`, interpreter round i4 wave
    /// 92, lane L5). Default: nothing, for contexts with no class manager.
    fn check_class_path_package_unlinked(
        &mut self,
        class_id: ClassId,
    ) -> Result<(), cratonvm_types::error::MethodCallFailed> {
        let _ = class_id;
        Ok(())
    }

    /// The same package step for the other doors that hand out an
    /// application class-path class without linking it (the `--compatible`
    /// `ClassLoader.loadClass` natives; an array class is checked through its
    /// bottom element class), behind `CRATONVM_UNLINKED_DOORS_CHECK_PACKAGE`
    /// instead of the `forName` switch (`vm::check_class_path_package_unlinked_at_door`,
    /// interpreter round i4 wave 94, lane L5). Default: nothing, for contexts
    /// with no class manager.
    fn check_class_path_package_at_door(
        &mut self,
        class_id: ClassId,
    ) -> Result<(), cratonvm_types::error::MethodCallFailed> {
        let _ = class_id;
        Ok(())
    }

    /// Is `class_id` an unlinked application class-path class whose
    /// `(package, code source)` pair the JDK's `defineOrCheckPackage` refused
    /// (a sealing violation)? HotSpot never defined it, so the
    /// `findLoadedClass` natives answer `null` for it
    /// (`vm::class_path_package_refused`, interpreter round i4 wave 95, lane
    /// L5). Default: `false`, for contexts with no class manager.
    fn class_path_package_refused(&self, class_id: ClassId) -> bool {
        let _ = class_id;
        false
    }

    /// Claim the one `ClassLoader.addClass` record of `class_id` in its
    /// built-in (application or platform) loader's `classes` list: `true`
    /// when the caller is to make the upcall now, `false` when it was made
    /// already (by a Java define door, or by the VM's own record of a class
    /// it defined for that loader, `vm::record_in_builtin_loader`). HotSpot
    /// lists each class once. A class of any other loader is never tracked
    /// (`true`). Interpreter round i7 wave 137, lane L5
    /// (`CRATONVM_BUILTIN_LOADER_ADD_CLASS`). Default: `true`, for contexts
    /// with no class manager (every door records, as before).
    fn claim_builtin_loader_class_record(&self, class_id: ClassId) -> bool {
        let _ = class_id;
        true
    }

    /// Register a lambda proxy synthesized from a *reflective*
    /// `LambdaMetafactory.metafactory` / `altMetafactory` call (as opposed to
    /// the `invokedynamic` opcode, which is handled inline in the interpreter).
    ///
    /// Returns the raw `u32` of a freshly-allocated synthetic proxy `ClassId`
    /// whose lambda call-site metadata is registered in the VM's
    /// `lambda_proxies` table — identical in shape to the metadata produced by
    /// the `invokedynamic` lambda bootstrap, so the interpreter's SAM-dispatch
    /// path (`try_lambda_dispatch`) handles instances of it without any further
    /// special-casing. A factory `MethodHandle` (kind `MH_KIND_LAMBDA_FACTORY`)
    /// later allocates proxy instances of this class once the captured values
    /// are known.
    ///
    /// `impl_ref_kind` is the JVMS `reference_kind` byte (1..=9) of the
    /// implementation method handle. `capture_types` is one type char per
    /// captured value (`'L'`, `'I'`, `'J'`, ...), in factory-argument order.
    ///
    /// Returns `0` when the host cannot register a proxy (e.g. test mocks, or
    /// the proxy table is full); callers treat `0` as "unsupported" and fall
    /// back to a non-null no-op `CallSite`.
    #[allow(clippy::too_many_arguments)]
    fn register_lambda_proxy(
        &mut self,
        functional_interface: &str,
        sam_method_name: &str,
        sam_descriptor: &str,
        impl_class: &str,
        impl_member: &str,
        impl_descriptor: &str,
        impl_ref_kind: u8,
        instantiated_descriptor: &str,
        capture_types: &str,
        // Whether the call site passed `LambdaMetafactory.FLAG_SERIALIZABLE`.
        // The reflective `metafactory` entry point has no flags word and passes
        // `false`; such a lambda can still be serializable by inheritance.
        serializable: bool,
    ) -> u32 {
        let _ = (
            functional_interface,
            sam_method_name,
            sam_descriptor,
            impl_class,
            impl_member,
            impl_descriptor,
            impl_ref_kind,
            instantiated_descriptor,
            capture_types,
            serializable,
        );
        0
    }

    /// Record the `LambdaMetafactory.altMetafactory` MARKER INTERFACES of a
    /// proxy just returned by `register_lambda_proxy`.
    ///
    /// `altMetafactory`'s `FLAG_MARKERS` (0x2) block names interfaces the spun
    /// proxy implements IN ADDITION to its functional interface, so
    /// `marker.isInstance(lambda)` and a `checkcast` to the marker must both
    /// succeed. They are not part of the SAM metadata `register_lambda_proxy`
    /// carries, so they are handed over separately, right after it.
    ///
    /// `proxy_class_id` is the raw `u32` that `register_lambda_proxy` returned
    /// (`0` means it declined — nothing to record). `marker_interfaces` are
    /// internal names (`java/lang/Cloneable`). No-op by default: a host with no
    /// lambda-proxy table (test mocks) has nowhere to put them.
    fn register_lambda_proxy_markers(&mut self, proxy_class_id: u32, marker_interfaces: &[String]) {
        let _ = (proxy_class_id, marker_interfaces);
    }

    /// Record the `LambdaMetafactory.altMetafactory` BRIDGE descriptors
    /// (`FLAG_BRIDGES`, 0x4) of a proxy just returned by
    /// `register_lambda_proxy`: extra descriptors of the SAM name the spun
    /// class implements, each forwarding to the SAM. Same hand-off contract as
    /// [`Self::register_lambda_proxy_markers`]; no-op by default.
    fn register_lambda_proxy_bridges(&mut self, proxy_class_id: u32, bridges: &[String]) {
        let _ = (proxy_class_id, bridges);
    }

    /// Record the HOST of a proxy just returned by `register_lambda_proxy`:
    /// the lookup class of the reflective `LambdaMetafactory` call
    /// (`caller.lookupClass()`), which HotSpot names the spun class after,
    /// makes its nest host and defines it in the loader of. The `invokedynamic`
    /// bootstrap records the class holding the instruction the same way. Same
    /// hand-off contract as [`Self::register_lambda_proxy_markers`]; no-op by
    /// default.
    fn register_lambda_proxy_host(&mut self, proxy_class_id: u32, host: ClassId) {
        let _ = (proxy_class_id, host);
    }

    /// Check if child_class is a subclass of parent_class.
    fn is_subclass(&self, child: ClassId, parent: ClassId) -> bool;

    /// Is an instance of `class_id` assignable to `target_class_name`, asked
    /// the LOADER-BLIND way the bytecode asks it?
    ///
    /// This is the third door onto one rule, and it exists for the reason
    /// [`synthetic_implements_declared`] and [`aastore_element_assignable`]
    /// exist: the walk was written once, for `checkcast`/`aastore`, and every
    /// reflective caller that asks a SECOND, narrower question is how two doors
    /// come to disagree about one object.
    ///
    /// [`is_subclass`] is not that question. It compares `ClassId`s, so under a
    /// forked loader it refuses a value whose class chain names the target
    /// under a different id — and while it does walk the interface DAG, it
    /// walks the one recorded on the value's own class, which for a
    /// `@CompileWithForkedClassLoader` copy can name the OTHER loader's
    /// interface. `ClassManager::is_assignable_to_name` compares NAMES over
    /// supers and interfaces transitively, is cycle-safe and depth-capped, and
    /// is what `typecheck::aastore_element_assignable` already uses for exactly
    /// this case.
    ///
    /// # `None` is the honest answer, and callers must treat it as "allow"
    ///
    /// `None` means this context cannot answer — the default for a host with no
    /// VM hierarchy behind it (mocks, the fabricated-class harnesses). A caller
    /// that turns a `false` into a refusal must leave `None` alone, or a test
    /// mock starts throwing `ClassCastException` at correct code.
    ///
    /// **This does NOT carry the proxy hatches.** A `java.lang.reflect.Proxy`
    /// instance and a lambda proxy acquire their interfaces at RUNTIME,
    /// invisibly to any static walk, and `aastore_element_assignable` has a
    /// separate block for each. A caller that refuses on `Some(false)` must
    /// apply those itself.
    ///
    /// [`synthetic_implements_declared`]: Self::synthetic_implements_declared
    /// [`aastore_element_assignable`]: Self::aastore_element_assignable
    /// [`is_subclass`]: Self::is_subclass
    fn class_assignable_to_name(&self, class_id: ClassId, target_class_name: &str) -> Option<bool> {
        let _ = (class_id, target_class_name);
        None
    }

    /// THIS VM's redefinitions stamped in `(after, upto]` on the process
    /// redefinition clock (`cratonvm_classloading::class_redefinition_count`):
    /// `visit` sees the class of each (a class may be seen more than once).
    ///
    /// * `Some(true)`: answered; nothing visited means this VM redefined
    ///   nothing in the span, whatever other VMs did.
    /// * `Some(false)`: this VM's log cannot answer (lapped, or a record that
    ///   named no class); the caller treats it as it treats the process
    ///   ring's `false`.
    /// * `None` (the default; mocks and other embeddings): no per-VM log --
    ///   ask the process ring (`cratonvm_classloading::for_each_redefined_class_between`),
    ///   which this crate cannot call itself.
    ///
    /// nio21 wave 6 L1
    /// (`r21-stackwalker-frame-lines-blank-on-another-vms-redefinition-FIXED-20261012.md`):
    /// a `ClassId` is per class store, so the process ring names another VM's
    /// class under the same id, and the `StackFrameInfo` readers blanked the
    /// line and file of a frame whose class THIS VM never redefined. The VM
    /// answers from its own `RedefinitionLog` while the thread's invoke cache
    /// is bound to it (`CRATONVM_INVOKE_CACHE_PER_VM_REDEFINITIONS`), as the
    /// obsolete-frame passes do.
    fn redefined_classes_between(
        &self,
        after: u64,
        upto: u64,
        visit: &mut dyn FnMut(ClassId),
    ) -> Option<bool> {
        let _ = (after, upto, visit);
        None
    }

    /// Would `checkcast target_desc` admit the array object `value`?
    /// `target_desc` is an array descriptor (`[I`, `[Ljava/lang/String;`,
    /// `[[I`). The VM answers with the interpreter's
    /// `typecheck::array_is_assignable_to`, `checkcast`'s own predicate
    /// (strict in every mode since i10-L2: an `Object[]` is no `T[]`).
    /// `None`: this context cannot answer (mocks), `value` is not an array, or
    /// a refusal would rest on a component name that several loaders define
    /// (the predicate resolves names without a loader). Callers refuse only on
    /// `Some(false)`.
    fn array_value_casts_to_desc(&self, value: ObjectRef, target_desc: &str) -> Option<bool> {
        let _ = (value, target_desc);
        None
    }

    /// Does the VM's `checkcast`/`instanceof` admit an instance of `class_id`
    /// as a `target_class_name` on a relationship that is **declared** rather
    /// than present in the loaded class hierarchy?
    ///
    /// This is the reflection door onto the interpreter's
    /// `typecheck::synthetic_implements` — the table that says a
    /// `cratonvm/internal/SystemLogger` is a `System.Logger` and a
    /// `cratonvm/internal/foreign/MemorySegmentImpl` is both a `MemorySegment`
    /// and an `AbstractMemorySegmentImpl`. Those classes are concrete stand-ins
    /// the VM mints itself, so the relationship exists nowhere a hierarchy walk
    /// can find it.
    ///
    /// It exists for the same reason [`aastore_element_assignable`] does: the
    /// rule was already written once, for the bytecode, and reflection asking a
    /// SECOND, narrower question is how the two doors come to disagree about
    /// one object. Measured, before this: `probes/FfmVectorSegmentProbe.java`'s
    /// IDENTITY section reported `abstractBase=false` for a segment that a
    /// `checkcast jdk/internal/foreign/AbstractMemorySegmentImpl` three frames
    /// away had just admitted.
    ///
    /// `false` by default: a host with no interpreter behind it (the test
    /// mocks) has no table to consult, and failing closed leaves the caller's
    /// own hierarchy checks as the only answer — which is what it had before
    /// this method existed.
    ///
    /// [`aastore_element_assignable`]: Self::aastore_element_assignable
    fn synthetic_implements_declared(&self, class_id: ClassId, target_class_name: &str) -> bool {
        let _ = (class_id, target_class_name);
        false
    }

    /// Get the superclass ClassId. Returns None for java/lang/Object.
    fn superclass_of(&self, class_id: ClassId) -> Option<ClassId>;

    /// JVMS §aastore covariance: may the non-null `value` be stored into the
    /// reference array `array`? `None` when this context cannot answer.
    ///
    /// This exists so `java.lang.reflect.Array.set` can enforce **the same rule
    /// as the `aastore` bytecode**, which is what HotSpot does — they are one
    /// check there, and were two here. The VM's implementation
    /// (`vm::runtime::interpreter::typecheck::aastore_element_assignable`) is
    /// already shared by the interpreter opcode and the JIT's `jit_aastore`
    /// helper; a reflective store is simply its third caller.
    ///
    /// The difference is not academic. That predicate is deliberately
    /// *additive*: it must never produce a FALSE `ArrayStoreException`, so it
    /// fails open for an interface component, for a `$Proxy`/`AnnotationProxy`
    /// value, for a synthetic class id, and for a same-named component that
    /// resolved to a different `ClassId` under another loader. Every one of
    /// those hedges was paid for by a real regression. A `ClassId`-identity
    /// check reproduces none of them.
    ///
    /// `None` (the default) means "no VM hierarchy here" — mocks and the
    /// fabricated-class harnesses — and leaves the caller on its own exact
    /// check rather than silently widening it.
    fn aastore_element_assignable(&self, array: ObjectRef, value: ObjectRef) -> Option<bool> {
        let _ = (array, value);
        None
    }

    /// JVMS §6.5 `instanceof` of the ARRAY object `array` against the ARRAY
    /// class `target_array_class`, decided on class identity, component by
    /// component -- the verdict the `instanceof` / `checkcast` bytecodes use
    /// under `--real-jdk`, where a same-named component of another loader is
    /// a different class.
    ///
    /// `None` (the default) when the context cannot decide: mocks, and a
    /// target that records no component (a primitive-component array class).
    fn array_instance_of_array_class(
        &self,
        array: ObjectRef,
        target_array_class: ClassId,
    ) -> Option<bool> {
        let _ = (array, target_array_class);
        None
    }

    /// [`Self::array_instance_of_array_class`] for two array CLASSES: is the
    /// array class `src_array_class` assignable to `target_array_class`
    /// (`Class.isAssignableFrom`), decided on component identity. `None` (the
    /// default) when the context cannot decide.
    fn array_class_assignable_to_array_class(
        &self,
        src_array_class: ClassId,
        target_array_class: ClassId,
    ) -> Option<bool> {
        let _ = (src_array_class, target_array_class);
        None
    }

    /// Get the ClassId for a loaded class by name. Returns None if not loaded.
    ///
    /// # `None` is two answers
    ///
    /// It means *either* "no loader has a class under this name" *or* "several
    /// distinct classes do, and with no initiating loader to disambiguate
    /// there is no correct one to return". Those demand opposite actions from
    /// a caller that reacts to a miss by loading or fabricating, so a caller
    /// that does anything other than give up should ask
    /// [`Self::classify_class_name`] instead.
    fn class_id_by_name(&self, name: &str) -> Option<ClassId>;

    /// `java/lang/Object`'s id (JIT r23 wave 3c, lane L5): what
    /// `class_id_by_name("java/lang/Object")` answers, without the name lookup
    /// a hot native (`System.arraycopy`'s reference path asks it per call) paid
    /// per call. The VM's context answers from its per-VM cache
    /// (`vm_exec::object_class_id`); the default is the name lookup.
    fn object_class_id(&self) -> Option<ClassId> {
        self.class_id_by_name("java/lang/Object")
    }

    /// The loader-aware form of [`Self::class_id_by_name`]: tells *why* there
    /// is no unique answer.
    ///
    /// Consult this — rather than the `Option` — from any native that would
    /// **act** on a miss (load the class, mint a stand-in, fall back to a
    /// same-named class of its own). Acting on
    /// [`NameLookup::Ambiguous`] adds one more class to a name that already
    /// has too many, and the new one usually outranks the others.
    ///
    /// # Default implementation
    ///
    /// Delegates to [`Self::class_id_by_name`], so a context that cannot tell
    /// the two apart keeps answering exactly what it answers today (`Absent`
    /// for both). That is the honest default for mocks and non-VM contexts:
    /// they have no name index and no ambiguity to report. The VM's own
    /// context overrides it against the class manager's `classify_loaded_name`,
    /// which is O(1) against the definition-count index.
    fn classify_class_name(&self, name: &str) -> NameLookup {
        match self.class_id_by_name(name) {
            Some(id) => NameLookup::Unique(id),
            None => NameLookup::Absent,
        }
    }

    /// Whether `class_id`'s static initializer has already run to completion.
    ///
    /// This is the query behind `Unsafe.shouldBeInitialized` /
    /// `shouldBeInitialized0`, which must answer "does this class still need
    /// initializing?". Before this existed, those natives returned a flat
    /// `false` ("everything is already initialized") because the VM's real
    /// `is_class_initialized_*` helpers live in the `vm` crate and were not
    /// reachable from `native-builtins` — a constant that is right for an
    /// initialized class and silently wrong for every other one.
    ///
    /// Deliberately distinct from `ensure_class_initialized`, which ACTS:
    /// calling that to answer the question would run the very `<clinit>` the
    /// caller is asking about, which is precisely what `Unsafe`'s callers are
    /// trying to avoid.
    ///
    /// The default returns `true` so implementations that do not track
    /// initialization state keep the previous observable behaviour instead of
    /// suddenly reporting every class as uninitialized.
    fn is_class_initialized(&self, _class_id: ClassId) -> bool {
        true
    }

    /// Resolve `name` to a `ClassId`, preferring whichever loaded class is
    /// registered under the SAME classloader as `near`'s own declaring
    /// context, falling back to the normal global (bootstrap-first) search
    /// used by [`Self::class_id_by_name`].
    ///
    /// A plain by-name lookup can silently resolve to an unrelated
    /// same-named class loaded under a DIFFERENT classloader when the JVM
    /// spec's (loader, name) identity legitimately produces two distinct
    /// classes with the same name -- e.g. Hibernate ORM's bytecode
    /// enhancement reloads an `@EmbeddedId` class under its own private
    /// ByteBuddy classloader. Resolving a field/parameter's declared type
    /// via plain name search can then find the FIRST-loaded (often stale)
    /// variant instead of the one the caller's own class actually sees,
    /// causing a real, correctly-typed value to be rejected as an
    /// assignability mismatch. Use this instead of `class_id_by_name`
    /// whenever `name` is a symbolic reference that must match the specific
    /// class variant visible to a known class (`near`) -- e.g. a
    /// `Field`/`Method`/`Constructor`'s own declaring class.
    fn class_id_by_name_near(&self, name: &str, _near: ClassId) -> Option<ClassId> {
        self.class_id_by_name(name)
    }

    /// The class a `CONSTANT_Class` reference to `name` in the class
    /// `referencing` resolves to, from what the VM already knows -- the
    /// class's own loader's definitions, its direct supertypes, and the names
    /// that loader INITIATED (delegated to another loader, a user-defined
    /// parent included) -- without loading anything; `None` when only running
    /// the loader could say. The VM answers with the compile doors' reader
    /// (`constants.rs` `class_resolved_without_loading`). The default is
    /// [`Self::class_id_by_name_near`], which does not see a delegated name.
    /// JIT round 17 wave 6, lane mh6 (the MethodHandle doors' loader view,
    /// `r17-mh-door-loader-residuals-after-mh3-FIXED-20261006.md` item 2).
    fn class_id_seen_by_class_without_loading(
        &self,
        name: &str,
        referencing: ClassId,
    ) -> Option<ClassId> {
        self.class_id_by_name_near(name, referencing)
    }

    /// HotSpot's `IllegalAccessError` message when the class `referencing`
    /// may not access the class a `CONSTANT_Class` reference to `name` (an
    /// array name: its bottom element) resolves to -- the JVMS §5.4.4 check
    /// every class-constant resolution runs -- or `None` when it may, or when
    /// the class is not known through `referencing`'s loader yet (no loading).
    /// The VM answers with its constant-pool door's check
    /// (`constants.rs` `class_constant_access_denial_in`); the default is
    /// `None`. JIT round 17 wave 6, lane mh6 (the MethodHandle site link's
    /// access check).
    fn class_access_denial_for_name(&self, _referencing: ClassId, _name: &str) -> Option<String> {
        None
    }

    /// Resolve `name` the way a requester-less parent-delegation loader
    /// would: bootstrap, then extension, then application. Do not collapse
    /// onto an unrelated user-defined loader's private same-named class unless
    /// it is the only possible answer.
    ///
    /// Use this for lookups performed on behalf of ordinary application code
    /// when no precise requesting class is available. Prefer
    /// [`Self::class_id_by_name_near`] whenever a requester is known.
    fn class_id_by_name_delegated(&self, name: &str) -> Option<ClassId> {
        self.class_id_by_name(name)
    }

    /// Resolve `name` to a `ClassId`, LOADING it through
    /// `referencing_class_id`'s own defining classloader if it isn't loaded
    /// yet -- exactly as a bytecode instruction (`new`/`checkcast`/
    /// `invokestatic`/...) referencing `name` FROM `referencing_class_id`
    /// would (JVMS SS5.4.3 initiating-loader semantics).
    ///
    /// Unlike [`Self::class_id_by_name_near`]/[`Self::class_id_by_name`] --
    /// pure lookups that only succeed once `name` has already been
    /// resolved/indexed under that loader -- this drives the loader's own
    /// `loadClass`/`defineClass` on a miss, so it also answers correctly the
    /// very first time a class is needed under a given loader (the gap that
    /// made two prior lookup-based fix attempts for the H2 `Parser`
    /// loader-collapse bug regress on a fresh session -- see
    /// bug-h2-suite-residual-fail-triage-FIXED.md's
    /// eighth-pass section).
    ///
    /// Native overrides that construct or invoke-special a DIFFERENT class
    /// than their own receiver's declaring class (an app/H2 native bridging
    /// into the receiver's own package -- e.g. `SessionLocal.prepareLocal`'s
    /// `new Parser(this)`) MUST use this instead of
    /// `new_object_initialized`/`invoke_special` with a bare name: those
    /// collapse to whichever loader defined `name` FIRST process-wide,
    /// silently constructing/invoking the WRONG loader's copy of the class
    /// whenever the receiver's own defining loader is a user-defined one
    /// distinct from the first-loaded (usually Application) copy.
    ///
    /// The default implementation ignores `referencing_class_id` and falls
    /// back to the name-only [`Self::ensure_class_initialized`] -- sufficient
    /// for test mocks and any context with a single (global) loader
    /// namespace; the real VM implementation honours per-loader identity.
    fn class_id_by_name_via_referencing_class(
        &mut self,
        _referencing_class_id: ClassId,
        name: &str,
    ) -> Result<ClassId, cratonvm_types::error::MethodCallFailed> {
        self.ensure_class_initialized(name)
    }

    /// For a synthetic lambda-proxy `ClassId` (created by `register_lambda_proxy`,
    /// class id `>= 0x8000_0000`, not in the class store), return the internal
    /// name of its functional (SAM) interface. Returns `None` for any non-lambda
    /// class. Used by reflection natives so a lambda's mirror reports a sane
    /// hierarchy (`getSuperclass()` = Object, `getInterfaces()` = [SAM]) instead
    /// of null/empty — Gradle's listener type-walk calls
    /// `concreteClass.getSuperclass().isInterface()` and a null superclass NPEs.
    fn lambda_functional_interface(&self, _class_id: ClassId) -> Option<String> {
        None
    }

    /// For a synthetic lambda-proxy `ClassId`, return the resolved `ClassId`
    /// of its functional (SAM) interface when the invokedynamic bootstrap
    /// recorded one.  This preserves defining-loader identity for reflection
    /// paths that must not re-resolve a potentially ambiguous interface name.
    fn lambda_functional_interface_id(&self, _class_id: ClassId) -> Option<ClassId> {
        None
    }

    /// For a synthetic lambda-proxy `ClassId`, return the internal name of the
    /// lambda's *defining* class (the class that owns the implementation method,
    /// e.g. `Refl6` for a `() -> {}` whose body compiles to `Refl6.lambda$..`).
    /// Returns `None` for any non-lambda class. Used by the reflection name
    /// natives (`getName`/`getSimpleName`/`getNestHost`) to synthesize the
    /// HotSpot-style `<host>$$Lambda/0x<id>` name instead of `unknown_<id>`.
    fn lambda_proxy_host(&self, _class_id: ClassId) -> Option<String> {
        None
    }

    /// [`Self::lambda_proxy_host`] as the recorded defining class's id, with
    /// no by-name lookup (a name two loaders define is ambiguous by name).
    /// `None` when the VM recorded no defining class for the proxy.
    /// Interpreter round i7 wave 142, lane L3. The default answers `None`.
    fn lambda_proxy_host_id(&self, _class_id: ClassId) -> Option<ClassId> {
        None
    }

    /// For a synthetic lambda-proxy `ClassId`, return the full lambda
    /// call-site metadata required to serialize and later reconstruct the
    /// lambda (see [`LambdaSerialMetadata`]). Returns `None` for any
    /// non-lambda class. Used by the object-serialization natives to emit a
    /// `SerializedLambda`-equivalent record instead of attempting to serialize
    /// the (un-loadable) synthetic `$$Lambda` proxy class by name.
    fn lambda_proxy_serial_metadata(&self, _class_id: ClassId) -> Option<LambdaSerialMetadata> {
        None
    }

    /// The VM's one record of the lambda proxy `class_id` (see
    /// [`LambdaProxyRecord`]), or `None` for any other class. Interpreter
    /// round i6 wave 118, lane L3. The default answers `None`.
    fn lambda_proxy_record(&self, _class_id: ClassId) -> Option<LambdaProxyRecord> {
        None
    }

    /// How this lambda proxy is (or is not) `java.io.Serializable`, per the
    /// JDK's own `LambdaMetafactory` rule. `NotSerializable` for any class that
    /// is not a lambda proxy.
    ///
    /// Real HotSpot spins a private `writeReplace()` and adds `Serializable` to
    /// the proxy's interfaces only for serializable lambdas, so every reflective
    /// `Class` surface must agree with this rather than assume all lambdas are
    /// serializable (which is what CratonVM did before 2026-08-01).
    fn lambda_proxy_serializability(&self, _class_id: ClassId) -> LambdaSerializability {
        LambdaSerializability::NotSerializable
    }

    /// Get the ClassLoaderId for a loaded class.
    /// Returns 0 = Bootstrap, 1 = Extension, 2 = Application, 3+ = UserDefined(id).
    fn loader_id_of_class(&self, class_id: ClassId) -> i32;

    /// [`Self::loader_id_of_class`] only when the VM KNOWS the class's
    /// defining loader (same encoding), `None` otherwise -- where
    /// `loader_id_of_class` answers "application" for a class whose loader
    /// it does not know. JIT round 16 wave 13 (lane wmte; proposal MH16-1 of
    /// `jit-r16-mh-proposals-RETIRED-20261006.md`). The default knows nothing.
    fn loader_id_of_class_definite(&self, _class_id: ClassId) -> Option<i32> {
        None
    }

    /// Check if a class is a record (has Record attribute, JEP 395).
    fn is_record_class(&self, class_id: ClassId) -> bool;

    /// Get the record components (name, descriptor) for a record class.
    fn record_components(&self, class_id: ClassId) -> Vec<(String, String)>;

    /// The `index`th record component's `RuntimeVisibleAnnotations` and
    /// `RuntimeVisibleTypeAnnotations` bodies (indexing the class's own
    /// constant pool), which HotSpot writes into `RecordComponent.annotations`
    /// / `typeAnnotations`. Interpreter round i2 wave 58, lane L3. The default
    /// keeps none.
    fn record_component_annotation_bytes(
        &self,
        class_id: ClassId,
        index: usize,
    ) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        let _ = (class_id, index);
        (None, None)
    }

    /// The `index`th record component's OWN `Signature` attribute (the one
    /// inside the Record attribute), which HotSpot writes into
    /// `RecordComponent.signature`. Interpreter round i5 wave 101, lane L3.
    /// The default keeps none.
    fn record_component_signature(&self, class_id: ClassId, index: usize) -> Option<String> {
        let _ = (class_id, index);
        None
    }

    /// `hashCode`/`equals` fast-path metadata for `class_id`, resolved in a
    /// single class-manager lock acquisition:
    ///
    /// * `.0` — a bitmask describing how instances can be hashed/compared
    ///   without a Java dispatch:
    ///   * bit 0 — declares the **javac-generated** (JEP 395) `hashCode()I`
    ///   * bit 1 — declares the generated `equals(Ljava/lang/Object;)Z`
    ///   * bit 2 — declares the generated `toString()Ljava/lang/String;`
    ///   * bit 3 — instances have identity `hashCode`/`equals` semantics
    ///   * bit 4 — the class is `java.lang.String`
    ///   * bit 5 — the class is exactly `java.util.ArrayList`
    ///   * bit 6 — the class is one of the eight boxed primitives
    ///     (`java.lang.Integer` .. `java.lang.Double`); `.1` is then the slot
    ///     of its instance `value` field, not `first_field_index`
    ///     (interpreter round i7 wave 135, lane L3)
    ///   * bit 7 — with bit 6, the class is `java.lang.Boolean`
    ///
    ///   The record bits are `0` for a non-record and for any of the three a
    ///   record hand-writes — an explicit override must keep its own
    ///   semantics. Bit 3 is set only for enum classes, where JLS §8.9 makes
    ///   `Enum.hashCode`/`Enum.equals` final and identity-based. Bits 3-5 are
    ///   mutually exclusive with the record bits and with each other.
    /// * `.1` — the field slot of the first record component, i.e. the class's
    ///   `first_field_index`. Always `0` for a real record (neither
    ///   `java.lang.Record` nor `java.lang.Object` declares an instance
    ///   field), but read rather than assumed.
    /// * `.2` — the number of record components, `0` for a non-record.
    ///
    /// Consulted once per component of every nested record by the direct
    /// record `hashCode`/`equals` implementations. Defaults to
    /// `(0, 0, 0)` — "no fast path" — for the test doubles.
    fn object_method_fast_path(&self, class_id: ClassId) -> (u8, usize, usize) {
        let _ = class_id;
        (0, 0, 0)
    }

    /// Check if a class is sealed (has PermittedSubclasses attribute, JEP 409).
    fn is_sealed_class(&self, class_id: ClassId) -> bool;

    /// Get the permitted subclass names for a sealed class.
    fn permitted_subclasses(&self, class_id: ClassId) -> Vec<String>;

    /// Get the total number of instance fields (including inherited) for a
    /// loaded class.  Returns 0 if the class isn't loaded.  Used by native
    /// allocators that otherwise hard-code a synthetic field count — in
    /// real-JDK mode the hard-coded count often underestimates the real
    /// layout, and allocating with too few slots causes out-of-bounds
    /// `get_field` / `set_field` later when bytecode accesses an inherited
    /// field at `first_field_index + local_offset`.
    fn class_num_total_fields(&self, class_id: ClassId) -> usize {
        let _ = class_id;
        0
    }

    // -- Reflection metadata methods --

    /// Get metadata for all fields declared in this class (not inherited).
    fn declared_fields(&self, class_id: ClassId) -> Vec<FieldMetadata>;

    /// Get metadata for all methods declared in this class (not inherited).
    fn declared_methods(&self, class_id: ClassId) -> Vec<MethodMetadata>;

    /// [`Self::declared_methods`] restricted to the methods named `name`, and
    /// whether the class declares ANY method at all (the "the class was
    /// enumerated" evidence a resolution verdict needs). Interpreter round i5
    /// wave 100, lane L4 (`CRATONVM_MHN_RESOLVE_NAMED_METHODS`): a
    /// `MethodHandleNatives.resolve` of one method copied every declared
    /// method's metadata (four strings each, and its attributes decoded)
    /// three times per class on its path. The default filters
    /// [`Self::declared_methods`]; the VM builds metadata for the matches only.
    fn declared_methods_named(&self, class_id: ClassId, name: &str) -> (Vec<MethodMetadata>, bool) {
        let all = self.declared_methods(class_id);
        let any = !all.is_empty();
        (all.into_iter().filter(|m| m.name == name).collect(), any)
    }

    /// [`Self::declared_fields`] restricted to the fields named `name`, and
    /// whether the class declares ANY field at all. Interpreter round i5 wave
    /// 102, lane L4 (`CRATONVM_MHN_RESOLVE_NAMED_FIELDS`): the field half of
    /// [`Self::declared_methods_named`]. Slot indices are those
    /// [`Self::declared_fields`] answers. The default filters
    /// [`Self::declared_fields`]; the VM builds metadata for the matches only.
    fn declared_fields_named(&self, class_id: ClassId, name: &str) -> (Vec<FieldMetadata>, bool) {
        let all = self.declared_fields(class_id);
        let any = !all.is_empty();
        (all.into_iter().filter(|f| f.name == name).collect(), any)
    }

    /// Get the ClassIds of directly implemented/extended interfaces.
    fn class_interfaces(&self, class_id: ClassId) -> Vec<ClassId>;

    /// Get the raw access_flags bits for a class.
    fn class_access_flags(&self, class_id: ClassId) -> u16;

    /// Get or create a Class mirror for a primitive type (e.g. "int", "boolean").
    fn primitive_class_mirror(&mut self, name: &str) -> ObjectRef;

    // -- Annotation support --

    /// Get runtime-visible annotation type descriptors for a class.
    /// Returns a list of (type_descriptor, element_value_pairs) tuples.
    fn class_annotations(&self, class_id: ClassId) -> Vec<AnnotationData>;

    /// Get runtime-visible annotation type descriptors for a method.
    /// `method_name` and `method_desc` identify the method within the class.
    fn method_annotations(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Vec<AnnotationData>;

    /// Get runtime-visible annotation type descriptors for a field.
    /// `field_name` identifies the field within the class.
    fn field_annotations(&self, class_id: ClassId, field_name: &str) -> Vec<AnnotationData>;

    /// Get parameter annotations for a method.
    /// Returns a Vec of Vec<AnnotationData>, one per parameter.
    fn method_parameter_annotations(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Vec<Vec<AnnotationData>>;

    /// Get the runtime-visible TYPE_USE annotations that target a method's
    /// return type (JVMS 4.7.20 `target_type` 0x14, METHOD_RETURN) with an
    /// empty `type_path` (i.e. annotations placed directly on the top-level
    /// return type rather than a nested array/type-argument component).
    ///
    /// Backs `Method.getAnnotatedReturnType().getDeclaredAnnotations()` so
    /// JSpecify-style `@Nullable`/`@NonNull` (which are TYPE_USE-only and thus
    /// live in `RuntimeVisibleTypeAnnotations`, not `RuntimeVisibleAnnotations`)
    /// are surfaced to reflection. Default impl returns an empty `Vec` so mock
    /// `NativeContext` implementations don't need to plumb the attribute store.
    fn method_return_type_annotations(
        &self,
        _class_id: ClassId,
        _method_name: &str,
        _method_desc: &str,
    ) -> Vec<AnnotationData> {
        Vec::new()
    }

    /// Get the runtime-visible TYPE_USE annotations that target a method return
    /// type's TYPE ARGUMENTS, at any nesting depth (JVMS 4.7.20 `target_type`
    /// 0x14, METHOD_RETURN, with a `type_path` made entirely of TYPE_ARGUMENT
    /// entries) -- e.g. `List<@NotBlank String> getNames()`, or nested generics
    /// like `ValueExtractor<Wrapper<@Foo ?>>`.
    ///
    /// The outer `Vec` is indexed by the top-level `type_argument_index`
    /// (0-based, per JVMS 4.7.20.2); each entry's own `children` carries the
    /// next nesting level. Default impl returns an empty `Vec`.
    fn method_return_type_argument_annotations(
        &self,
        _class_id: ClassId,
        _method_name: &str,
        _method_desc: &str,
    ) -> Vec<TypeArgAnnotations> {
        Vec::new()
    }

    /// Get the runtime-visible TYPE_USE annotations that target a method's
    /// formal parameters (JVMS 4.7.20 `target_type` 0x16,
    /// METHOD_FORMAL_PARAMETER) with an empty `type_path`. The outer `Vec` is
    /// indexed by `formal_parameter_index`; entries with no annotations are
    /// empty inner `Vec`s.
    ///
    /// Backs `Parameter.getAnnotatedType().getDeclaredAnnotations()`. Default
    /// impl returns an empty `Vec`.
    fn method_parameter_type_annotations(
        &self,
        _class_id: ClassId,
        _method_name: &str,
        _method_desc: &str,
    ) -> Vec<Vec<AnnotationData>> {
        Vec::new()
    }

    /// Get the runtime-visible TYPE_USE annotations that target a field's type
    /// (JVMS 4.7.20 `target_type` 0x13, FIELD) with an empty `type_path`.
    /// Backs `Field.getAnnotatedType().getDeclaredAnnotations()`. Default impl
    /// returns an empty `Vec`.
    fn field_type_annotations(&self, _class_id: ClassId, _field_name: &str) -> Vec<AnnotationData> {
        Vec::new()
    }

    /// Get the runtime-visible TYPE_USE annotations that target a field type's
    /// TYPE ARGUMENTS, at any nesting depth (JVMS 4.7.20 `target_type` 0x13,
    /// FIELD, with a `type_path` made entirely of TYPE_ARGUMENT entries) --
    /// e.g. `List<@NotBlank String> names`.
    ///
    /// The outer `Vec` is indexed by the top-level `type_argument_index`
    /// (0-based, per JVMS 4.7.20.2); each entry's own `children` carries the
    /// next nesting level. Default impl returns an empty `Vec`.
    fn field_type_argument_annotations(
        &self,
        _class_id: ClassId,
        _field_name: &str,
    ) -> Vec<TypeArgAnnotations> {
        Vec::new()
    }

    /// Get the runtime-visible TYPE_USE annotations that target a method
    /// formal parameter's type ARGUMENTS, at any nesting depth (JVMS 4.7.20
    /// `target_type` 0x16, METHOD_FORMAL_PARAMETER, with a `type_path` made
    /// entirely of TYPE_ARGUMENT entries) -- e.g. the `@Valid` in
    /// `List<@Valid Person> persons`, which annotates the type argument
    /// `Person`, not the top-level `List` parameter type.
    ///
    /// The outer `Vec` is indexed by `formal_parameter_index`; the inner
    /// `Vec` is indexed by the top-level `type_argument_index` (0-based, per
    /// JVMS 4.7.20.2), and each entry's own `children` carries the next
    /// nesting level. Backs
    /// `((AnnotatedParameterizedType) method.getAnnotatedParameterTypes()[i])
    /// .getAnnotatedActualTypeArguments()[j].getDeclaredAnnotations()`, which
    /// Spring's `HandlerMethod.MethodValidationInitializer
    /// .getContainerElementAnnotations` walks to decide whether e.g.
    /// `addPeople(List<@Valid Person> persons)` needs method validation.
    /// Default impl returns an empty `Vec`.
    fn method_parameter_type_argument_annotations(
        &self,
        _class_id: ClassId,
        _method_name: &str,
        _method_desc: &str,
    ) -> Vec<Vec<TypeArgAnnotations>> {
        Vec::new()
    }

    /// Get the generic Signature attribute for a class (if present).
    fn class_signature(&self, class_id: ClassId) -> Option<String>;

    /// Get the generic Signature attribute for a method (if present).
    fn method_signature(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Option<String>;

    /// Get the generic Signature attribute for a field (if present).
    fn field_signature(&self, class_id: ClassId, field_name: &str) -> Option<String>;

    /// WP2.1 — Get the parsed `MethodParameters` attribute (JVMS 4.7.24)
    /// for a method. Each entry is `(name, access_flags)`. The name is the
    /// resolved Utf8 from the constant pool, or empty string if
    /// `name_index == 0` (synthetic / unnamed parameter).
    ///
    /// Returns an empty `Vec` if the method has no `MethodParameters`
    /// attribute (the common case for code not compiled with `-parameters`),
    /// or if the class / method cannot be located. Callers should fall
    /// back to synthesizing `arg0`, `arg1`, … names in that case.
    ///
    /// Default implementation returns an empty `Vec` so that mock
    /// `NativeContext` implementations don't need to plumb through
    /// the class-file attribute store.
    fn method_parameters(
        &self,
        _class_id: ClassId,
        _method_name: &str,
        _method_desc: &str,
    ) -> Vec<(String, u16)> {
        Vec::new()
    }

    /// The `MethodParameters` attribute (JVMS 4.7.24) of a method as
    /// HotSpot's `JVM_GetMethodParameters` reads it (interpreter round i5
    /// wave 101, lane L3): `None` when the method has no attribute, otherwise
    /// one row per ENTRY (an attribute with zero entries is `Some(vec![])`,
    /// not "absent"), each name classified by [`MethodParameterName`] so the
    /// native can raise HotSpot's `IllegalArgumentException` for a bad
    /// `name_index` and keep an empty name distinct from no name.
    ///
    /// Default: derived from [`Self::method_parameters`] (an empty answer is
    /// "absent", an empty name is "unnamed"), so mock contexts keep their
    /// answer.
    fn method_parameters_attribute(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Option<Vec<(MethodParameterName, u16)>> {
        let rows = self.method_parameters(class_id, method_name, method_desc);
        if rows.is_empty() {
            return None;
        }
        Some(
            rows.into_iter()
                .map(|(name, flags)| {
                    let name = if name.is_empty() {
                        MethodParameterName::Unnamed
                    } else {
                        MethodParameterName::Name(name)
                    };
                    (name, flags)
                })
                .collect(),
        )
    }

    /// Get annotation default values for annotation type methods.
    /// Returns the default ElementValue for the given method, if any.
    fn method_annotation_default(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Option<AnnotationElementValue>;

    /// WP2.1 — Get the list of checked-exception class internal names from
    /// a method's `Exceptions` attribute (JVMS 4.7.5).
    ///
    /// Returns an empty `Vec` if the method has no `Exceptions` attribute
    /// (no `throws` clause), or if the class / method cannot be located.
    /// Each entry is a binary internal class name like
    /// `"java/io/IOException"`.
    ///
    /// Default implementation returns an empty `Vec` so that mock
    /// `NativeContext` implementations don't need to plumb through the
    /// class-file attribute store.
    fn method_exceptions(
        &self,
        _class_id: ClassId,
        _method_name: &str,
        _method_desc: &str,
    ) -> Vec<String> {
        Vec::new()
    }

    // -- JPMS Module support (N3) --

    /// Return the module name (JPMS) of the class with the given ClassId.
    ///
    /// Returns `None` for classes in the unnamed module.
    fn module_name_of_class(&self, class_id: ClassId) -> Option<String>;

    /// The module the class with the given ClassId is a member of, by
    /// IDENTITY: a module of a non-boot `ModuleLayer` defined to a
    /// user-defined loader (`--real-jdk`; the module `Class.getModule()`
    /// answers, which [`Self::module_name_of_class`] does not see) is named
    /// together with its loader, and every other class is what
    /// [`Self::module_name_of_class`] answers. The one question a native asks
    /// when it names or compares a class's module (interpreter round i1 wave
    /// 45, lane L5;
    /// `i44-L5-proposal-a-classs-module-is-an-identity-every-reader-sees`).
    /// Default: [`Self::module_name_of_class`]'s answer.
    fn module_identity_of_class(&self, class_id: ClassId) -> ClassModuleIdentity {
        match self.module_name_of_class(class_id) {
            Some(name) if !name.is_empty() => ClassModuleIdentity::Named(name),
            _ => ClassModuleIdentity::Unnamed,
        }
    }

    /// The module of `class_id` as HotSpot's `Klass::class_in_module_of_loader`
    /// spells it in a message: `module m`, `module m@v` when
    /// `ModuleEntry::should_show_version()` holds (a module-path or non-boot
    /// layer module with a version, never a JDK module of the image), or
    /// `unnamed module`. Interpreter round i4 wave 99, lane L5. Default:
    /// [`ClassModuleIdentity::describe`] (a layer module's version included).
    fn module_clause_of_class(&self, class_id: ClassId) -> String {
        self.module_identity_of_class(class_id).describe_with_version()
    }

    /// The one module question ([`ModuleQuestion`]) about the modules of
    /// `accessor` and `target`, by the modules' identities, from the VM's
    /// module registry: `Some(answer)`, or `None` when the registry cannot
    /// answer it (a layer module's opens live in the JDK's `Module` fields; a
    /// class the VM does not know) and the caller must ask the JDK's `Module`
    /// methods. Interpreter round i1 wave 47, lane L6
    /// (`i45-L5-proposal-one-module-access-question-by-identity`). Default:
    /// `None`.
    fn module_access(
        &self,
        _accessor: ClassId,
        _target: ClassId,
        _question: ModuleQuestion,
    ) -> Option<bool> {
        None
    }

    /// Find a classpath resource by name. Returns raw bytes or None if not found.
    ///
    /// Searches bootstrap → extension → application classpaths.
    /// The name should be a forward-slash-separated path (leading `/` is stripped).
    fn find_resource(&self, name: &str) -> Option<Vec<u8>>;

    /// Return the raw bytes of the class file from which `class_id` was
    /// loaded (or last redefined). Reads from `ClassManager::class_bytes_cache`,
    /// which is populated by every `define_class_with_options` call.
    /// Used by `Instrumentation.retransformClasses` to seed the transformer
    /// chain with the true source bytes — `find_resource` would only find
    /// classpath-resident classes, missing dynamically-defined and hidden
    /// classes. Default impl returns `None` so test mocks compile.
    fn class_bytes(&self, _class_id: ClassId) -> Option<Vec<u8>> {
        None
    }

    /// Do `bytes` fingerprint-match the class file `class_id`'s current
    /// definition came from?
    ///
    /// `None` means the VM recorded no fingerprint and cannot adjudicate.
    /// Used by `Instrumentation.retransformClasses` when `class_bytes` misses
    /// (that cache is size-capped and FIFO-evicted) and the retransformation
    /// base has to be re-read from the classpath: the classpath is searched
    /// bootstrap → extension → application, *not* through the class's defining
    /// loader, so what it returns can be a different build of the same name.
    /// Matching `this_class` does not catch that; this does. Default impl
    /// returns `None` so test mocks compile.
    fn class_bytes_match_base(&self, _class_id: ClassId, _bytes: &[u8]) -> Option<bool> {
        None
    }

    /// Normalize parser-stored flags in a resource/cache retransformation
    /// base when no agent-owned byte-for-byte base was saved.
    fn normalize_retransformation_bytes(&self, _class_id: ClassId, bytes: Vec<u8>) -> Vec<u8> {
        bytes
    }

    /// A class file written back from the live class `class_id`, for
    /// `Instrumentation.retransformClasses` when no retransformation base
    /// survives in the class-bytes cache, on the class path or behind the
    /// defining loader (a generated class whose base the cache evicted), as
    /// HotSpot reconstitutes one. `None` when the live class may not be its
    /// base (it was redefined) or nothing could be written. Default `None`
    /// (interpreter round i1 wave 29, lane L3).
    fn reconstituted_class_bytes(&self, _class_id: ClassId) -> Option<Vec<u8>> {
        None
    }

    /// Return a URL string (e.g. `file:/...` or `jar:file:/...!/...`) for
    /// every classpath entry that contains a resource with the given name.
    /// Used by `ClassLoader.getResources` / `getSystemResources`.
    /// Default implementation returns an empty vector so test mocks compile.
    fn find_all_resource_urls(&self, name: &str) -> Vec<String> {
        let _ = name;
        Vec::new()
    }

    /// The first `jrt:` URL [`Self::find_all_resource_urls`] lists for
    /// `name`. The VM override asks only the class-path entries that can name
    /// a `jrt:` resource (the runtime image, a JDK `jmods/<module>.jmod`);
    /// this default filters the whole list, so the two answer alike.
    /// Interpreter round i3 wave 73, lane L5
    /// (`CRATONVM_PLATFORM_RESOURCE_JRT_WALK`).
    fn first_runtime_image_resource_url(&self, name: &str) -> Option<String> {
        self.find_all_resource_urls(name)
            .into_iter()
            .find(|url| url.starts_with("jrt:"))
    }

    /// The incremental form of [`Self::find_all_resource_urls`]: the next
    /// matching URL at or after the cursor `(segment, index)`, plus the cursor
    /// to resume at.
    ///
    /// Enumerating from `(0, 0)` to exhaustion yields exactly what
    /// `find_all_resource_urls` yields, in the same order. It exists so that a
    /// consumer which stops early stops the classpath SCAN early — the JDK's
    /// `getResources` enumeration is lazy per element, and
    /// `resources(name).anyMatch(..)` is the common shape that depends on it.
    ///
    /// Only valid for names [`Self::resource_name_supports_incremental_scan`]
    /// accepts. Default returns `None` so test mocks compile.
    fn next_resource_url(
        &self,
        name: &str,
        segment: u32,
        index: u32,
    ) -> Option<(String, u32, u32)> {
        let _ = (name, segment, index);
        None
    }

    /// [`Self::find_all_resource_urls`] restricted to ONE class-path segment:
    /// 0 bootstrap, 1 extension, 2 application — the numbering
    /// [`Self::next_resource_url`] already uses.
    ///
    /// Exists for `ClassLoader.getDefinedPackage`, which does NOT delegate: the
    /// application loader must answer `null` for `java.lang`, which lives on
    /// segment 0. The default falls back to the unsegmented probe so that an
    /// implementation which has not overridden it behaves exactly as before
    /// rather than silently reporting "nothing is visible".
    fn find_resource_urls_in_segment(&self, name: &str, segment: u8) -> Vec<String> {
        let _ = segment;
        self.find_all_resource_urls(name)
    }

    /// `ModuleReader.find(name)` of resolved `--module-path` module `module`:
    /// `None` when `module` is not a module-path module of this VM;
    /// `Some(None)` when none of its roots holds `name`; `Some(Some(url))`
    /// with the URL the application class path spells for the first root
    /// (patch paths, then the module root) that holds it. Interpreter round
    /// i4 wave 98, lane L3. Default `None` so test mocks compile.
    fn module_path_resource_url(&self, module: &str, name: &str) -> Option<Option<String>> {
        let _ = (module, name);
        None
    }

    /// The bytes sibling of [`Self::module_path_resource_url`] (same `None`
    /// / `Some(None)` meaning). Default `None` so test mocks compile.
    fn module_path_resource_bytes(&self, module: &str, name: &str) -> Option<Option<Vec<u8>>> {
        let _ = (module, name);
        None
    }

    /// `true` when [`Self::next_resource_url`] can serve `name` — i.e. the
    /// name matches at most one entry per classpath entry, so "the first URL
    /// this entry serves" cannot be dropping others. Default `false`.
    fn resource_name_supports_incremental_scan(&self, name: &str) -> bool {
        let _ = name;
        false
    }

    /// Return the raw bytes of every classpath entry that contains a
    /// resource with the given name. Parallel to [`find_all_resource_urls`]
    /// but returns content rather than URLs — used by Rust-native resource
    /// enumeration paths (e.g. `ServiceLoader` provider discovery in
    /// `native-builtins/src/service_loader.rs`) that bypass the JDK's
    /// `URL.openStream` / `BufferedReader` chain.
    /// Default implementation returns an empty vector so test mocks compile.
    fn find_all_resource_bytes(&self, name: &str) -> Vec<Vec<u8>> {
        let _ = name;
        Vec::new()
    }

    /// Find the filesystem path of the classpath entry that holds a given
    /// class (for `Class.getProtectionDomain().getCodeSource().getLocation()`).
    /// Returns a `file:`-scheme-ready absolute path (directory has trailing
    /// slash, JAR is a plain path).  Returns `None` for classes loaded from
    /// jimage (bootstrap JDK) or if the class cannot be found on any path.
    fn find_class_source_path(&self, class_name: &str) -> Option<String> {
        let _ = class_name;
        None
    }

    /// SBR5 `clload`: may the application loader define `internal` natively, with the JDK's
    /// `findClassOnClassPathOrNull` bytecode unable to observe a difference (single-entry
    /// package, unsigned, unsealed, no manifest `Class-Path`, startup class path)? Default
    /// `false`: a context with no class path answers "run the bytecode".
    fn class_path_fast_define_ok(&self, internal: &str) -> bool {
        let _ = internal;
        false
    }

    /// SBR5 `clload`: is `internal` provably on no entry of the application class path (so the
    /// JDK's `URLClassPath` lookup can only answer "not found")? Default `false`.
    fn class_path_fast_absent_ok(&self, internal: &str) -> bool {
        let _ = internal;
        false
    }

    /// Take one of the `Error("Circular loading of URL stream handler
    /// providers detected")`s JDK 25's lazy application `URLClassPath` throws
    /// while opening this class path (`ClassPath::apply_url_class_path_model`):
    /// `true` once per error. Interpreter round i5 wave 108, lane L5. Default
    /// `false`.
    fn class_path_take_provider_circularity_error(&self) -> bool {
        false
    }

    /// Does opening the application class path run a URL stream handler
    /// provider lookup (a manifest `Class-Path` token with a scheme other than
    /// `file`, `jar` and `jrt`), so that the JDK's lazy `URLClassPath` and not
    /// the VM's eager one must answer the application loader? Wave 108, lane
    /// L5. Default `false`.
    fn class_path_provider_lookup_sensitive(&self) -> bool {
        false
    }

    /// The application class path's own parse of the archive at `path`
    /// (`cratonvm_classloading::ClassPath::outer_archive_for`: its bytes and
    /// a clone of its central directory, when the file still has the stamp
    /// it had when the class path loaded it), type-erased because the type
    /// lives in `classloading`; `native-builtins` downcasts it to
    /// `cratonvm_classloading::OuterArchive`. For the nested-jar indexes
    /// under `CRATONVM_NESTED_JAR_CLASS_PATH_OUTER` (interpreter round i2
    /// wave 69, lane L5). Default `None` (no class path: open the file).
    fn class_path_outer_archive(
        &self,
        path: &std::path::Path,
    ) -> Option<Box<dyn std::any::Any + Send + Sync>> {
        let _ = path;
        None
    }

    /// The bytes of the FILE entry `name` of the jar at `path`, read straight
    /// from the application class path's own entry for that jar
    /// (`cratonvm_classloading::ClassPath::jar_entry_bytes_matching`: its
    /// mapping or copy, its parse, its index, its load-time stamp compared
    /// with the file now), served only under the plain-view and CRC/size
    /// rules of the jar-loader bytes rows. For the jar-loader bytes index
    /// under `CRATONVM_JAR_LOADER_CLASS_PATH_DIRECT` (interpreter round i3
    /// wave 71, lane L5). Default [`ClassPathJarBytes::NotHeld`] (no class
    /// path: ask as before).
    fn class_path_jar_entry_bytes(
        &self,
        path: &std::path::Path,
        name: &str,
        crc32: u32,
        size: u64,
    ) -> ClassPathJarBytes {
        let _ = (path, name, crc32, size);
        ClassPathJarBytes::NotHeld
    }

    /// Return the CodeSource URL attached to a loaded class — what
    /// `Class.getProtectionDomain().getCodeSource().getLocation()` returns.
    /// This is populated at class-load time from the classpath entry that
    /// produced the class, and surfaces real JAR/dir URLs (e.g.
    /// `file:/opt/app.jar`) rather than the synthetic `class:` placeholder.
    /// Returns `None` for synthetic stubs and JDK internals.
    fn class_code_base(&self, class_id: ClassId) -> Option<String> {
        let _ = class_id;
        None
    }

    /// Return the SHA-256 hex digests of every signer certificate block on
    /// the class's CodeSource (one per JAR-signer). Empty vector means an
    /// unsigned source; used by `security_manager` policy enforcement to
    /// match `grant signedBy "..."` entries.
    fn class_code_source_cert_digests(&self, class_id: ClassId) -> Vec<String> {
        let _ = class_id;
        Vec::new()
    }

    /// Return the raw DER-encoded signer certificate blocks (PKCS#7 / CMS
    /// SignedData) attached to the class's CodeSource.  Parallel to
    /// `class_code_source_cert_digests` — one block per JAR-signer — but
    /// exposes the original bytes so the policy engine can parse each
    /// signer's X.509 subject DN for `grant signedBy "CN=..."` matching.
    /// Empty vector means an unsigned source.
    fn class_code_source_certs(&self, class_id: ClassId) -> Vec<Vec<u8>> {
        let _ = class_id;
        Vec::new()
    }

    /// Reverse-lookup: given a `java.lang.Class` mirror object, return the
    /// backing `ClassId` (the class the mirror reflects).  Returns `None`
    /// for primitive-type mirrors and for non-mirror objects.
    ///
    /// Implemented by consulting the VM's `class_mirrors_reverse` map;
    /// avoids encoding the class_id in the mirror's Java-visible fields,
    /// which would clash with real-JDK `java/lang/Class` layout.
    fn class_id_from_mirror(&self, mirror: ObjectRef) -> Option<ClassId> {
        let _ = mirror;
        None
    }

    /// The mirror already minted for `class_id`, or `None` when it has none
    /// yet. Unlike [`Self::get_class_mirror`] it never allocates, so a LEAF
    /// native ([`NativeMethodRegistry::set_leaf`]) may ask. The default answers
    /// `None` (mocks), which makes such a native decline to the funnel.
    fn cached_class_mirror(&self, class_id: ClassId) -> Option<ObjectRef> {
        let _ = class_id;
        None
    }

    /// JIT round 20 wave 19, lane zAssign (`CRATONVM_JIT_CLASS_IS_ASSIGNABLE_INLINE`):
    /// the reflective natives tell the VM that `mirror` reverse-maps to
    /// `class_id` (they just resolved it), so the VM may publish the compiled
    /// `Class.isAssignableFrom` / `isInstance` arms' mirror hint for it.
    /// `kind` is asked only when the VM wants to write a hint: `0` = no hint
    /// (an array or primitive-named class, or no class at the id), `1` = a
    /// hint whose MISSES must be declined (a `synthetic_implements` target
    /// with `CRATONVM_JIT_TYPECHECK_DISPLAY_MISS_ANY_TARGET=0`),
    /// `2` = a hint whose misses may be answered. Never changes an answer;
    /// a no-op by default (the test mocks have no compiled code).
    fn note_class_mirror_id(&self, mirror: ObjectRef, class_id: ClassId, kind: &dyn Fn() -> u8) {
        let _ = (mirror, class_id, kind);
    }

    /// List all class names on the application classpath.
    ///
    /// Returns binary class names (e.g. `com/example/MyClass`).
    fn list_application_class_names(&self) -> Vec<String>;

    /// Dynamically extend the application classpath (for URLClassLoader).
    ///
    /// Each string in `paths` is a filesystem path to either a directory or a
    /// JAR/ZIP file. Paths are appended to the application class finder so that
    /// subsequent `ensure_class_initialized` and `find_resource` calls search them.
    fn register_dynamic_classpath(&mut self, paths: &[String]);

    /// Retract paths previously passed to [`Self::register_dynamic_classpath`].
    ///
    /// The other half of `URLClassLoader.close()`: a closed loader must stop
    /// serving classes and resources it has not already loaded. Classes already
    /// defined stay defined, matching HotSpot — `close()` shuts the loader's
    /// `URLClassPath`, it does not unload anything.
    ///
    /// Returns the number of classpath entries removed. A path handed to more
    /// than one live loader is only retracted when the last of them releases it,
    /// so `0` is a normal answer and not an error.
    ///
    /// The default is `0` ("this context cannot retract"), which is the
    /// behaviour every caller had before the VM implementation existed.
    fn unregister_dynamic_classpath(&mut self, _paths: &[String]) -> usize {
        0
    }

    /// Reset per-thread JMM blocked/waited counters when contention monitoring
    /// is enabled. Contexts without a live VM need no bookkeeping.
    fn reset_thread_jmx_contention_stats(&mut self) {}

    /// Append paths to the BOOTSTRAP class search path so their classes load
    /// with the bootstrap loader (null `Class.getClassLoader()`). Drives
    /// `Instrumentation.appendToBootstrapClassLoaderSearch`. The default
    /// implementation falls back to the application classpath; the VM's
    /// `NativeContext` overrides it to target the bootstrap loader.
    fn register_bootstrap_classpath(&mut self, paths: &[String]) {
        self.register_dynamic_classpath(paths);
    }

    /// Define a new class from raw bytecode (Phase 23.2).
    ///
    /// Parses the class bytes, registers the class with the ClassManager under
    /// the application loader, and returns the ClassId of the newly defined class.
    /// Returns `None` if parsing fails.
    fn define_class_from_bytes(&mut self, name: &str, bytes: &[u8]) -> Option<ClassId>;

    /// NEW-8: Define a hidden class (JEP 371 / JEP 429) from raw bytecode.
    ///
    /// `stored_name` is the mangled name under which the class is
    /// registered in the class store — typically
    /// `"<original>/0x<counter>"` so multiple hidden classes derived
    /// from the same template get distinct names. The class's
    /// `hidden` flag is set atomically with registration so the class
    /// is never visible to `find_class_by_name` / `Class.forName`.
    ///
    /// Returns a typed error string on parse / link failure so the
    /// caller can surface a proper `ClassFormatError` or
    /// `LinkageError` to Java code. The default implementation
    /// delegates to [`define_class_from_bytes`] for backward
    /// compatibility — implementations that want real JEP 371
    /// semantics should override this.
    fn define_hidden_class_from_bytes(
        &mut self,
        stored_name: &str,
        bytes: &[u8],
    ) -> Result<ClassId, String> {
        match self.define_class_from_bytes(stored_name, bytes) {
            Some(cid) => {
                self.set_class_hidden(cid);
                Ok(cid)
            }
            None => Err(format!("failed to define hidden class {stored_name}")),
        }
    }

    /// Define a new class under a specific user-defined classloader namespace.
    ///
    /// `loader_id` is the unique integer ID of the user-defined classloader.
    /// Classes defined with different loader IDs are isolated (same class name
    /// can exist in multiple loader namespaces per JVM spec §5.3).
    fn define_class_with_loader(
        &mut self,
        name: &str,
        bytes: &[u8],
        loader_id: u32,
    ) -> Option<ClassId>;

    /// WP2.3 — full-options defineClass. Returns
    /// `Ok(class_id)` on success or `Err(error_message)` describing
    /// the LinkageError / ClassFormatError. Used by all four entry
    /// points (Unsafe.defineClass, jdk.internal.misc.Unsafe.defineClass,
    /// MethodHandles.Lookup.defineClass, ClassLoader.defineClass1/2)
    /// so they all dispatch to the same backend.
    ///
    /// `loader_id` is the flat wire encoding of a
    /// [`cratonvm_types::ClassLoaderId`] — the same one
    /// [`NativeContext::loader_id_of_class`] produces, so a loader id fetched
    /// from an existing class can be handed straight back here to define a new
    /// class in that *same* loader (which is exactly what the CGLIB
    /// `@Configuration` enhancer does). Implementations MUST decode it with
    /// `ClassLoaderId::from_native_id_or_default`: `1` is the extension
    /// loader, `2` the application loader, anything `>= 3` a user-defined
    /// namespace, and `0` means "unspecified — use the application loader"
    /// (several callers pass a literal `0` for that). Decoding `2` as
    /// `UserDefined(2)` puts the new class in a different runtime package from
    /// its own superclass and silently breaks package-private override
    /// detection — see
    /// `configproxy-cglib-loaderid-fixed-20260727.md`.
    fn define_class_full(
        &mut self,
        name: &str,
        bytes: &[u8],
        loader_id: u32,
        opts: DefineClassFull,
    ) -> Result<ClassId, String> {
        // Default: degrade to define_class_with_loader / define_class_from_bytes.
        let result = if loader_id > 0 {
            self.define_class_with_loader(name, bytes, loader_id)
        } else {
            self.define_class_from_bytes(name, bytes)
        };
        match result {
            Some(cid) => {
                if opts.hidden {
                    self.set_class_hidden(cid);
                }
                Ok(cid)
            }
            None => Err(format!("define_class_full failed for {name}")),
        }
    }

    /// zt t16/df6: bind the name of a define made with
    /// [`DefineClassFull::bind_name_after_add_class`] whose `addClass` upcall
    /// returned. `None`: `class_id` was bound already (the define did not
    /// defer, or a context that never defers: the default); `Some(true)`:
    /// bound now; `Some(false)`: the name was taken meanwhile, the class stays
    /// nameless and the define is a duplicate definition.
    fn bind_unbound_define(&mut self, class_id: ClassId) -> Option<bool> {
        let _ = class_id;
        None
    }

    /// zt t16/df6: the `addClass` upcall of a define made with
    /// [`DefineClassFull::bind_name_after_add_class`] threw: give the name up,
    /// so no lookup answers the class and a later define of the name defines
    /// a fresh one (HotSpot's failed define leaves no class). `false` when
    /// `class_id` was not unbound (the default: a context that never defers).
    fn withdraw_unbound_define(&mut self, class_id: ClassId) -> bool {
        let _ = class_id;
        false
    }

    /// zt t16/df6 (define page D6): the define of `name` into `loader_id`
    /// (encoded as for [`Self::define_class_full`]) that still waits for its
    /// `addClass` upcall, if any. An empty `name` answers any such define into
    /// that loader. `None` by default.
    fn unbound_define_of(&self, name: &str, loader_id: u32) -> Option<ClassId> {
        let _ = (name, loader_id);
        None
    }

    /// WP2.4 — redefine the bytecode of an already-loaded class.
    ///
    /// Used by `Instrumentation.redefineClasses` /
    /// `retransformClasses`. The class must already exist; otherwise
    /// returns `Err("class not loaded")`. On success, the JIT cache
    /// for the old class is invalidated and any new method lookups
    /// resolve through the new bytecode.
    fn redefine_class(&mut self, class_id: ClassId, new_bytes: &[u8]) -> Result<(), String> {
        let _ = (class_id, new_bytes);
        Err("redefine_class not implemented".to_string())
    }

    /// Like [`Self::redefine_class`] but for
    /// `Instrumentation.retransformClasses`: preserves the class's original
    /// cached bytes so each retransformation re-runs the transformer chain
    /// from the ORIGINAL bytes (not the previously-woven ones). Without this,
    /// a second retransform of the same class — e.g. `mockStatic(X)` then
    /// `mock(X)` — double-instruments it. The default delegates to
    /// `redefine_class` (correct for impls that don't cache original bytes).
    fn retransform_class(&mut self, class_id: ClassId, new_bytes: &[u8]) -> Result<(), String> {
        self.redefine_class(class_id, new_bytes)
    }

    /// Would [`Self::redefine_class`] accept `new_bytes` for `class_id`?
    /// Changes nothing. JVMTI `RedefineClasses` / `RetransformClasses` check
    /// every class of a call before they change any, so the
    /// `java.lang.instrument` natives ask this for each class first
    /// (interpreter round i1 wave 27, lane L3; the VM's check verifies the new
    /// bodies too since wave 28). An `Ok` answer does not promise the
    /// redefinition succeeds (a native `ClassFileLoadHook` may substitute other
    /// bytes). The default accepts everything.
    fn check_class_redefinition(
        &self,
        class_id: ClassId,
        new_bytes: &[u8],
    ) -> Result<(), InstrumentRefusal> {
        let _ = (class_id, new_bytes);
        Ok(())
    }

    /// [`Self::redefine_class`] (`retransform == false`) or
    /// [`Self::retransform_class`], with a refusal reported as the throwable
    /// `java.lang.instrument` raises for it. `retransform_base`, for a
    /// redefinition: the class file as it was before the first
    /// retransform-capable transformer changed it, which a later
    /// retransformation must start from (JVMTI's cached class file); `None`
    /// when no such transformer changed `new_bytes`. For a retransformation:
    /// the base the transformers were handed, kept (if the class keeps none
    /// yet) so the next retransformation starts from it and not from the
    /// live, woven class (interpreter round i1 wave 29, lane L3). The default
    /// ignores it and reports every refusal as an
    /// `UnsupportedOperationException`.
    fn redefine_class_as_instrument(
        &mut self,
        class_id: ClassId,
        new_bytes: &[u8],
        retransform: bool,
        retransform_base: Option<&[u8]>,
    ) -> Result<(), InstrumentRefusal> {
        let _ = retransform_base;
        let outcome = if retransform {
            self.retransform_class(class_id, new_bytes)
        } else {
            self.redefine_class(class_id, new_bytes)
        };
        outcome.map_err(|detail| InstrumentRefusal {
            throwable: "java/lang/UnsupportedOperationException",
            message: None,
            detail,
        })
    }

    /// Open a batch of redefinitions on this thread: the VM work each
    /// [`Self::redefine_class`] / [`Self::retransform_class`] owes the other
    /// threads once per call (the loop-exit handshake for the compiled frames
    /// it withdrew) is taken once, by [`Self::end_redefinition_batch`], for
    /// the whole `Instrumentation.retransformClasses` /
    /// `redefineClasses` call (interpreter round i1 wave 25, lane L6).
    /// Returns whether this call opened the batch (`false`: one is already
    /// open, or the context batches nothing); hand the answer to the end.
    fn begin_redefinition_batch(&mut self) -> bool {
        false
    }

    /// Close a batch [`Self::begin_redefinition_batch`] opened (`opened`),
    /// taking the work its redefinitions deferred. Nothing when `opened` is
    /// `false`.
    fn end_redefinition_batch(&mut self, opened: bool) {
        let _ = opened;
    }

    /// The classes of one `Instrumentation.redefineClasses` /
    /// `retransformClasses` call, every one of them checked, are about to be
    /// installed one after another: hold the other threads so none of them
    /// runs between two of the call's classes (JVMTI installs a call in one
    /// VM operation; interpreter round i1 wave 37, lane L3). `classes` names
    /// each class and the bytes it will get. Returns whether anything was
    /// held; hand the answer to [`Self::end_redefinition_install`]. The
    /// default holds nothing.
    fn begin_redefinition_install(&mut self, classes: &[(ClassId, &[u8])]) -> bool {
        let _ = classes;
        false
    }

    /// Release what [`Self::begin_redefinition_install`] held (`held`), once
    /// the call's last class is installed or one was refused.
    fn end_redefinition_install(&mut self, held: bool) {
        let _ = held;
    }

    /// WP2.4 — list all loaded classes.
    fn list_loaded_class_ids(&self) -> Vec<ClassId> {
        Vec::new()
    }

    /// The lambda proxy classes the VM minted (ids at or above the lambda
    /// proxy base, never in the class store): hidden classes HotSpot's
    /// `Instrumentation.getAllLoadedClasses` lists. Interpreter round i1
    /// wave 42, lane L3.
    fn list_lambda_proxy_class_ids(&self) -> Vec<ClassId> {
        Vec::new()
    }

    /// WP2.4 — list the classes of the loader `loader_id` names in
    /// `ClassLoaderId::to_native_id`'s exact encoding (`0` is the bootstrap
    /// loader, not the application default; interpreter round i1 wave 41):
    /// those it defined, and those it initiated as another loader's where the
    /// VM records that (`Instrumentation.getInitiatedClasses`).
    fn list_initiated_class_ids(&self, _loader_id: u32) -> Vec<ClassId> {
        Vec::new()
    }

    /// An initiating load a native made on the VM's behalf
    /// (`Class.forName(name, init, loader)`, which HotSpot routes through
    /// `SystemDictionary`): the user-defined loader whose namespace id is
    /// `loader_ns` answered `class_id` for `internal_name`. JVMS §5.3.4 first:
    /// `Err` — a thrown `java.lang.LinkageError` with HotSpot's "loader
    /// constraint violation: loader … wants to load class …" message — when a
    /// recorded constraint pins the name to another class for that loader.
    /// Then JVMS §5.3: the loader is recorded as an initiating loader of the
    /// class, which its `findLoadedClass` and its own later define of the name
    /// read. `--real-jdk` only (the other mode records neither); the default
    /// does nothing. Interpreter round i1 wave 38, lane L5.
    fn note_initiating_load(
        &mut self,
        _loader_ns: u32,
        _internal_name: &str,
        _class_id: ClassId,
    ) -> Result<(), MethodCallFailed> {
        Ok(())
    }

    /// The class the user-defined loader with namespace id `loader_ns`
    /// initiated under `internal_name` (JVMS §5.3: the VM asked it and it
    /// answered another loader's class; see [`Self::note_initiating_load`]),
    /// for its `findLoadedClass`. `None` outside `--real-jdk` and by default.
    /// Interpreter round i1 wave 38, lane L5.
    fn initiated_class_for_loader(&self, _loader_ns: u32, _internal_name: &str) -> Option<ClassId> {
        None
    }

    /// Whether the VM records the JDK classes each loader initiates, for
    /// `Instrumentation.getInitiatedClasses` (one relaxed load). A native
    /// that resolves types through a loader asks it before computing what it
    /// would record. `false` by default. Interpreter round i1 wave 47, lane
    /// L5.
    fn jdk_names_initiated_armed(&self) -> bool {
        false
    }

    /// `Class.forName(name, init, loader)` with a built-in or user-defined
    /// loader (`loader_native_id` in `ClassLoaderId::to_native_id`'s
    /// encoding; `0`, the bootstrap loader, records nothing) answered
    /// `class_id`: HotSpot files a JDK class in the loader's dictionary. Does
    /// nothing unless [`Self::jdk_names_initiated_armed`], and by default.
    /// Interpreter round i1 wave 47, lane L5.
    fn note_jdk_class_initiated(&self, _loader_native_id: u32, _class_id: ClassId) {}

    /// A reflective listing of `class_id`'s members (`kind`: `0` methods, `1`
    /// constructors, `2` fields; `public_only` the natives' flag) resolved
    /// their signature types through the class's loader, as HotSpot's
    /// `Reflection::new_method` / `new_constructor` / `new_field` do. Does
    /// nothing unless [`Self::jdk_names_initiated_armed`], and by default.
    /// Interpreter round i1 wave 47, lane L5.
    fn note_reflected_members(&self, _class_id: ClassId, _kind: u8, _public_only: bool) {}

    /// An agent started (`agentmain` by a self-attach): what HotSpot's
    /// `InstrumentationImpl.loadClassAndStartAgent` and the JPLIS library
    /// resolve through loaders on the way (the agent class's declared method
    /// signatures, JNI `FindClass` through the system loader). Does nothing
    /// unless [`Self::jdk_names_initiated_armed`], and by default.
    /// Interpreter round i1 wave 47, lane L5.
    fn note_agent_start(&self, _agent_class_id: Option<ClassId>) {}

    /// zt t22/df10 (define page §9c): whether the application and platform
    /// loaders' `findLoadedClass` answers an ANCESTOR's class (one their
    /// parent chain defined) only when the loader initiated it, as HotSpot's
    /// `JVM_FindLoadedClass` reads the loader's own dictionary
    /// (`CRATONVM_FIND_LOADED_BUILTIN_INITIATED_ONLY`, default ON, `=0` kill;
    /// both modes; needs the resolved class-entry table, whose rows are the
    /// record). Read once per VM. `false` by default: a context without the
    /// record keeps the ancestor answer.
    fn builtin_find_loaded_initiated_only(&self) -> bool {
        false
    }

    /// zt t22/df10: whether the built-in loader `loader_native_id` (`1`
    /// platform, `2` application) has a recorded initiating row for
    /// `class_id` (`ClassRealm::jdk_names_initiated`: a class its classes
    /// resolved, `Class.forName` with it, a reflective listing's signature
    /// type). One read guard. `false` by default.
    fn builtin_loader_initiated(&self, _loader_native_id: u32, _class_id: ClassId) -> bool {
        false
    }

    /// zt t22/df10: at most `limit` DIRECT subtypes of `class_id` (classes
    /// whose superclass is it, classes and interfaces naming it among their
    /// direct interfaces) whose loader is one of `loader_native_ids`
    /// (`ClassLoaderId::to_native_id`'s encoding), under one class-manager
    /// read guard. Empty by default.
    fn direct_subtypes_of_loaders(
        &self,
        _class_id: ClassId,
        _loader_native_ids: &[u32],
        _limit: usize,
    ) -> Vec<ClassId> {
        Vec::new()
    }

    /// zt df4 §9c: does a boot-namespace class of a platform or tool module
    /// of the runtime image -- a class whose Java-visible defining loader is
    /// the built-in loader `loader_native_id` (`ClassRealm::
    /// builtin_image_class_rows`) -- have `class_id` as a DIRECT supertype?
    /// Every direct subtype is read (the subtypes of a JDK root number in the
    /// thousands; t22/df10's bounded read of 4096 missed a platform class
    /// defined after them). `None` when the rule is off (the caller keeps its
    /// bounded read). `None` by default.
    fn builtin_image_class_has_direct_supertype(
        &self,
        _class_id: ClassId,
        _loader_native_id: u32,
    ) -> Option<bool> {
        None
    }

    /// zt t22/df10: count one answer of the built-in `findLoadedClass` rule
    /// ([`Self::builtin_find_loaded_initiated_only`]) for an ancestor's class:
    /// `0` withheld (null), `1` served from a recorded row, `2` served as a
    /// direct supertype of the loader's own class. Printed in the
    /// `CRATONVM_DBG=access` exit census. Nothing by default.
    fn note_builtin_find_loaded_initiated(&self, _outcome: u8) {}

    /// Look up a class by name within a specific user-defined loader's namespace.
    /// Falls back to the standard delegation chain if not found.
    fn class_id_by_name_and_loader(&self, name: &str, loader_id: u32) -> Option<ClassId>;

    /// Exact `(loader, name)` lookup with **no** delegation/global fallback —
    /// only a class the loader with `loader_id` has itself defined. Used by
    /// loader-faithful `findLoadedClass` so it never returns another loader's
    /// class. Default impl falls back to the (fallback-prone)
    /// [`Self::class_id_by_name_and_loader`] for contexts that do not override
    /// it (e.g. test mocks).
    fn class_id_defined_by_loader_exact(&self, name: &str, loader_id: u32) -> Option<ClassId> {
        self.class_id_by_name_and_loader(name, loader_id)
    }

    /// The class the BOOTSTRAP loader itself defined under `name` (HotSpot's
    /// boot dictionary): an exact `(Bootstrap, name)` lookup with no
    /// delegation, no global fallback and no load. The native boundary cannot
    /// ask [`Self::class_id_defined_by_loader_exact`] for it, because its
    /// `loader_id` `0` decodes as the application loader
    /// (`ClassLoaderId::from_native_id_or_default`). `None` by default (test
    /// mocks). Interpreter round i7 wave 138, lane L5: the bootstrap-namespace
    /// doors (`findBootstrapClass`, `JavaLangAccess.findBootstrapClassOrNull`)
    /// of `sun/reflect/misc/Trampoline`, a name `MethodUtil` also defines.
    fn bootstrap_class_id_exact(&self, name: &str) -> Option<ClassId> {
        let _ = name;
        None
    }

    /// The array class `name` as the user-defined loader `loader_id` sees it
    /// (JVMS 5.3.3), synthesising it on demand -- the SAME `ClassId` an `ldc` /
    /// `anewarray` of that descriptor resolves to from a class that loader
    /// defined. Two loaders that each define their own `p/X` get two different
    /// `[Lp/X;` classes. `None` when the context has no loader-faithful
    /// answer (mock contexts), so the caller keeps its flat fallback.
    fn array_class_id_for_loader(&mut self, name: &str, loader_id: u32) -> Option<ClassId> {
        let _ = (name, loader_id);
        None
    }

    /// Make a virtual class (a lambda proxy, which has no class file and no class-store entry)
    /// known by its internal name `<host>$$Lambda/0x<id>`, so array classes whose component it
    /// is (`Array.newInstance(lambdaClass, n)`, `arrayType()`) can be synthesised. A context with
    /// no class manager (mocks) ignores it.
    fn register_virtual_array_component(&mut self, internal_name: &str, class_id: ClassId) {
        let _ = (internal_name, class_id);
    }

    /// Allocate a unique classloader ID for a new user-defined classloader instance.
    fn allocate_loader_id(&mut self) -> u32;

    // -- JPMS module queries (Phase B) --

    /// Check if module `reader` reads module `provider`.
    fn reads_module(&self, reader: &str, provider: &str) -> bool {
        // Default: all modules can read each other (classpath-only mode).
        let _ = (reader, provider);
        true
    }

    /// Check if `module_name` exports `pkg` unconditionally (to all modules).
    ///
    /// AUDIT 2026-05-19: this method has **no default** and is REQUIRED. A
    /// fail-open default (`true`) silently grants arbitrary cross-module
    /// access for any implementor that forgets to override it. Forcing every
    /// `NativeContext` impl to provide a body makes the security decision
    /// explicit. Classpath-only / mock contexts should return `true`
    /// deliberately; a JPMS-enabled VM must perform a real readability check.
    fn is_package_exported_unqualified(&self, module_name: &str, pkg: &str) -> bool;

    /// Check if `module_name` exports `pkg` to `to_module`.
    ///
    /// AUDIT 2026-05-19: REQUIRED, no default — see
    /// `is_package_exported_unqualified` for the fail-open rationale.
    fn is_package_exported_to(&self, module_name: &str, pkg: &str, to_module: &str) -> bool;

    /// Check if `module_name` opens `pkg` unconditionally.
    ///
    /// AUDIT 2026-05-19: REQUIRED, no default — see
    /// `is_package_exported_unqualified` for the fail-open rationale.
    fn is_package_open_unqualified(&self, module_name: &str, pkg: &str) -> bool;

    /// Check if `module_name` opens `pkg` to `to_module`.
    ///
    /// AUDIT 2026-05-19: REQUIRED, no default — see
    /// `is_package_exported_unqualified` for the fail-open rationale.
    fn is_package_open_to(&self, module_name: &str, pkg: &str, to_module: &str) -> bool;

    /// Does the name-map module `module_name` export (`open`: open) `pkg` to
    /// the module NAMED `layer_module` of a non-boot `ModuleLayer` -- by
    /// identity, so a descriptor's qualified `exports p to m` does not reach a
    /// layer's module that only shares the name `m` (the JDK resolves the
    /// target in the module's own configuration)? `None`: no answer (no
    /// registry; the caller keeps its name-keyed answer). Interpreter round
    /// i6 wave 122, lane L5
    /// (`i45-L5-proposal-one-module-access-question-by-identity`).
    ///
    /// `layer_loader_ns` (wave 125): the layer module's defining loader
    /// namespace, so a run-time edge recorded to another layer's (or the boot
    /// layer's) module of that name does not reach it under
    /// `CRATONVM_MODULE_ACCESS_EXPORT_ONE_QUESTION`; `None` matches by name.
    fn is_package_exported_or_open_to_layer_module(
        &self,
        module_name: &str,
        pkg: &str,
        layer_module: &str,
        layer_loader_ns: Option<u32>,
        open: bool,
    ) -> Option<bool> {
        let _ = (module_name, pkg, layer_module, layer_loader_ns, open);
        None
    }

    /// Add a dynamic read edge: `reader` reads `provider`.
    fn module_add_reads(&mut self, reader: &str, provider: &str) {
        let _ = (reader, provider);
    }

    /// Add a dynamic export: `module_name` exports `pkg` to `target`.
    fn module_add_exports(&mut self, module_name: &str, pkg: &str, target: &str) {
        let _ = (module_name, pkg, target);
    }

    /// [`Self::module_add_exports`] whose target is the module `target` of a
    /// non-boot `ModuleLayer`, defined to the loader `target_loader_ns`
    /// (`Module.addExports0` with a user-layer `other`): recorded by that
    /// identity. The default records it by name. Interpreter round i6 wave
    /// 125, lane L5 (`i45-L5-proposal-one-module-access-question-by-identity`).
    fn module_add_exports_to_layer_module(
        &mut self,
        module_name: &str,
        pkg: &str,
        target: &str,
        target_loader_ns: u32,
    ) {
        let _ = target_loader_ns;
        self.module_add_exports(module_name, pkg, target);
    }

    /// [`Self::module_add_exports_to_layer_module`]'s `opens` twin (the
    /// forced `implAddOpens(pn, other)` / `addOpens` natives). The default
    /// records it by name.
    fn module_add_opens_to_layer_module(
        &mut self,
        module_name: &str,
        pkg: &str,
        target: &str,
        target_loader_ns: u32,
    ) {
        let _ = target_loader_ns;
        self.module_add_opens(module_name, pkg, target);
    }

    /// Add a dynamic open: `module_name` opens `pkg` to `target`.
    fn module_add_opens(&mut self, module_name: &str, pkg: &str, target: &str) {
        let _ = (module_name, pkg, target);
    }

    /// `--real-jdk`: define the module `name` of a non-boot `ModuleLayer` to
    /// the user-defined loader `loader_ns`, holding `packages` (internal
    /// form), in the VM's module registry by IDENTITY (`(loader_ns, name)`;
    /// two layers may each define a module `m`): `Module.defineModule0`'s VM
    /// half. `is_open` is its `isOpen` argument. Access control then judges
    /// a class the loader defines in one of the packages as a member of the
    /// module. Default: no-op.
    fn layer_module_define(
        &mut self,
        loader_ns: u32,
        name: &str,
        is_open: bool,
        packages: &[String],
    ) {
        let _ = (loader_ns, name, is_open, packages);
    }

    /// `--real-jdk`: the layer module `(loader_ns, name)` (see
    /// [`Self::layer_module_define`]) reads `target`: the VM half of
    /// `Module.addReads0` for such a module. Default: no-op.
    fn layer_module_add_read(
        &mut self,
        loader_ns: u32,
        name: &str,
        target: LayerModuleEdgeTarget<'_>,
    ) {
        let _ = (loader_ns, name, target);
    }

    /// `--real-jdk`: the layer module `(loader_ns, name)` (see
    /// [`Self::layer_module_define`]) has the descriptor version `version`
    /// (`Module.defineModule0`'s `version` argument), which a stack trace
    /// element of its classes names. Default: no-op.
    fn layer_module_set_version(&mut self, loader_ns: u32, name: &str, version: &str) {
        let _ = (loader_ns, name, version);
    }

    /// `--real-jdk`: the layer module `(loader_ns, name)` (see
    /// [`Self::layer_module_define`]) exports `pkg` (internal form) to
    /// `target`: the VM half of `Module.addExports0` / `addExportsToAll0` /
    /// `addExportsToAllUnnamed0` for such a module. Default: no-op.
    fn layer_module_add_export(
        &mut self,
        loader_ns: u32,
        name: &str,
        pkg: &str,
        target: LayerModuleEdgeTarget<'_>,
    ) {
        let _ = (loader_ns, name, pkg, target);
    }

    /// Return all packages owned by `module_name`.
    fn module_packages(&self, module_name: &str) -> Vec<String> {
        let _ = module_name;
        vec![]
    }

    /// Return the `uses` service-type binary names (slash format) declared by
    /// `module_name`'s `module-info.class` `uses` directives. Empty for the
    /// unnamed module, an unregistered module, or a module whose descriptor
    /// declares no `uses`.
    fn module_uses(&self, module_name: &str) -> Vec<String> {
        let _ = module_name;
        vec![]
    }

    /// True if `module_name`'s `module-info.class` declared `open module ...`
    /// (the real `ACC_MODULE_OPEN` flag). `false` for the unnamed module, an
    /// unregistered module, or a module that isn't open.
    fn module_is_open(&self, module_name: &str) -> bool {
        let _ = module_name;
        false
    }

    /// True if the VM's `ModuleRegistry` holds a descriptor for `module_name`.
    ///
    /// The default is `false` ("I know nothing"), so a `NativeContext` that does
    /// not model modules can never be read as asserting a module's ABSENCE —
    /// callers must probe a known-present name (`java.base`) before treating a
    /// `false` here as evidence.
    fn module_is_registered(&self, module_name: &str) -> bool {
        let _ = module_name;
        false
    }

    /// Every module name the VM's module registry knows, in no defined order.
    ///
    /// The registry is small by construction, so callers may enumerate it
    /// eagerly. An empty vector means "this context does not model modules",
    /// exactly as [`module_is_registered`] returning `false` does, and must not
    /// be read as "no modules exist".
    ///
    /// It is NOT just "`java.base` plus whatever `--module-path` supplied", and
    /// it is NOT populated by only two sites. A THIRD site scans the
    /// APPLICATION CLASS PATH for `module-info.class` and registers what it
    /// finds — so a modular jar on `-cp`, which a real JVM treats as an
    /// unnamed-module citizen whose `module-info` is ignored, appears here as a
    /// module. That false sentence is what made a real defect look impossible
    /// from this trait's side: registering such a jar's services twice made
    /// `ServiceLoader` throw "multiple engines with the same ID". Use
    /// [`Self::module_is_class_path_only`] below to tell the two apart.
    /// docs/feature-designs/class-resolution-and-caching.md
    fn module_names(&self) -> Vec<String> {
        vec![]
    }

    /// True when `module_name` reached the registry ONLY by scanning the
    /// application class path — i.e. a modular jar on `-cp`, which a real JVM
    /// treats as an unnamed-module citizen whose `module-info` is ignored.
    ///
    /// Callers that model the JDK's module *system* must skip these; callers
    /// that only want labelling may not care. Defaults to `false` so a context
    /// that does not model modules keeps its existing behaviour.
    fn module_is_class_path_only(&self, module_name: &str) -> bool {
        let _ = module_name;
        false
    }

    /// True when `module_name` is a system module the boot configuration
    /// does not resolve (JEP 261's default roots, `--add-modules`, `requires`
    /// and service binding; `ModuleRegistry::settle_boot_configuration`):
    /// it is not in `ModuleLayer.boot()`. Defaults to `false` (every
    /// registered module in, as before). Interpreter round i4 wave 86, L5.
    fn module_outside_boot_configuration(&self, module_name: &str) -> bool {
        let _ = module_name;
        false
    }

    /// The registered modules the boot configuration resolves (HotSpot's
    /// `ModuleLayer.boot()` set; `ModuleRegistry::boot_configuration_modules`),
    /// sorted. Empty means "not known" (not settled, or a context that does
    /// not model modules), never "no modules". Interpreter round i4 wave 99,
    /// lane L3.
    fn boot_configuration_module_names(&self) -> Vec<String> {
        Vec::new()
    }

    /// The `--patch-module` paths of `module_name` (searched before the
    /// module's own content), empty when it is not patched. Default: none.
    /// Interpreter round i4 wave 87, lane L5.
    fn module_patch_paths(&self, module_name: &str) -> Vec<String> {
        let _ = module_name;
        Vec::new()
    }

    /// `exports` directives declared by `module_name`, as
    /// `(package, targets)`. Package names are INTERNAL (slash) form; an empty
    /// `targets` is an unqualified export.
    fn module_exports(&self, module_name: &str) -> Vec<(String, Vec<String>)> {
        let _ = module_name;
        vec![]
    }

    /// `opens` directives declared by `module_name`, same shape as
    /// [`module_exports`].
    fn module_opens(&self, module_name: &str) -> Vec<(String, Vec<String>)> {
        let _ = module_name;
        vec![]
    }

    /// Edges recorded AFTER the module-info was parsed (`--add-exports`,
    /// `Module.addExports`), as `(package (slash form), target)`: `None` =
    /// every module, `Some("")` = the unnamed module, `Some(name)` = that
    /// module. NOT part of [`module_exports`], which is the declaration.
    fn module_dynamic_exports(&self, module_name: &str) -> Vec<(String, Option<String>)> {
        let _ = module_name;
        vec![]
    }

    /// The `opens` twin of [`module_dynamic_exports`] (`--add-opens`).
    fn module_dynamic_opens(&self, module_name: &str) -> Vec<(String, Option<String>)> {
        let _ = module_name;
        vec![]
    }

    /// Modules `reader` was given a read edge to after the module-info was
    /// parsed (`--add-reads`, `Module.addReads`); `""` is the unnamed module.
    fn module_dynamic_reads(&self, reader: &str) -> Vec<String> {
        let _ = reader;
        vec![]
    }

    /// `requires` directives declared by `module_name`, as
    /// `(module name, is_transitive, is_static)`.
    fn module_requires(&self, module_name: &str) -> Vec<(String, bool, bool)> {
        let _ = module_name;
        vec![]
    }

    /// `provides` directives declared by `module_name`, as
    /// `(service, providers)` — binary class names in INTERNAL (slash) form.
    fn module_provides(&self, module_name: &str) -> Vec<(String, Vec<String>)> {
        let _ = module_name;
        vec![]
    }

    /// True if `module_name`'s descriptor was registered for a jar on the
    /// CLASS path rather than a real module path — the JDK's *automatic
    /// module* shape. Backs `ModuleDescriptor.isAutomatic()` and
    /// `ModuleDescriptor.Modifier.AUTOMATIC`.
    fn module_is_automatic(&self, module_name: &str) -> bool {
        let _ = module_name;
        false
    }

    /// True if `module_name` is an AUTOMATIC module of the MODULE path (a jar
    /// with no `module-info.class`, `ModulePath.deriveModuleDescriptor`), which
    /// the registry keeps as an open descriptor that reads every module.
    /// `Module.getDescriptor()` answers `isAutomatic() == true`,
    /// `isOpen() == false` for it, as `ModuleDescriptor.newAutomaticModule`
    /// builds it. Interpreter round i5 wave 101, lane L5. Default `false`.
    fn module_is_module_path_automatic(&self, module_name: &str) -> bool {
        let _ = module_name;
        false
    }

    /// The raw module version string from `module-info.class`
    /// (`Module.version_index`), if the producer recorded one. Backs
    /// `ModuleDescriptor.rawVersion()` and, through `Version.parse`,
    /// `version()`.
    fn module_version(&self, module_name: &str) -> Option<String> {
        let _ = module_name;
        None
    }

    /// The module names the `ModuleHashes` attribute of `module_name`'s
    /// `module-info.class` records (the set `jlink` hashed into `java.base`),
    /// if the module carries the attribute. Backs the boot `java.base`
    /// `ModuleReferenceImpl.recordedHashes()`, which
    /// `StackTraceElement.HashedModules` reads.
    fn module_recorded_hash_names(&self, module_name: &str) -> Option<Vec<String>> {
        let _ = module_name;
        None
    }

    /// `StackTraceElement.HashedModules.contains(module_name)` for a
    /// boot-layer module: `module_name` is `java.base` or one of the names
    /// `java.base`'s recorded hashes list. `None` when `java.base` records
    /// no hashes (the caller decides its fallback). The default answers from
    /// [`Self::module_recorded_hash_names`]; the VM answers without copying
    /// the set (interpreter round i3 wave 79, lane L2).
    fn module_hashed_in_java_base(&self, module_name: &str) -> Option<bool> {
        self.module_recorded_hash_names("java.base")
            .map(|names| module_name == "java.base" || names.iter().any(|n| n == module_name))
    }

    /// The `ModuleMainClass` attribute's class in INTERNAL (slash) form, if
    /// the module declares one. Backs `ModuleDescriptor.mainClass()`.
    fn module_main_class(&self, module_name: &str) -> Option<String> {
        let _ = module_name;
        None
    }

    /// `requires` directives with the FULL modifier set and compiled version:
    /// `(name, transitive, static, synthetic, mandated, compiled_version)`.
    ///
    /// Supersedes [`module_requires`], whose `(String, bool, bool)` tuple
    /// cannot carry `ACC_SYNTHETIC`/`ACC_MANDATED` — which is why
    /// `Requires.modifiers()` reports `[]` for `requires java.base` where
    /// HotSpot reports `[MANDATED]`. Added alongside rather than widening the
    /// existing method so no current caller has to change.
    fn module_requires_full(
        &self,
        module_name: &str,
    ) -> Vec<(String, bool, bool, bool, bool, Option<String>)> {
        let _ = module_name;
        vec![]
    }

    /// `exports` directives with their modifier bits:
    /// `(package, targets, synthetic, mandated)`. Package names are INTERNAL
    /// (slash) form, as in [`module_exports`].
    fn module_exports_full(&self, module_name: &str) -> Vec<(String, Vec<String>, bool, bool)> {
        let _ = module_name;
        vec![]
    }

    /// `opens` directives, same shape as [`module_exports_full`].
    fn module_opens_full(&self, module_name: &str) -> Vec<(String, Vec<String>, bool, bool)> {
        let _ = module_name;
        vec![]
    }

    /// Return all registered module names.
    fn all_module_names(&self) -> Vec<String> {
        vec![]
    }

    /// Find which module owns a given package (slash format).
    fn module_for_package(&self, pkg: &str) -> Option<String> {
        let _ = pkg;
        None
    }

    /// [`Self::module_for_package`] for a NAMED module only: a modular jar on the class path is an
    /// ordinary jar to a real JVM (its classes are in the unnamed module), so it answers `None`
    /// here. SBR5 `clload`. The default is the raw answer (never `None` for a package it knows).
    fn named_module_for_package(&self, pkg: &str) -> Option<String> {
        self.module_for_package(pkg)
    }

    /// Whether the runtime image (`lib/modules`) defines the class `internal`
    /// (slash form) -- the classes the bootstrap and platform loaders can
    /// define. `None` when there is no image to ask.
    fn runtime_image_defines_class(&self, internal: &str) -> Option<bool> {
        let _ = internal;
        None
    }

    /// Is any currently-LOADED class a member of `package_slash` (immediate
    /// members only; `""` is the default package)?
    ///
    /// The question `ClassLoader.getDefinedPackage` actually asks. A loader
    /// DEFINES a package once it has defined a class in it, which is why the
    /// class-file *visibility* probes above cannot answer it: the boot image
    /// contains `java/awt/image/*.class` on every run, and HotSpot still
    /// answers `null` for `java.awt.image` until something loads one.
    ///
    /// # The default is `false`, and that is the safe direction
    ///
    /// An implementation that has not overridden this answers "no class of
    /// that package is loaded", which makes the module-backed arm of
    /// `classloader::builtin_loader_defines_package` decline rather than
    /// fabricate. Declining is the answer `getDefinedPackage`'s contract
    /// prefers (a missing `Package` over an invented one) and is exactly what
    /// this VM did before that arm existed.
    fn any_loaded_class_in_package(&self, package_slash: &str) -> bool {
        let _ = package_slash;
        false
    }

    /// [`Self::any_loaded_class_in_package`], narrowed to the classes ONE
    /// loader defined. `loader_id` is the flat `ClassLoaderId` numbering
    /// [`Self::loader_id_of_class`] reports.
    ///
    /// Same `false` default, for the same reason: an implementation that has
    /// not overridden this declines rather than fabricating.
    fn any_loaded_class_in_package_for_loader(&self, package_slash: &str, loader_id: u32) -> bool {
        let _ = (package_slash, loader_id);
        false
    }

    /// One loaded class per distinct package among the classes the loader
    /// tagged `loader_id` (the flat numbering of [`Self::loader_id_of_class`])
    /// defined from real bytes -- the boot image or the class path; no hidden,
    /// array, generated or stub class -- as `(package in internal form, class
    /// id)`, `""` for the unnamed package. The built-in loaders' package
    /// listings ask it (interpreter round i2 wave 55, lane L5). The default
    /// lists nothing, as [`Self::any_loaded_class_in_package_for_loader`]
    /// declines.
    fn loaded_package_representatives(&self, loader_id: u32) -> Vec<(String, ClassId)> {
        let _ = loader_id;
        Vec::new()
    }

    /// [`Self::loaded_package_representatives`]'s class for the one package
    /// `package_slash` (internal form), or `None`: the same class the listing
    /// names for it. The VM answers with one scan that stops at the first
    /// match and allocates nothing; the default filters the listing.
    /// Interpreter round i4 wave 93, lane L5 (`--real-jdk`'s
    /// `getDefinedPackage` sync asks it per call until the package is synced).
    fn loaded_package_representative(&self, loader_id: u32, package_slash: &str) -> Option<ClassId> {
        self.loaded_package_representatives(loader_id)
            .into_iter()
            .find(|(pkg, _)| pkg == package_slash)
            .map(|(_, cid)| cid)
    }

    /// Mark a class as hidden (JEP 371). Hidden classes are not discoverable via
    /// `Class.forName` or `ClassLoader.findLoadedClass`.
    fn set_class_hidden(&mut self, class_id: ClassId) {
        let _ = class_id;
    }

    /// Query whether a class is hidden (JEP 371). Used by the
    /// `Class.isHidden()` native. The default returns `false` for every
    /// class so implementations that do not support hidden classes
    /// continue to work.
    fn is_class_hidden(&self, class_id: ClassId) -> bool {
        let _ = class_id;
        false
    }

    /// Copy nest-host and nest-members information from `source_class` to
    /// `target_class`. Used by `defineHiddenClass` when the `NESTMATE`
    /// class option is specified: the hidden class joins the lookup
    /// class's nest rather than being a standalone nest of its own.
    /// The default is a no-op for implementations that do not track
    /// nest membership.
    fn copy_nest_info(&mut self, source_class: ClassId, target_class: ClassId) {
        let _ = (source_class, target_class);
    }

    /// Force a class to complete its `<clinit>` immediately. Used by
    /// `defineHiddenClass` when the `initialize` flag is `true`, and by
    /// `Class.forName`/`Constructor.newInstance`/`Lookup.ensureInitialized`.
    /// The default is a no-op — callers that care about deterministic init
    /// must override this in their NativeContext impl.
    ///
    /// HIB-CV-26 fix (2026-07-16): the error type is `MethodCallFailed`
    /// (not a flattened `String`) so a `<clinit>` failure keeps its
    /// two-layer identity all the way to the caller: a genuine Java
    /// exception from a static initializer comes back as
    /// `MethodCallFailed::ExceptionThrown` (already correctly wrapped as a
    /// catchable `ExceptionInInitializerError`/`NoClassDefFoundError` by
    /// `ensure_class_initialized_shared` per JVMS §5.5) and only a true
    /// VM-level bug comes back as `MethodCallFailed::InternalError`.
    /// Collapsing both into a `String` here previously forced every call
    /// site to treat ordinary `<clinit>` exceptions as unrecoverable
    /// internal errors, aborting the VM instead of letting Java code catch
    /// them.
    fn initialize_class(&mut self, class_id: ClassId) -> Result<(), MethodCallFailed> {
        let _ = class_id;
        Ok(())
    }

    /// [`Self::initialize_class`] for a native serving a JDK method whose
    /// initialization HotSpot runs `chain`'s hops below it (the served method
    /// first, `Unsafe.ensureClassInitialized0` last): the
    /// `ExceptionInInitializerError` / `NoClassDefFoundError` the
    /// initialization raises lists those frames above the native's caller
    /// (`chain.throwable_class` is ignored: the class's state picks it).
    /// Interpreter round i2 wave 58, lane L3. The default initializes alone.
    fn initialize_class_served(
        &mut self,
        class_id: ClassId,
        chain: ServedRaiseChain,
    ) -> Result<(), MethodCallFailed> {
        let _ = chain;
        self.initialize_class(class_id)
    }

    /// [`Self::ensure_class_initialized`] (the class named `name`, resolved
    /// as that method resolves it) whose initialization lists `chain()`'s
    /// frames, as [`Self::initialize_class_served`]; `chain` is built only
    /// for a class that is not initialized yet, so an initialized one costs
    /// what `ensure_class_initialized` costs. For the method-handle and
    /// VarHandle doors that name their member's class by name (`--compatible`
    /// handles record no owner). Interpreter round i2 wave 59, lane L4. The
    /// default initializes alone.
    fn ensure_class_initialized_served(
        &mut self,
        name: &str,
        chain: &dyn Fn() -> ServedRaiseChain,
    ) -> Result<ClassId, MethodCallFailed> {
        let _ = chain;
        self.ensure_class_initialized(name)
    }

    /// True while the current thread is executing inside some class's
    /// `<clinit>` (including nested/re-entrant `<clinit>` calls it
    /// transitively triggers).
    ///
    /// Callers that would otherwise force a *different*, unrelated class's
    /// full initialization as a side effect of reflection (e.g. resolving
    /// the return type of a `java.lang.reflect.Method` mirror) must check
    /// this first and skip the eager `initialize_class` call when true --
    /// JVMS §5.5 never requires initializing a class merely because its
    /// name shows up in another class's method signature, and doing so
    /// anyway while genuinely mid-`<clinit>` lets the forced class observe
    /// the in-progress class's statics before they are assigned. Defaults
    /// to `false` (preserves prior behavior) for implementors that do not
    /// track this.
    fn in_clinit(&self) -> bool {
        false
    }

    /// Return all JPMS `provides` implementation class names for a given service
    /// interface (binary class name, e.g. `"com/example/MyService"`).
    /// Walks all registered module descriptors' `provides` entries.
    fn service_providers_from_modules(&self, service_class: &str) -> Vec<String> {
        let _ = service_class;
        vec![]
    }

    // -- JPMS deep reflection access (Phase B) --

    /// Check whether `accessor_class_id` has deep (reflective) access to
    /// `target_class_id` via JPMS `opens` directives.
    ///
    /// Returns `Ok(())` if allowed (same module, unnamed module, target module
    /// opens the package, or a dynamic `addOpens` edge exists).
    /// Returns `Err(message)` if the access is denied.
    ///
    /// Called by reflection natives (`Method.invoke`, `Field.get/set`,
    /// `Constructor.newInstance`) when `setAccessible(true)` is used on a
    /// member in a different module.
    ///
    /// AUDIT 2026-05-19: this method has **no default** and is REQUIRED. The
    /// previous `Ok(())` default silently allowed arbitrary cross-module
    /// `setAccessible` for any implementor that forgot to override it.
    /// Forcing every `NativeContext` impl to provide a body makes the
    /// security decision explicit — classpath-only / mock contexts may
    /// return `Ok(())` deliberately; a JPMS-enabled VM must perform a real
    /// module-readability + opens check.
    fn check_deep_reflection_access(
        &self,
        accessor_class_id: ClassId,
        target_class_id: ClassId,
    ) -> Result<(), String>;

    /// Is the package declaring `target_class_id` *exported* (not opened) to
    /// the module of `accessor_class_id`?
    ///
    /// This is the narrower JPMS question `AccessibleObject
    /// .checkCanSetAccessible` asks once the `opens` test has failed. JDK 17+
    /// still permits `setAccessible(true)` on a **public** member of a
    /// **public** class when the declaring package is merely *exported* — no
    /// `--add-opens` needed. An `opens`-only gate therefore over-denies in
    /// exactly the place HotSpot allows, which is why this second query
    /// exists rather than being folded into the one above.
    ///
    /// Default `false` (fail closed): it is only ever consulted to widen a
    /// decision [`check_deep_reflection_access`] has already refused, so a
    /// mock that does not implement it simply keeps that refusal.
    ///
    /// [`check_deep_reflection_access`]: NativeContext::check_deep_reflection_access
    fn reflective_export_to_accessor(
        &self,
        _accessor_class_id: ClassId,
        _target_class_id: ClassId,
    ) -> bool {
        false
    }

    /// Does [`Self::reflective_export_to_accessor`] answer `true` for
    /// `target_class_id` whatever the accessor -- the module registry is
    /// empty (class-path-only mode), or the class is in an unnamed module and
    /// in no non-boot layer's module
    /// (`classloading::access_control::reflective_export_reaches_every_accessor`)?
    /// A served reflective door that gets `true` skips its caller walk: the
    /// `exports` gate cannot refuse. Asked per call, never cached (interpreter
    /// round i3 wave 74, lane L3, `CRATONVM_REFLECT_INVOKE_PLAN`).
    ///
    /// Default `false` (fail closed: the caller asks the full question).
    fn reflective_export_to_every_accessor(&self, _target_class_id: ClassId) -> bool {
        false
    }

    // -- T13 java/lang/Class reflection metadata --

    /// Get the class file version (major number) for a class.
    fn class_file_version(&self, _class_id: ClassId) -> u16 {
        65 // Default: Java 21
    }

    /// The class file's MINOR version (`0xFFFF` for a preview class file),
    /// which `Class.getClassFileVersion0` packs above the major as HotSpot's
    /// `JVM_GetClassFileVersion` does (interpreter round i5 wave 106, lane
    /// L3). Default `0`.
    fn class_file_minor_version(&self, _class_id: ClassId) -> u16 {
        0
    }

    /// Get the inner classes of a class.
    /// Returns vec of (inner_class_name, outer_class_name, inner_name, access_flags).
    fn inner_classes(&self, _class_id: ClassId) -> Vec<(String, String, String, u16)> {
        Vec::new()
    }

    /// Get the enclosing method info for a class.
    /// Returns (enclosing_class, method_name, method_descriptor) or None.
    fn enclosing_method(&self, _class_id: ClassId) -> Option<(String, String, String)> {
        None
    }

    /// The class half of [`Self::enclosing_method`] alone: one `String`
    /// instead of three, for a caller that only asks "does it have
    /// `EnclosingMethod`, and naming which class" (JIT round 21 wave 22, lane
    /// hidden-resid; item 4 of `r21-review-w22-findings-FIXED-20261020.md`).
    fn enclosing_method_class_name(&self, class_id: ClassId) -> Option<String> {
        self.enclosing_method(class_id).map(|(class_name, _, _)| class_name)
    }

    /// Get the declaring class of this class (from InnerClasses attribute).
    /// Returns the class ID of the outer class, or None if not an inner class.
    fn declaring_class(&self, _class_id: ClassId) -> Option<ClassId> {
        None
    }

    /// Get the raw annotation bytes for a class.
    /// Returns the bytes of the RuntimeVisibleAnnotations attribute, or empty.
    fn raw_annotations(&self, _class_id: ClassId) -> Vec<u8> {
        Vec::new()
    }

    /// Get the raw type annotation bytes for a class.
    fn raw_type_annotations(&self, _class_id: ClassId) -> Vec<u8> {
        Vec::new()
    }

    /// SBR wave 5 (reflmeta): the BODY bytes of a member's annotation
    /// attributes, one answer per entry of `attrs`, exactly what HotSpot
    /// stores in the reflection object's `annotations` /
    /// `parameterAnnotations` / `annotationDefault` byte fields and answers
    /// from `getTypeAnnotationBytes0`. The constant pool indices inside refer
    /// to the declaring class's own pool, which [`reflect_constant_pool_entry`]
    /// serves to the real `jdk.internal.reflect.ConstantPool`. One call finds
    /// the member once, however many attributes a mint needs.
    ///
    /// `member_desc` is the JVM descriptor (a field's type descriptor for a
    /// field). `None` for an attribute the member does not have -- HotSpot's
    /// `null`, which the JDK parsers read as "no annotations" (an EMPTY array
    /// would underflow their first `getShort`). Default: all `None`.
    ///
    /// [`reflect_constant_pool_entry`]: NativeClassAccess::reflect_constant_pool_entry
    fn member_raw_annotation_attributes(
        &self,
        _class_id: ClassId,
        _member_name: &str,
        _member_desc: &str,
        _is_method: bool,
        attrs: &[RawAnnotationAttr],
    ) -> Vec<Option<Vec<u8>>> {
        vec![None; attrs.len()]
    }

    /// SBR wave 5 (reflmeta): the size of `class_id`'s constant pool
    /// (`constant_pool_count`), for `jdk.internal.reflect.ConstantPool.getSize0`.
    /// `None` when the class has no pool here. Default `None`.
    fn reflect_constant_pool_size(&self, _class_id: ClassId) -> Option<usize> {
        None
    }

    /// SBR wave 5 (reflmeta): entry `index` of `class_id`'s constant pool, as
    /// the natives of `jdk.internal.reflect.ConstantPool` need it (the pool the
    /// raw annotation bytes index into). Default [`ReflectConstant::Invalid`].
    fn reflect_constant_pool_entry(&self, _class_id: ClassId, _index: i32) -> ReflectConstant {
        ReflectConstant::Invalid
    }

    /// Get the runtime-visible TYPE_USE annotations targeting one of this
    /// class's declared supertypes (JVMS 4.7.20 `target_type` 0x10,
    /// CLASS_EXTENDS). `supertype_index` is the JVMS-defined index: `0xFFFF`
    /// (65535) selects the superclass, `0..n` selects the n-th entry of
    /// `getInterfaces()`.
    ///
    /// Returns a [`TypeArgAnnotations`] tree: `.anns` holds annotations with
    /// an empty `type_path` (directly on the supertype itself, e.g.
    /// `implements @Foo Bar`); `.children[i]` holds the subtree for the
    /// supertype's i-th type argument (recursively, for arbitrarily nested
    /// generics, e.g. `implements ValueExtractor<ArgumentValue<@ExtractedValue
    /// ?>>`). Backs `Class.getAnnotatedSuperclass()` /
    /// `Class.getAnnotatedInterfaces()` and their
    /// `getAnnotatedActualTypeArguments()` chains. Default impl returns an
    /// empty tree so mock `NativeContext` implementations don't need to plumb
    /// the attribute store.
    fn class_extends_type_annotations(
        &self,
        _class_id: ClassId,
        _supertype_index: u16,
    ) -> TypeArgAnnotations {
        TypeArgAnnotations::default()
    }

    /// Get the nest host class name for a class.
    /// Returns None if the class is its own nest host.
    fn nest_host_name(&self, _class_id: ClassId) -> Option<String> {
        None
    }

    /// Get the nest member class names for a class.
    fn nest_member_names(&self, _class_id: ClassId) -> Vec<String> {
        Vec::new()
    }
}

pub trait NativeInvokeAccess: NativeClassAccess {
    /// Capability boundary: Java method invocation and linkage.

    /// Invoke a method by class name, method name, descriptor, and arguments.
    fn invoke(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult;

    /// Invoke a method on an ALREADY-RESOLVED declaring class, bypassing
    /// name-based class resolution entirely.
    ///
    /// `Method.invoke()` (reflection) on a static method already has an
    /// unambiguous declaring `ClassId` in hand (from the `Method` object's
    /// own `clazz` mirror) — it must not re-resolve the class by NAME, which
    /// goes through the loader-blind global lookup (`load_class`/
    /// `get_loaded_class_id`). That lookup deliberately returns "not found"
    /// (not a guess) whenever 2+ *different* user-defined loaders each
    /// register their own distinct class under the identical simple name —
    /// an intentional, documented anti-ambiguity guard (see
    /// `ClassManager::get_loaded_class_id`), but it means ANY name-based
    /// re-resolution after the fact is unsound the moment a second same-named
    /// class from a different loader exists anywhere in the process — a
    /// completely ordinary pattern for repeatedly-invoked test/codegen
    /// harnesses that mint a fresh ClassLoader + identically-named generated
    /// class each time (e.g. Spring's `TestCompiler`/
    /// `@CompileWithForkedClassLoader`, which produced the exact
    /// `GroupsMetadataValueDelegateTests` "class file error: class not
    /// found" VM abort this fixes). Default implementation falls back to the
    /// name-based [`Self::invoke`] for callers/mocks that have no ClassId
    /// fast path; the real VM overrides this to skip re-resolution.
    fn invoke_by_class_id(
        &mut self,
        class_id: ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        let _ = class_id;
        self.invoke(class_name, method_name, descriptor, args)
    }

    /// [`Self::invoke`] for a caller that knows the target is a STATIC method
    /// (a `REF_invokeStatic` method handle): the class initialized first is
    /// the one that DECLARES the method (JVMS §5.5, as `invokestatic` and
    /// `DirectMethodHandle.ensureInitialized` do), never the class an
    /// inherited static is named through. [`Self::invoke`] cannot tell a
    /// static from an instance target and initializes `class_name`.
    ///
    /// Default: [`Self::invoke`], for contexts that model no initialization.
    fn invoke_static(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        self.invoke(class_name, method_name, descriptor, args)
    }

    /// [`Self::invoke_static`] on an ALREADY-RESOLVED owner class — the
    /// class the method is named through, not necessarily its declaring
    /// class — for the loader-identity reason on [`Self::invoke_by_class_id`].
    ///
    /// Default: [`Self::invoke_by_class_id`].
    fn invoke_static_by_class_id(
        &mut self,
        class_id: ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        self.invoke_by_class_id(class_id, class_name, method_name, descriptor, args)
    }

    /// [`Self::invoke_static_by_class_id`] (`owner` is `Some`) or
    /// [`Self::invoke_static`] (`None`) for a `REF_invokeStatic` handle that
    /// remembers when its initialization is settled, as HotSpot's
    /// `DirectMethodHandle` drops its `ensureInitialized` check once the
    /// declaring class is initialized.
    ///
    /// `settled` is the owner class an earlier call of the same handle
    /// returned. When the owner this call dispatches on is that class, the
    /// call skips the initialization check (and the member resolution an
    /// inherited static pays for it on every call). The second value is the
    /// owner the handle may remember from now on: `Some` only once nothing is
    /// left to initialize for it — initialization is monotone, so that stays
    /// true. Interpreter round i1 wave 20, lane L3.
    ///
    /// Default: the two routes above, never settled.
    fn invoke_static_settling(
        &mut self,
        owner: Option<ClassId>,
        settled: Option<ClassId>,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> (MethodCallResult, Option<ClassId>) {
        let _ = settled;
        let result = match owner {
            Some(owner) => {
                self.invoke_static_by_class_id(owner, class_name, method_name, descriptor, args)
            }
            None => self.invoke_static(class_name, method_name, descriptor, args),
        };
        (result, None)
    }

    /// Settling invocation whose first argument was just allocated as an
    /// ordinary (non-array, non-lambda) object of `first_class` by this VM.
    /// The caller must know this from the allocation, not predict a class
    /// from the method descriptor. The ordinary tail still witnesses the
    /// first successful memo fill. Default contexts keep the ordinary door.
    #[allow(clippy::too_many_arguments)]
    fn invoke_static_settling_minted(
        &mut self,
        owner: Option<ClassId>,
        settled: Option<ClassId>,
        first_class: ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> (MethodCallResult, Option<ClassId>) {
        let _ = first_class;
        self.invoke_static_settling(owner, settled, class_name, method_name, descriptor, args)
    }

    /// [`Self::invoke_static_settling`] for a handle that also keeps its
    /// door's served-raise key ([`SettledDoorKey`]) beside its settled owner:
    /// the VM's static door notes the method it is entering for the length
    /// of the call (`CRATONVM_THROWABLE_SERVED_RAISE_DOOR_KEY`), and a handle
    /// that passes the slot lets a later call write that note with no
    /// class-manager read and no method scan. The slot is the caller's (one
    /// per handle record) and the VM's to fill; the caller never reads it.
    /// Interpreter round i2 wave 65, lane L3
    /// (`CRATONVM_THROWABLE_SERVED_RAISE_SETTLED_DOOR_KEY`, proposal
    /// `i64-L3-proposal-the-settling-handle-door-carries-its-door-key`).
    ///
    /// Default: [`Self::invoke_static_settling`], the slot untouched.
    #[allow(clippy::too_many_arguments)]
    fn invoke_static_settling_keyed(
        &mut self,
        owner: Option<ClassId>,
        settled: Option<ClassId>,
        door_key: &SettledDoorKey,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> (MethodCallResult, Option<ClassId>) {
        let _ = door_key;
        self.invoke_static_settling(owner, settled, class_name, method_name, descriptor, args)
    }

    /// Whether a settling static door whose settled owner lives in a heap
    /// slot of the handle object (the `--compatible` carrier handle's
    /// `MH_STATIC_SETTLED`, which has no Rust record to hold a
    /// [`SettledDoorKey`]) should keep its door key in a per-thread side
    /// line and call [`Self::invoke_static_settling_keyed`]. Per VM: `true`
    /// only while both `CRATONVM_THROWABLE_SERVED_RAISE_CARRIER_DOOR_KEY` and
    /// `CRATONVM_THROWABLE_SERVED_RAISE_SETTLED_DOOR_KEY` are on (the second
    /// is what makes the VM read and fill the slot). Interpreter round i2
    /// wave 66, lane L3 (proposal
    /// `i65-L3-proposal-the-carrier-handle-keeps-its-door-key`).
    ///
    /// Default: `false` (the carrier calls [`Self::invoke_static_settling`]).
    fn carrier_settled_door_key(&self) -> bool {
        false
    }

    // -- LinkResolver wiring for Java-side reflection natives -----------
    //
    // Round 9 audit fix (HIGH #7): Java-side reflection
    // (`Class.getDeclaredMethod`, `Class.getMethod`,
    // `Class.getDeclaredField`, `Class.getField`) re-walks the entire
    // metadata + hierarchy on every probe. Spring / Hibernate /
    // ByteBuddy run thousands of identical `(class_id, name, desc)`
    // probes during cold start. Round-8 wired JNI's `GetMethodID` /
    // `GetFieldID` into the per-VM `LinkResolver`; these helpers
    // extend the same cache to the Java-side natives.
    //
    // The trait methods are intentionally narrow: probe + insert.
    // The native code keeps owning the actual hierarchy walk (it has
    // synthetic-method awareness, ByteBuddy reentrancy guards, and
    // shim short-circuits that the LinkResolver layer doesn't model).
    // The default implementations are no-ops so the existing
    // `MockNativeContext` and other test contexts compile unchanged
    // and behave as if the cache is permanently empty (correct but
    // slow — no caching).
    //
    // The `index_or_slot` payload is overloaded:
    //   * for **method** lookups it is the position within the
    //     declaring class's `methods` vec (matches the JNI shape);
    //   * for **field** lookups it is the absolute heap slot index
    //     (matches `LinkResolver`'s `absolute_index`).
    // The boolean is `is_static` (only meaningful for fields; ignored
    // for methods).

    /// Probe the per-VM `LinkResolver` for a previously-resolved
    /// reflective method `(class_id, name, descriptor)` triple. Returns
    /// `Some((declaring_class_id, method_index))` on cache hit,
    /// `None` on cold miss (caller should walk the hierarchy and call
    /// [`Self::link_resolver_insert_method`] with the result).
    fn link_resolver_get_method(
        &self,
        _class_id: ClassId,
        _name: &str,
        _descriptor: &str,
    ) -> Option<(ClassId, u32)> {
        None
    }

    /// Populate the LinkResolver method cache. `index` is the position
    /// of the resolved method inside `declaring`'s `methods` vec.
    fn link_resolver_insert_method(
        &self,
        _class_id: ClassId,
        _name: &str,
        _descriptor: &str,
        _declaring: ClassId,
        _index: u32,
    ) {
    }

    /// Probe the LinkResolver field cache. Returns
    /// `Some((declaring_class_id, absolute_index, is_static))` on hit.
    fn link_resolver_get_field(
        &self,
        _class_id: ClassId,
        _name: &str,
        _descriptor: &str,
    ) -> Option<(ClassId, u32, bool)> {
        None
    }

    /// Populate the LinkResolver field cache.
    fn link_resolver_insert_field(
        &self,
        _class_id: ClassId,
        _name: &str,
        _descriptor: &str,
        _declaring: ClassId,
        _absolute_index: u32,
        _is_static: bool,
    ) {
    }

    /// Invoke a virtual method on a receiver, with lambda-proxy awareness.
    ///
    /// If the receiver's ClassId is registered as a lambda proxy, this performs
    /// lambda dispatch (reading captures, dispatching by MethodHandle kind).
    /// Otherwise, it resolves the receiver's class name and performs normal
    /// method invocation.
    ///
    /// `args` does NOT include the receiver — the implementation prepends it.
    fn invoke_virtual(
        &mut self,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult;

    /// JVMS 5.4.6 selection of `method_name` / `descriptor` resolved in
    /// `owner` for `receiver`'s class, when it ENDS IN AN ERROR: the thrown
    /// `AbstractMethodError` (the selected method is abstract) or
    /// `IncompatibleClassChangeError` (conflicting maximally-specific
    /// defaults). `None` when selection finds a body (or cannot judge).
    ///
    /// For `Method.invoke`, whose virtual door dispatches by the receiver's
    /// class and so never sees the resolved method's owner (round 12 wave 8
    /// orchestrator, `r12w8-orch-probe-findings-on-w8` item 1). Mock/test
    /// contexts answer `None`.
    fn virtual_selection_failure(
        &mut self,
        _receiver: ObjectRef,
        _owner: ClassId,
        _method_name: &str,
        _descriptor: &str,
    ) -> Option<MethodCallFailed> {
        None
    }

    /// Invoke a virtual method whose declaring class is known by the caller.
    ///
    /// Most callers should use [`Self::invoke_virtual`]. Method-handle dispatch
    /// has one extra piece of information, though: the owner class stored in the
    /// handle. VM contexts can use that as a recovery target when receiver-based
    /// dispatch collapses to bare `java/lang/Object` for a non-Object member.
    /// Mock/test contexts keep the simple virtual behavior by default.
    fn invoke_virtual_declared(
        &mut self,
        _declared_class: &str,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        self.invoke_virtual(receiver, method_name, descriptor, args)
    }

    /// [`Self::invoke_virtual_declared`] with the declaring class known by id
    /// (`owner`, a method handle's recorded owner), so the VM finds it on the
    /// receiver's superclass chain by id rather than by `declared_class`'s
    /// name (interpreter round i6 wave 118, lane L4;
    /// `CRATONVM_MH_VIRTUAL_BY_OWNER_ID`). `declared_class` is still passed for
    /// the by-name fallbacks. Default: the by-name method.
    fn invoke_virtual_declared_by_class_id(
        &mut self,
        _owner: ClassId,
        declared_class: &str,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        self.invoke_virtual_declared(declared_class, receiver, method_name, descriptor, args)
    }

    /// [`Self::invoke_virtual_declared`] for a `receiver` the caller has
    /// established is a plain object (not an array) whose class IS the
    /// declared class `declared_class` names (its header class id equals the
    /// declaring class's id). For such a receiver §5.4.6 selection against
    /// the declared method cannot move away from the receiver-class walk, so
    /// the VM skips that selection question (a class-manager read and a name
    /// compare that answer "no move" for it); the pins, the dispatch and the
    /// `NoSuchMethodError`-on-`Object` retry are the declared door's. The
    /// served `Method.invoke` calls it under
    /// `CRATONVM_REFLECT_INVOKE_PLAN_FACTS` (interpreter round i3 wave 77,
    /// lane L3). Default: [`Self::invoke_virtual_declared`] -- the same answer.
    fn invoke_virtual_exact(
        &mut self,
        declared_class: &str,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        self.invoke_virtual_declared(declared_class, receiver, method_name, descriptor, args)
    }

    /// Invoke a virtual method on `receiver`, skipping the native-override
    /// check entirely so a registered Rust native for this exact
    /// (class, method, descriptor) triple is NOT re-entered — dispatch goes
    /// straight to the receiver's real JDK bytecode.
    ///
    /// This exists for natives that must distinguish a genuinely-real object
    /// from a same-named synthetic one by *instance* state rather than by
    /// class name (registration is static/global and can't make that call).
    /// The canonical example is `ThreadPoolExecutor.execute`/`submit`/
    /// `shutdown`: CratonVM's synthetic `Executors.newSingleThreadExecutor()`
    /// et al. stamp their 2-field placeholder with the REAL `ThreadPoolExecutor`
    /// class name, so a real executor (e.g. the internal async worker pool in
    /// `native-builtins`) and a synthetic one are indistinguishable by class
    /// name alone. The native override checks `executor_has_real_workers`
    /// per-instance and calls this method for a real receiver instead of
    /// recursing back into itself via [`Self::invoke_virtual`] (which would
    /// hit the same native registration again and loop forever).
    ///
    /// Default implementation falls back to [`Self::invoke_virtual`] — safe
    /// for any context that has no such real/synthetic ambiguity to resolve
    /// (mocks, tests, other native contexts).
    fn invoke_virtual_bytecode_only(
        &mut self,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        self.invoke_virtual(receiver, method_name, descriptor, args)
    }

    /// Invoke a method with invokespecial semantics — *exactly* the resolved
    /// method on `class_name`, with no virtual dispatch and no interface
    /// retarget to the receiver's concrete class.
    ///
    /// This is the dispatch contract behind `MethodHandles.Lookup.findSpecial`
    /// and the JLS `super.m()` call sequence. Use cases:
    ///
    /// - private-to-private calls within the same class
    /// - default-method super calls: `Lookup.findSpecial(I.class, "m", mt, C.class)`
    ///   on an `I.super.m()` pattern must invoke `I.m()`, NOT C's overriding
    ///   `m()` — even though the receiver is a concrete `C`.
    ///
    /// `args[0]` MUST be the receiver. Parameters follow.
    ///
    /// The default implementation falls back to [`Self::invoke`] for
    /// implementations that do not need super-call semantics. The `Vm`
    /// override bypasses the iface/abstract retarget that `invoke_on_class_shared`
    /// applies, so it is correct for super-call invocation.
    fn invoke_special(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        self.invoke(class_name, method_name, descriptor, args)
    }

    /// [`Self::invoke_special`] for a caller that ALREADY holds the declaring
    /// class's resolved `ClassId`.
    ///
    /// Same rationale as [`Self::invoke_by_class_id`] (see its doc comment for
    /// the full argument), applied to the invokespecial dispatch contract.
    /// `invoke_special` resolves `class_name` through the loader-blind global
    /// lookup, which collapses to ONE copy per binary name - so a caller that
    /// knows the exact declaring class must not throw that identity away.
    ///
    /// The concrete bug this exists for: `Method.invoke` routes a *private* or
    /// *cross-package package-private* instance method through
    /// `invoke_special` (both must bypass virtual dispatch - JLS 8.4.8.1).
    /// The `Method` mirror's own `clazz` slot already names the exact declaring
    /// class, but the name-based re-resolution picked the APPLICATION-loader
    /// copy whenever an isolating loader (Spring Boot's
    /// `ModifiedClassPathClassLoader` under `@ForkedClassPath`) had defined its
    /// own copy of that class. See servletcontextlistener-forkedclasspath-mockito-notamock-FIXED.md.
    ///
    /// Default implementation falls back to the name-based
    /// [`Self::invoke_special`] for contexts with no ClassId fast path.
    fn invoke_special_by_class_id(
        &mut self,
        class_id: ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        let _ = class_id;
        self.invoke_special(class_name, method_name, descriptor, args)
    }

    /// Like [`Self::invoke_special`] but for a native that IS ITSELF the
    /// native registered for `(class_name, method_name, descriptor)` and
    /// must run that class's own real bytecode body directly.
    ///
    /// [`Self::invoke_special`] re-finds ANY registered native FIRST (see its
    /// own contract) -- calling it from inside that same native re-enters it
    /// (unbounded Rust-stack recursion). This variant skips that check
    /// entirely and resolves straight to `class_name`'s own bytecode, still
    /// with true invokespecial semantics: static binding on `class_name`'s
    /// hierarchy, never virtual dispatch to a receiver's overriding
    /// subclass. That distinction is the whole point -- the bug this exists
    /// to fix was a native registered on `ThreadPoolExecutor.shutdown()`,
    /// reached via `ScheduledThreadPoolExecutor.shutdown()`'s
    /// `super.shutdown()`, which used `invoke_virtual_bytecode_only` (dynamic
    /// receiver class) and so re-dispatched straight back into the STPE
    /// override that called it -- `StackOverflowError` from infinite
    /// self-recursion.
    ///
    /// For an instance method, `args[0]` is the receiver. For a static method,
    /// `args` contains only its parameters (as for `SecureRandom.getInstance`).
    ///
    /// Default implementation falls back to [`Self::invoke_special`] -- safe
    /// for any context with no such native-reentrancy hazard (mocks, tests).

    fn invoke_special_bytecode_only(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        self.invoke_special(class_name, method_name, descriptor, args)
    }

    /// [`Self::invoke_special_bytecode_only`] for a caller that ALREADY holds the
    /// declaring class's resolved `ClassId`.
    ///
    /// Preserves loader identity when dispatching to real bytecode directly,
    /// preventing fallback to `load_class_concurrent` under the Application loader.
    fn invoke_special_bytecode_only_by_class_id(
        &mut self,
        class_id: ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        let _ = class_id;
        self.invoke_special_bytecode_only(class_name, method_name, descriptor, args)
    }

    /// [`Self::invoke_special_bytecode_only_by_class_id`] with a
    /// declared-method index hint the implementation keeps in `*method_hint`
    /// across calls, for a native that re-enters its OWN JDK body on every
    /// miss (a memo `Intrinsic`). Interpreter round i3 wave 76, lane L4
    /// (`CRATONVM_NATIVE_OWN_BODY_HINT`, default off; proposal
    /// `i75-L4-proposal-a-native-re-entry-into-its-own-body-by-resolved-method-20261108`).
    ///
    /// The caller starts with `None` and passes back whatever the previous
    /// call left. The VM verifies the index against the class's current
    /// methods on every call (name, descriptor, non-abstract), so a stale or
    /// foreign hint costs one re-resolution by name, never a wrong method. A
    /// hint is only meaningful for the VM and `class_id` it was learnt for;
    /// keep it keyed by both.
    ///
    /// Default implementation ignores the hint (leaves it `None`) and falls
    /// back to [`Self::invoke_special_bytecode_only_by_class_id`].
    fn invoke_special_bytecode_only_hinted(
        &mut self,
        class_id: ClassId,
        method_hint: &mut Option<u32>,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        *method_hint = None;
        self.invoke_special_bytecode_only_by_class_id(
            class_id,
            class_name,
            method_name,
            descriptor,
            args,
        )
    }
}

/// The descriptor byte that means "do not decode this slot, hand me the bits".
///
/// A JVM field descriptor's first byte is always one of `B C D F I J S Z L [`
/// (JVMS §4.3.2). `0` is none of them and cannot be produced by a well-formed
/// class file, so it is unambiguous as a sentinel.
///
/// It is load-bearing for [`NativeHeapAccess::get_field_raw`]: the descriptor
/// -aware decode in `gc::heap::coerce_field_value_for_slot` is a `match` on
/// this byte whose final arm is `_ => value`, so an unrecognised byte returns
/// the slot verbatim AND — because every coercion-loss report lives inside one
/// of the recognised arms — reports nothing to the G30 instrument. If a future
/// arm is ever added for `0`, `get_field_raw` silently stops being raw; that is
/// why the byte is named here rather than spelled inline at the call, and why
/// `a_raw_read_uses_a_byte_that_is_not_a_jvm_descriptor` pins it.
pub const RAW_SLOT_DESCRIPTOR: u8 = 0;

/// A receiver **validated once**, for a read-only region a safepoint cannot
/// cross.
///
/// # The cost this exists to remove
///
/// Every `NativeHeapAccess` read accessor is, at minimum, a
/// `is_object_address` membership probe and a forwarding barrier; for a field
/// it is also a layout lookup, a `check_field_index` and a descriptor
/// coercion. MEASURED on the 2026-09-20 `dev` build, `ChmProbe get`
/// (5 M `ConcurrentHashMap<Integer,Integer>.get`, `perf record -F 299` self
/// time): `is_object_address` **15.3 %** of the whole run across its two
/// layers, the layout lookups ~10 %, `check_field_index` 2.8 %,
/// `coerce_field_value_for_slot` 2.9 %, `load_and_forward_inner` 3.0 %. One
/// `get` makes roughly ELEVEN such accesses, and the probe is a random word of
/// a bitmap sized one bit per eight arena bytes — a cache miss, not an
/// instruction count. See
/// `retired/perf-a-collection-native-pays-the-checked-heap-api-per-field-20260920-RETIRED-20260921.md`.
///
/// Removing accesses one at a time does NOT work and was measured twice: wave
/// 12 removed three of the fifteen and both binaries came back flat inside a
/// ±5–9 % control arm. The lever is the cost of one access, which is why this
/// is a REGION and not another fused accessor.
///
/// # Why this is sound, and how the claim is CHECKED rather than asserted
///
/// A cursor borrows the context immutably (`&self`). Every `NativeContext`
/// method that can allocate, call Java, pin, or otherwise reach a safepoint
/// takes `&mut self` — so while a cursor is alive the borrow checker refuses
/// all of them. The "no allocation, no Java, no pins" premise that natives like
/// `chm_wrapper_chain_walk` already state in prose is, inside a region, a
/// compile error rather than a comment. Nothing can move, so the forwarding
/// barrier and the membership probe are re-answering a question the region's
/// entry already answered.
///
/// [`NativeHeapAccess::reached`] extends that to references READ through a
/// cursor: a reference slot of a live object holds a live object, and no
/// safepoint has intervened, so it needs no probe of its own. This is the arm
/// that removes most of the eleven.
///
/// # What a cursor is NOT
///
/// It is not a pin and not a root. It must not be stored, returned, or held
/// across anything that could collect — and it cannot be, because its lifetime
/// is the immutable borrow. An index passed to a cursor read is still the
/// caller's obligation exactly as it is for [`NativeHeapAccess::get_field`];
/// the implementation still bounds-checks it.
///
/// # The default is the checked path
///
/// [`Self::trusted`] is `0` for any implementor that does not override
/// [`NativeHeapAccess::read_cursor`] — mocks, test contexts, any heap with no
/// cheaper answer — and every cursor accessor then delegates to the ordinary
/// checked accessor. So a native written against this API is correct on every
/// implementor and merely faster on the one that overrides it.
#[derive(Clone, Copy, Debug)]
pub struct ReadCursor<'a> {
    /// The validated, forwarded reference. Always safe to hand to the checked
    /// accessors.
    pub(in crate::registry) obj: ObjectRef,
    /// Implementation-private token. `0` means "no trusted state — use the
    /// checked path"; the VM's heap puts the object's header address here.
    pub(in crate::registry) trusted: usize,
    /// The region: an immutable borrow of the context, which is what forbids
    /// every `&mut self` (i.e. every safepoint-reaching) method while this
    /// lives.
    pub(in crate::registry) region: std::marker::PhantomData<&'a ()>,
}

