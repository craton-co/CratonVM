// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! When a registered native wins over real bytecode, and when it must not.
//!
//! This VM ships Rust natives for methods that also have real JDK or library
//! bytecode. Choosing between them is a policy, and the policy is large: one
//! `force_native_over_real_jdk_bytecode` decision function plus about seventy
//! `is_*_native_override` predicates, each naming a specific class or library
//! whose bytecode this VM cannot yet run correctly.
//!
//! Two facts here have live dependents and must not be tidied away:
//!
//! * **A registered native wins over real bytecode unconditionally.** The
//!   predicates decide *which* methods are registered as overrides, not whether
//!   an override applies once it exists.
//! * **A native on an abstract class intercepts every subclass.** That is the
//!   superclass walk in `intercept_force_registered_native`, and several
//!   library shims depend on exactly that reach.
//!
//! The `redefine_immune_*` family is the counterweight: once a class has been
//! redefined, its bytecode is by definition newer than any native this VM
//! shipped for it, so the override has to yield. Getting that backwards means
//! a redefinition silently does nothing.
//!
//! The two test modules moved with the code they test — their `use super::X`
//! names this module now.

use cratonvm_native_api::NativeCallSiteExt;
use super::*;

pub(crate) mod hotpath_counts {
    use std::sync::atomic::{AtomicU64, Ordering};
    pub static FORCE_NATIVE_CALLS: AtomicU64 = AtomicU64::new(0);
    pub static RESOLVE_METHOD_REF_CALLS: AtomicU64 = AtomicU64::new(0);
    pub static LOOKUP_LOADER_INITIATED_CALLS: AtomicU64 = AtomicU64::new(0);
    pub static RETARGET_FIELD_CALLS: AtomicU64 = AtomicU64::new(0);
    /// Bytecodes dispatched through the **decoded** handler
    /// (`opcodes::execute_instruction`) — NOT the total executed.
    ///
    /// It was called `TOTAL_INSTRUCTIONS`, and it never counted one. Its only
    /// bump site is the top of `execute_instruction`, which the raw-bytecode
    /// fast path in `execute_frame_from_index` reaches only when it has no arm
    /// for the opcode. Read as a total it under-reports by the fast path's
    /// share — which is the large majority of executed bytecodes — so any
    /// ratio taken against it (per-opcode cost, native-call frequency) came out
    /// inflated by exactly the factor nobody had measured. The name now states
    /// the population it actually samples.
    pub static DECODED_INSTRUCTIONS: AtomicU64 = AtomicU64::new(0);

    pub fn bump(counter: &AtomicU64) {
        if !crate::runtime::env_cache::dbg_hotpath_counts() {
            return;
        }
        let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_power_of_two() || n % 1_000_000 == 0 {
            eprintln!(
                "[hotpath-counts] force_native={} resolve_method_ref={} \
                 lookup_loader_initiated={} retarget_field={} decoded_instr={}",
                FORCE_NATIVE_CALLS.load(Ordering::Relaxed),
                RESOLVE_METHOD_REF_CALLS.load(Ordering::Relaxed),
                LOOKUP_LOADER_INITIATED_CALLS.load(Ordering::Relaxed),
                RETARGET_FIELD_CALLS.load(Ordering::Relaxed),
                DECODED_INSTRUCTIONS.load(Ordering::Relaxed),
            );
        }
    }
}

/// True when `class_name` has been redefined in place by a JVMTI agent
/// (e.g. Mockito's inline mock maker), so its woven bytecode is authoritative
/// and the VM's per-class native/intrinsic shadows must be suppressed — the
/// woven advice has to run for the mock to intercept (matching HotSpot, which
/// always executes the retransformed bytecode).
///
/// Fast-pathed on the global [`any_class_redefined`] flag: until some agent
/// redefines a class (the overwhelming common case) this is a single relaxed
/// atomic load and never touches the class-manager lock. Once armed, it costs
/// one `class_manager` read + a generation lookup, but only at the handful of
/// dispatch sites that were about to serve a native/intrinsic shadow.
#[inline]
pub(crate) fn native_shadow_suppressed_by_redefine(shared: &SharedVm, class_name: &str) -> bool {
    if !crate::classloading::any_class_redefined() {
        return false;
    }
    let cm = shared.classes.class_manager.read();
    native_shadow_suppressed_in(&cm, class_name)
}

/// Reflection-metadata natives on the `java.lang.reflect.*` member types that
/// must stay authoritative even after their declaring class is redefined.
///
/// CratonVM serves these (annotations, parameter annotations, annotation
/// defaults, annotated types) from VM-side structures, NOT from raw class-file
/// bytes a real `sun.reflect.annotation.AnnotationParser` + `ConstantPool`
/// could decode. So the suppress-native-shadow-on-redefine guard — which
/// otherwise correctly cedes a redefined class's methods to their woven
/// bytecode so a Mockito inline mock's advice runs — must NOT fire for these.
///
/// The trigger: `Mockito.mock(java.lang.reflect.Method.class)` inline-redefines
/// `java.lang.reflect.Method`. That bumps its `redefine_generation`, so EVERY
/// subsequent `Method.getDeclaredAnnotations()` / `isAnnotationPresent(...)`
/// call — on ANY method object, not just the mocked class — was routed to the
/// real `Executable.declaredAnnotations()` bytecode, which under CratonVM reads
/// empty annotation bytes and returns no annotations. JUnit's `@Test` scan then
/// finds zero test methods and Spring AOP's `MethodMatchersTests` (whose
/// `static final Method TEST_METHOD = mock(Method.class)` runs at class-init)
/// discovers 0 tests where HotSpot runs 14. A mock only needs its per-INSTANCE
/// dispatch woven; these class-level metadata accessors are not instance
/// behaviour and the mock never stubs them, so keeping the native is correct
/// (and matches HotSpot, where the redefine leaves real annotation reflection
/// intact). Business-method inline mocks (e.g. `InetAddress.getHostName`) are
/// unaffected — they are not in this list.
pub(super) fn redefine_immune_reflection_native(class_name: &str, method_name: &str) -> bool {
    matches!(
        class_name,
        "java/lang/reflect/Method"
            | "java/lang/reflect/Constructor"
            | "java/lang/reflect/Field"
            | "java/lang/reflect/Executable"
            | "java/lang/reflect/AccessibleObject"
    ) && matches!(
        method_name,
        "getDeclaredAnnotations"
            | "getAnnotations"
            | "getAnnotation"
            | "getDeclaredAnnotation"
            | "isAnnotationPresent"
            | "getAnnotationsByType"
            | "getDeclaredAnnotationsByType"
            | "getParameterAnnotations"
            | "getDefaultValue"
            | "getAnnotatedReturnType"
            | "getAnnotatedParameterTypes"
            | "getAnnotatedExceptionTypes"
            | "getAnnotatedReceiverType"
    )
}

/// Methods whose real JDK bodies access compact `byte[]`/`coder`/`count`
/// fields while CratonVM StringBuilder objects intentionally use a synthetic
/// `char[]`/`count` layout. A registered native must win for every one of these
/// operations, including direct methods on StringBuilder rather than only their
/// AbstractStringBuilder implementation.
pub(crate) fn is_string_builder_layout_native_override(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    // `java/lang/StringBuffer` is NOT here, and its absence is load-bearing.
    // Its 62 natives were retired from the real-JDK registrar (see the block
    // at `register_string_builder_natives`'s call site in
    // `native-builtins/src/lib.rs`) so that its own `synchronized` bodies run
    // and supply the monitor and the `toStringCache` invalidation a shared
    // native cannot. Leaving the class here would let this gate resolve
    // `StringBuffer.append` by walking to the INHERITED
    // `AbstractStringBuilder.append` native and running it directly, skipping
    // the `StringBuffer` body entirely — a retirement that is silently undone
    // by the gate that outlived it.
    if !matches!(
        class_name,
        "java/lang/StringBuilder" | "java/lang/AbstractStringBuilder"
    ) {
        return false;
    }
    if matches!(
        method_name,
        // Keep this deliberately narrow: these direct JDK bodies read the
        // incompatible compact-string layout on synthetic builders. Other
        // operations keep their established dispatch to avoid turning the
        // high-volume AOT code-generation path into an all-native slow path.
        //
        // `setCharAt` joined this list once the invoke-cache redefine guards
        // became precise enough to actually evict a stale native shadow for
        // a genuinely real (non-mock) StringBuilder after an UNRELATED
        // Mockito.mock(StringBuilder.class) redefined the class elsewhere in
        // the process: real `AbstractStringBuilder.setCharAt`'s bytecode
        // writes through `String.checkIndex` against the compact
        // byte[]/coder layout, which CratonVM's synthetic builder doesn't
        // have, so it AIOOBE'd instead of writing the synthetic char[].
        //
        // 2026-07-31: the list below `setCharAt` was still incomplete, and
        // `SbMethodMatrixProbe` (Spring-free: run each builder operation on a
        // REAL builder before and after an unrelated
        // `Mockito.mock(StringBuilder.class)`) named the survivors exactly.
        // Six operations silently changed behaviour after the redefinition:
        //
        //   setLength(4)   'abcdefghij' -> len=4 but toString() == "a"
        //   setLength(0)   left one stale char behind
        //   deleteCharAt   'abcdefghij' -> len=9 but toString() == "a"
        //   replace        ArrayStoreException (src=Byte, dest=Char)
        //   ensureCapacity buffer overwritten with spaces
        //   trimToSize     same
        //   repeat         appended nothing
        //
        // Each has a registered native in `register_string_builder_natives`
        // (`setLength(I)V`, `deleteCharAt(I)…`, `replace(IILjava/lang/String;)…`,
        // `ensureCapacity(I)V`, `trimToSize()V`, `repeat(II)…`), so before the
        // redefinition they all dispatched native and were correct; the
        // redefinition evicted the shadow and handed them back to real JDK
        // bodies that index a compact `byte[] value` / `byte coder` /
        // `int count` layout CratonVM's two-field `char[]`/`int` builder does
        // not have. Adding them here is the same trade `setCharAt` already
        // makes: these mutate builder state, and nothing stubs them on a mock.
        // `capacity`, `getCoder`, `getValue`, `reverse` and the `codePoint*`
        // readers join for the same reason — they read `value`/`coder`
        // directly and cannot be expressed against the synthetic layout.
        //
        // `length()` and `substring(int)` stay OUT of this list on purpose;
        // see the long note below. They are the two operations
        // `MockitoBeanByTypeLookupIntegrationTests` genuinely stubs and
        // verifies on a mocked StringBuilder, so their native shadow must
        // stay evictable for Mockito's woven advice to run.
        "<init>"
            | "append"
            | "capacity"
            | "charAt"
            | "codePointAt"
            | "codePointBefore"
            | "codePointCount"
            | "delete"
            | "deleteCharAt"
            | "ensureCapacity"
            | "getChars"
            | "getCoder"
            | "getValue"
            | "insert"
            | "repeat"
            | "replace"
            | "reverse"
            | "setCharAt"
            | "setLength"
            | "toString"
            | "trimToSize"
    ) {
        return true;
    }
    // `length()` FIX (2026-07-23, follow-up to the KNOWN GAP left by the
    // previous session): stop blanket-immunizing "length" for ANY of the
    // three class names -- matching `substring(int)`'s existing treatment
    // exactly (that method is not, and never has been, in this list).
    //
    // Ground truth, captured by dumping Mockito's OWN redefined bytecode on
    // real HotSpot (`-Dnet.bytebuddy.dump=...`, JDK 25, Mockito 5.23.0):
    // `Mockito.mock(StringBuilder.class)` redefines BOTH `StringBuilder`
    // (whose `length()` is a compiler-generated public bridge --
    // `AbstractStringBuilder` is package-private -- confirmed via `javap -p
    // -c java.lang.StringBuilder`: `aload_0; invokespecial
    // AbstractStringBuilder.length:()I; ireturn`, UNCHANGED by redefinition)
    // AND `AbstractStringBuilder` itself, weaving the actual
    // `MockMethodDispatcher.get/isMocked/isOverridden/handle` advice
    // directly into `AbstractStringBuilder.length()`'s own body, ahead of
    // its original `getfield count:I` tail. So blanket-forcing native for
    // `AbstractStringBuilder.length()` (an earlier version of this fix kept
    // that arm immune, reasoning the bridge alone was the redefined method,
    // by analogy with `substring`) permanently pre-empted the advice for
    // BOTH a mock AND a real receiver of the class -- the STRINGBUILDER
    // bridge's `invokespecial` reached `AbstractStringBuilder.length()`,
    // which our own force-native gate intercepted before Mockito's advice
    // ever got to run. Removing immunity here (verified against
    // `InvocationCountProbe`, which reflects
    // `Mockito.mockingDetails(mock).getInvocations()`) now byte-for-byte
    // matches real HotSpot's `length()`/`substring(0)`/`verify()` sequence.
    //
    // KNOWN REMAINING GAP: a REAL (non-mock) receiver's `.length()`, called
    // AFTER some OTHER StringBuilder has been Mockito-redefined ANYWHERE in
    // the process, now falls through the woven advice's "not mocked" branch
    // into `AbstractStringBuilder.length()`'s original `getfield count:I` --
    // which reads the wrong field index against CratonVM's 2-field
    // (`char[]`, `int`) synthetic layout (real JDK's compiled class expects
    // `value`/`coder`/`count` at indices 0/1/2) and silently returns `0`
    // instead of the real length (confirmed via a dedicated probe:
    // `RealAfterMockLengthProbe`, `/data/tmp/mockitobean-substring-20260723/`).
    // This is the EXACT SAME latent risk `substring(int)` has carried,
    // unaddressed, since bug 3 of this class's fix history -- not a
    // regression this change introduces, just the same known tradeoff now
    // also applying to `length()`. Fixing it for real needs an authoritative
    // per-instance "is this receiver actually mocked" signal reachable from
    // Rust WITHOUT re-entering bytecode dispatch for the same (class,
    // method) pair (a naive `MockUtil.isMock` + re-invoke attempt during
    // this session's investigation infinite-looped, since re-invoking
    // "this method's bytecode" from inside the very native registered for
    // it re-triggers the identical force-native decision) -- left open, not
    // hit by any currently-passing suite class.
    if method_name == "length" && method_descriptor == "()I" {
        return false;
    }
    // `substring(int, int)` -- deliberately excludes `substring(int)`.
    // `substring(int)` must stay evictable: `MockitoBeanByTypeLookup*
    // IntegrationTests` explicitly stubs/verifies `.substring(anyInt())`
    // on a Mockito-mocked StringBuilder, which only works if the redefine
    // guards can drop this method's native shadow so the woven advice
    // actually runs (see the `Native{}` cache-hit redefine guard). But
    // `substring(int, int)`'s native shadow needs the SAME layout-safety
    // forcing as `setCharAt` above for a REAL (non-mock) receiver: Mockito
    // itself calls `new StringBuilder(...).substring(start, end)` inside
    // `StringUtil.join` (`Reporter.unfinishedVerificationException`'s
    // message formatting) on its own internal, never-mocked StringBuilder,
    // and once ANY StringBuilder in the process gets Mockito-redefined,
    // real `AbstractStringBuilder.substring(int,int)` bytecode AIOOBE'd
    // reading the incompatible compact layout -- masking the ACTUAL
    // "unfinished verification" failure behind a crash in the exception
    // message it was trying to construct. No test in this suite stubs or
    // verifies the two-arg overload, so forcing it native is safe.
    method_name == "substring" && method_descriptor == "(II)Ljava/lang/String;"
}

/// The layout-incompatible natives, for the invoke-cache dispatch sites.
///
/// These sites must not use `redefine_immune_forced_native`. Broadening them to
/// the full set — which additionally covers reflection metadata, JFR, BC crypto,
/// StampedLock and FileHandler — was measured on 2026-07-31 and **regressed**
/// ByteBuddy type creation: Spring AOT chunk 3 started failing roughly one run
/// in eight with `NoSuchMethodError: java.lang.Integer.isArray()Z` /
/// `Integer.represents(Type)Z` out of
/// `TypeDescription$Generic$Visitor$Substitutor`, a signature that appears in no
/// pre-change log across three full sweeps. 9/9 clean on the unmodified binary
/// under the same harness, 8/9 with the broadening. Those extra arms exist for
/// the *slow* path and are not safe to assert here.
///
/// What every member of this set has in common is narrower and checkable: the
/// receiver's real JDK body indexes a field layout CratonVM's object does not
/// have, so running it can only produce nonsense — whatever else is true about
/// the class.
pub(super) fn redefine_immune_layout_native(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    // The ZIP arm is DELIBERATELY absent here, and this is the one arm whose
    // two aggregators must NOT match.
    //
    // Every other member of this set is decided by the CLASS: a synthetic
    // collection, a VM-minted carrier, a `ThreadLocal`, a `StringBuilder` — for
    // those, every instance of the class is in the same position and a
    // receiver-blind cache entry is a correct one. `ZipFile`/`JarFile` are not:
    // a REAL archive keeps its handle in `jar_table()` and must stay on the
    // native, while a Mockito INLINE mock of the same class has no handle at
    // all and must reach the woven advice. Same class, same method, opposite
    // answers — which is exactly what a per-call-site cache cannot express, and
    // caching either answer is wrong for the other receiver.
    //
    // So the cache paths refuse to decide: with the arm gone, a redefined
    // `ZipFile`/`JarFile` fails their immunity check, the cached native is not
    // installed (or is evicted), and dispatch falls through to the slow path —
    // where `redefine_immune_forced_native_for_receiver` HAS the receiver and
    // answers per instance. A real archive still gets its native there; only
    // the fast path is given up, and only in a process that mocked one of these
    // two classes.
    //
    // `leaves_the_zip_arm_to_the_receiver_aware_slow_path` pins this, and says
    // so, because the standing rule for this file is the opposite one
    // (`thread_local_immunity_reaches_the_invoke_cache_sites_too`).
    redefine_immune_string_builder_native(class_name, method_name, method_descriptor)
        || redefine_immune_path_native(class_name, method_name, method_descriptor)
        || redefine_immune_synthetic_collection_native(class_name)
        || redefine_immune_vm_minted_carrier_native(class_name)
        || redefine_immune_thread_local_native(class_name)
}

pub(super) fn redefine_immune_string_builder_native(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    is_string_builder_layout_native_override(class_name, method_name, method_descriptor)
}

pub(super) fn redefine_immune_path_native(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    class_name == "java/nio/file/Path"
        && method_name == "toString"
        && method_descriptor == "()Ljava/lang/String;"
}

pub(super) fn redefine_immune_jfr_native(
    _class_name: &str,
    _method_name: &str,
    _method_descriptor: &str,
) -> bool {
    false
}

/// `ZipFile`/`JarFile` archives live in a Rust handle table, not in the JDK's
/// `res`/`zsrc` field graph.
///
/// CratonVM serves the operations listed below from registered natives whose
/// state is a `jar_table()` entry keyed by a handle stashed on the object
/// (`native-io/src/zip_real_jar.rs`). The real `java.util.zip.ZipFile` body
/// reads `this.res.zsrc` -- a `CleanableResource` those natives never
/// populate -- so it can only ever NPE, for every archive the process has
/// opened. Exactly the shape of the synthetic-collection and `ThreadLocal`
/// arms above, and exactly the same trigger: Mockito's inline mock maker
/// instruments its target's whole superclass chain, so `spy()` of ANY
/// `JarFile` subclass retransforms `java.util.jar.JarFile` and
/// `java.util.zip.ZipFile` themselves, the suppress-native-shadow-on-redefine
/// rule fires, and the next `getInputStream` on ANY jar in the process runs
/// the real body against an instance that has no `res`.
///
/// Found by Spring Boot's `JarUrlConnectionTests`: 46 of its 47 tests pass,
/// and the 47th fails only when it runs after
/// `getInputStreamWhenNoCachedClosesJarFileOnClose`, the one test in the class
/// that calls `spy(jarFile)`:
///
/// ```text
/// java.lang.NullPointerException: Cannot read field "zsrc" because "<local3>.res" is null
///     at java.util.zip.ZipFile.getInputStream(ZipFile.java:327)
///     at java.util.jar.JarFile.getInputStream(JarFile.java:834)
/// ```
///
/// Method-wise rather than class-wide, and deliberately keyed to the SAME
/// method sets the two force-native gates already use (the `java/util/zip/
/// ZipFile` arm in this file's warm-cache policy and the `java/util/jar/
/// JarFile` arm in the cold-path twin). That is the principled boundary: a
/// method is forced to its native because the instance lacks the JDK field
/// graph it needs, and a redefinition cannot conjure that field graph -- so
/// every force-native method here is immune, and nothing else is. A `JarFile`
/// method NOT on that list is ordinary bytecode and stays evictable, so an
/// agent can still weave it.
/// `System.identityHashCode(obj)` computed from `shared` alone.
///
/// Byte-identical to `NativeContextImpl::identity_hash_code` (and therefore to
/// the `java/lang/System.identityHashCode` native, which is that method's only
/// caller), because the side table this key indexes is written from the native
/// side and read from here. A different derivation would look up under a key
/// nothing ever wrote.
pub(crate) fn vm_identity_hash(shared: &SharedVm, obj: ObjectRef) -> i32 {
    let heap = &shared.mem.heap;
    let heap_answer = heap.identity_hash_code(obj);
    shared
        .threads
        .monitors
        .java_identity_hash(obj, heap_answer, || heap.next_identity_hash())
}

/// Does the `ZipFile`/`JarFile` redefine immunity have to stand DOWN for this
/// receiver?
///
/// [`redefine_immune_zip_file_native`] is class-and-method scoped and says so:
/// a method is forced to its native "because the instance lacks the JDK field
/// graph it needs, and a redefinition cannot conjure that field graph". That
/// is the right rule for a real archive and the wrong one for a receiver that
/// is not an archive at all.
///
/// Mockito's inline mock maker mocks `java.util.jar.JarFile` by retransforming
/// `JarFile` and `ZipFile` themselves and instantiating the target through
/// objenesis -- no constructor runs, so `open_and_register` never gave the
/// object a handle. The immunity then keeps a native that has nothing to
/// answer from: `mock(JarFile.class).getName()` returns `null` and `.close()`
/// does nothing, and neither call is RECORDED, so
/// `then(jarFile).should().close()` surfaces as an unfinished verification on
/// the NEXT `mock()` in the class. Spring Boot's `UrlJarFilesTests` fails two
/// tests that way, each naming an EARLIER test whose `close()` never landed.
///
/// So the immunity is asked one more question -- is this receiver an archive
/// we actually opened? -- and stands down when the answer is no, letting
/// dispatch fall through to the woven bytecode where the mock advice lives.
///
/// Two deliberate narrowings:
///
///  * `<init>` is NEVER waived. A real archive's handle is registered BY the
///    constructor, so at its entry the receiver necessarily has none yet;
///    waiving there would cede every real `new JarFile(...)` in a redefined
///    process to a JDK body that cannot open it.
///  * The receiver must be in hand. At a gate that does not carry one the
///    immunity stands, which is the pre-existing behaviour.
///
/// The rows behind `identity_is_known_archive_in` outlive `close()` (they go
/// only when the archive object itself dies, gc-common w17-c), so a CLOSED
/// archive still counts as real -- `close` is on the immunity list and a
/// double-close must not reach `ZipFile.close`'s real body, which reads the
/// `res` field CratonVM never populates.
pub(crate) fn zip_immunity_waived_for_receiver(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    receiver: Option<ObjectRef>,
) -> bool {
    if method_name == "<init>" {
        return false;
    }
    if !redefine_immune_zip_file_native(class_name, method_name) {
        return false;
    }
    let Some(obj) = receiver else {
        if dbg_zip_immune().is_some() {
            eprintln!("[zipimmune] {class_name}.{method_name} waiver: NO RECEIVER");
        }
        return false;
    };
    // gc-common w17-c: the known-archive rows are per VM (the identity hash
    // is per VM, and the set used to be process-wide, so another VM's archive
    // with the same hash made a mock look real). gc-common w29-c: and per
    // object -- the row is found by the receiver's current address too, so a
    // mock whose identity hash equals a LIVE real archive's no longer looks
    // real. `obj` comes off the operand stack at dispatch, so it is current;
    // the stop-the-world epilogue re-files a moved archive's row before any
    // mutator resumes.
    let known = cratonvm_native_io::zip_real_jar::identity_is_known_archive_in(
        shared.vm_identity,
        vm_identity_hash(shared, obj),
        obj.as_ptr() as usize,
    );
    if dbg_zip_immune().is_some() {
        eprintln!(
            "[zipimmune] {class_name}.{method_name} waiver: known_archive={known} -> waived={}",
            !known
        );
    }
    !known
}

/// [`redefine_immune_forced_native`] with the receiver-aware ZIP waiver
/// applied. Every gate that HAS the receiver calls this instead.
pub(crate) fn redefine_immune_forced_native_for_receiver(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    receiver: Option<ObjectRef>,
) -> bool {
    redefine_immune_forced_native(class_name, method_name, method_descriptor)
        && !zip_immunity_waived_for_receiver(shared, class_name, method_name, receiver)
}

/// `CRATONVM_DBG_ZIPIMMUNE` — the ZIP redefine-immunity lever. Two values:
///
///  * `1` — trace: one line per distinct (class, method) the immunity is
///    consulted for, plus one per waiver decision. This is what named the last
///    receiver-blind gate: a `java/util/zip/ZipFile` consultation printed with
///    no waiver line beside it is a caller that is not asking about the
///    receiver.
///  * `off` — force the immunity to `false` everywhere. Behaviour-changing, and
///    the point: it separates "this gate decides" from "some other gate
///    decides" in one run, which three rounds of converting gates one at a time
///    could not.
///
/// Both are inert unless set, and the read is one `OnceLock`.
fn dbg_zip_immune() -> Option<&'static str> {
    use std::sync::OnceLock;
    static C: OnceLock<Option<String>> = OnceLock::new();
    C.get_or_init(|| Err::<String, std::env::VarError>(std::env::VarError::NotPresent).ok())
        .as_deref()}

pub(super) fn redefine_immune_zip_file_native(class_name: &str, method_name: &str) -> bool {
    let hit = redefine_immune_zip_file_native_inner(class_name, method_name);
    if let Some(mode) = dbg_zip_immune() {
        if hit {
            use parking_lot::Mutex;
            use std::sync::OnceLock;
            static SEEN: OnceLock<Mutex<std::collections::BTreeSet<(String, String)>>> =
                OnceLock::new();
            let seen = SEEN.get_or_init(|| Mutex::new(Default::default()));
            if seen
                .lock()
                .insert((class_name.to_string(), method_name.to_string()))
            {
                eprintln!("[zipimmune] {class_name}.{method_name} immune=true mode={mode}");
            }
        }
        if mode == "off" {
            return false;
        }
    }
    hit
}

fn redefine_immune_zip_file_native_inner(class_name: &str, method_name: &str) -> bool {
    match class_name {
        "java/util/zip/ZipFile" => matches!(
            method_name,
            "<init>"
                | "getEntry"
                | "getInputStream"
                | "entries"
                | "stream"
                | "getComment"
                | "close"
                | "getName"
                | "isMultiRelease"
                | "size"
        ),
        "java/util/jar/JarFile" => matches!(
            method_name,
            "<init>"
                | "getManifest"
                | "getManifestFromReference"
                | "stream"
                | "entries"
                | "getEntry"
                | "getJarEntry"
                | "getInputStream"
                | "size"
                | "close"
                | "getName"
        ),
        _ => false,
    }
}

/// The full immunity set, for the slow dispatch path.
///
/// The invoke-cache sites deliberately use the narrower
/// `redefine_immune_layout_native` instead — see the note there for the
/// measurement that says why. What both must share is the layout arm: when the
/// 2026-07-31 collections entry was added here only, the collection probe went
/// from 32 broken operations to 18 rather than to 0, because the cache sites
/// re-assembled their own chain and never saw it.
/// `layout_immunity_is_not_open_coded` keeps the two in step.
pub(crate) fn redefine_immune_forced_native(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    redefine_immune_reflection_native(class_name, method_name)
        || redefine_immune_string_builder_native(class_name, method_name, method_descriptor)
        || redefine_immune_path_native(class_name, method_name, method_descriptor)
        || redefine_immune_jfr_native(class_name, method_name, method_descriptor)
        // java.util.logging.FileHandler's registered natives store their
        // filename/closed bookkeeping in an identity-hash side table
        // (jul_file_handler_state_table, native-builtins/src/
        // logging_shims.rs) rather than real instance field slots. Real
        // FileHandler bytecode (loaded from java.base) is concrete, so
        // without this entry the default rule ran its REAL <init>()V /
        // <init>(String)V -- which try to actually open/lock a real log
        // file via NIO and throw NoSuchFileException -- instead of the
        // registered native. Keep in sync with vm_exec.rs's
        // invoke_on_class_shared_inner check_override entry for the same
        // triples; see
        // filehandler-noarg-ctor-handler-field-layout-gap-FIXED.md.
        || (class_name == "java/util/logging/FileHandler"
            && matches!(
                method_name,
                "<init>" | "publish" | "flush" | "close"
            ))
        || redefine_immune_synthetic_collection_native(class_name)
        || redefine_immune_vm_minted_carrier_native(class_name)
        || redefine_immune_thread_local_native(class_name)
        || redefine_immune_zip_file_native(class_name, method_name)
}

/// CratonVM implements these collections as small synthetic objects — a bucket
/// array plus a size, not the JDK's `table`/`root`/`head` field graph — and
/// every operation on them is a registered native. Their real JDK bodies can
/// therefore NEVER run correctly against an instance CratonVM built, whatever
/// the circumstances.
///
/// That makes them unconditionally immune: a redefinition drops native shadows
/// so an agent's woven bytecode can run, which is right for an ordinary class
/// and catastrophic here. `RedefineCollectionLayoutProbe` measured the damage
/// before this gate existed — **32 of 89 operations** changed behaviour after
/// redefining these classes with their OWN bytes, and the worst of them are
/// silent:
///
/// ```text
/// TreeMap.get               v7  -> null
/// TreeMap.containsKey       true -> false
/// ConcurrentHashMap.get     v7  -> null
/// ConcurrentHashMap.size    12  -> 0
/// ConcurrentHashMap.isEmpty false -> true
/// HashMap.keySet            [k0..k11] -> []
/// ```
///
/// The rest throw — `LinkedHashMap$Node.getKey` NoSuchMethodError,
/// `AnonymousObject$4 cannot be cast to Map$Entry`, `TreeSet` NPEs on a null
/// `this.m`. One `Mockito.mock()` anywhere in the process was enough to arm it,
/// which is how it reached Spring's AOT run: `AnnotationAttributes` extends
/// `LinkedHashMap`, and its `keySet()` NPE'd.
///
/// Unlike the StringBuilder list above, this is class-wide rather than
/// method-wise. There is no analogue of `length()`/`substring(int)` here — no
/// suite stubs a method on a mocked JDK collection, and the cost of being wrong
/// in that direction (one un-stubbed mock) is far below the cost of being wrong
/// in the other (silent data loss on every real collection in the process). If
/// a test ever does need to stub one, narrow this the way the builder list is
/// narrowed, and say which test.
fn redefine_immune_synthetic_collection_native(class_name: &str) -> bool {
    matches!(
        class_name,
        "java/util/ArrayDeque"
            | "java/util/ArrayList"
            | "java/util/HashMap"
            | "java/util/HashSet"
            | "java/util/IdentityHashMap"
            | "java/util/LinkedHashMap"
            | "java/util/LinkedHashSet"
            | "java/util/LinkedList"
            | "java/util/TreeMap"
            | "java/util/TreeSet"
            | "java/util/concurrent/ConcurrentHashMap"
    )
}

/// A class the VM MINTED has no bytecode to yield to, in this or any image.
///
/// `cratonvm/internal/*` names a carrier this VM allocates and serves entirely
/// from registered natives -- `UnmodifiableList`, `UnmodifiableMap`, the
/// iterator and entry-set carriers, `MemorySegmentImpl`, `SystemLogger`. No
/// image contains a class file for any of them, so "drop the native and let the
/// woven bytecode run" cannot mean what it means for a real class: there is no
/// bytecode of theirs to run, and `find_method_recursive` walks past them to
/// whatever ANCESTOR declares the name -- in practice `java/lang/Object`.
///
/// Mockito's inline mock maker retransforms `java.lang.Object` whenever it mocks
/// a class (rather than an interface), which bumps Object's redefine generation
/// for the rest of the process. Without this arm, `vm_exec`'s
/// `native_shadow_dropped_by_redefine` then resolved
/// `cratonvm/internal/UnmodifiableList.equals` to `Object.equals`'s body, saw
/// `has_body && generation > 0`, and dropped the carrier's native -- so every
/// `Collections.unmodifiableList(..)` / `List.of(..)` /
/// `Collections.unmodifiableSet(..)` in the process compared by IDENTITY from
/// the first `mock()` onwards. Measured on `MinAssertRedefine.java`
/// (mock an abstract class, then compare):
///
/// ```text
/// [native-shadow] cratonvm/internal/UnmodifiableList.equals(Ljava/lang/Object;)Z
///                 dropped=true probe=Some((ClassId(0), true, 2)) immune=false
/// ```
///
/// and, one door up, AssertJ's `assertThat(list).isEqualTo(other)` failing with
/// `expected: "[x] (SingletonList@..)" but was: "[x] (UnmodifiableRandomAccessList@..)"`
/// while `list.equals(other)` on the same two objects answered `true` -- the
/// shape recorded for `LoggersEndpointTests` and
/// `CouchbaseAutoConfigurationTests` in the twelve-unclustered Spring residuals.
///
/// Class-wide and name-prefixed on purpose: the property is structural, not a
/// per-method judgement. A `cratonvm/internal/*` receiver never has a real body
/// under it, so there is nothing an agent could have woven into it and nothing
/// to narrow later. An agent that wants to intercept these collections mocks the
/// `java.util` class it sees, which is what the arm above covers.
fn redefine_immune_vm_minted_carrier_native(class_name: &str) -> bool {
    class_name.starts_with("cratonvm/internal/")
}

/// `ThreadLocal`'s values do not live where its real JDK body looks for them.
///
/// CratonVM serves `get`/`set`/`remove`/`initialValue`/`withInitial`/`<init>` on
/// these two classes from registered natives whose store is a Rust
/// thread-local of per-ThreadLocal rows (`TL_MAP`, plus the `withInitial` /
/// `InheritableThreadLocal` side tables, `native-builtins/src/
/// phases_early.rs`). The real `java.lang.ThreadLocal` bytecode reads
/// `Thread.threadLocals` — a `ThreadLocalMap` those natives never populate —
/// so it can only ever answer `null`, for every value the process has set.
///
/// Same shape as the synthetic-collection arm above, and the same trigger:
/// Mockito's inline mock maker instruments the target's whole superclass chain,
/// so `mock()` of ANY `ThreadLocal` subclass — `org.springframework.core.
/// NamedThreadLocal` is the one that found this — retransforms
/// `java.lang.ThreadLocal` itself, the suppress-native-shadow-on-redefine rule
/// fires, and every `ThreadLocal` in the process silently empties. Measured on
/// one binary with the mock target as the only variable
/// (`probes/ThreadLocalRetransformProbe.java`, `--dump-native-registry`):
///
/// ```text
///                                 mock a ThreadLocal subclass | mock any other class
///   ThreadLocal.get native calls                            4 |                  23
///   value set before the mock                            null |              "hello"
///   ThreadLocal.withInitial(() -> TRUE).get()            null |                true
///   probe                                        PROBE-FAIL 4 |           PROBE-OK
/// ```
///
/// Mockito's own internals are the first casualty, which is why the failure
/// reads as "Mockito broke" rather than "ThreadLocal broke":
/// `InlineDelegateByteBuddyMockMaker` holds `ThreadLocal.withInitial(() ->
/// false)` fields, and once those answer `null` the very next
/// `Boolean.booleanValue()` NPEs — inside mock creation, so EVERY subsequent
/// `mock()` in the JVM fails too (89 of 728 in the sweep that found this).
///
/// Class-wide rather than method-wise, for the collection arm's reason: no
/// suite stubs a method on a mocked `ThreadLocal`, and one un-stubbed mock
/// costs far less than silent data loss on every ThreadLocal in the process.
/// A subclass that overrides `initialValue()` is unaffected either way — that
/// override is the subclass's own bytecode and dispatch resolves to it before
/// reaching this gate.
///
/// # `--real-jdk` reads this table too (G5-1, 2026-08-16)
///
/// It is easy to read the paragraphs above as a `Compatible`-mode story. It is
/// not. `register_thread_local_natives` (`native-builtins/src/phases_early.rs`)
/// wraps all six registrations in `set_category(NativeKind::Intrinsic)`, and
/// `resolve_native_dispatch_wave1` (`vm/src/vm/vm_exec.rs`) admits an
/// `Intrinsic` over concrete bytecode under `--real-jdk` as well — §1.4's
/// reviewed exception. So under `--real-jdk`, exactly as under `Compatible`,
/// `Thread.threadLocals` and `Thread.inheritableThreadLocals` are never
/// written by anything in the process.
///
/// That is not only a storage detail. It silently disables three real-JDK
/// behaviours that live in `java.lang.Thread`'s and `java.lang.ThreadLocal`'s
/// own bytecode and have no analogue in `TL_MAP`:
///
/// * the construction-time inheritance copy at pc 175..201 of the master
///   `Thread(ThreadGroup, String, int, Runnable, long)` constructor — its
///   `ifnull` at pc 182 always takes the skip branch;
/// * the `characteristics & 4` opt-out that `Thread(g, r, n, ss, false)` sets;
/// * `InheritableThreadLocal.childValue(T)` overrides, which the JDK applies
///   inside `createInheritedMap`.
///
/// All three are MEASURED divergences on Temurin 25.0.3+9 and are written up,
/// with the demotion nomination that would restore them, in
/// `docs/feature-designs/class-resolution-and-caching.md
/// construction-20260816.md`. Do not "fix" the ITL timing anywhere downstream
/// without reading §5 there first: the workaround it describes lives in
/// `native-builtins/src/lang_system.rs::native_thread_start0`, not here.
fn redefine_immune_thread_local_native(class_name: &str) -> bool {
    matches!(
        class_name,
        "java/lang/ThreadLocal" | "java/lang/InheritableThreadLocal"
    )
}

pub(crate) fn should_force_registered_native_over_bytecode(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    let registry = &shared.natives.native_methods;
    let is_intrinsic = registry
        .resolve_id(class_name, method_name, method_descriptor)
        .and_then(|id| registry.kind_of_id(id))
        == Some(cratonvm_native_api::NativeKind::Intrinsic)
        && !(crate::vm::dispatch_policy(shared).is_jdk_only()
            && cratonvm_native_api::unretire::pure_bytecode_for(class_name, method_name));
    should_force_registered_native_over_bytecode_precomputed(
        shared,
        is_intrinsic,
        class_name,
        method_name,
        method_descriptor,
    )
}

/// Same decision as [`should_force_registered_native_over_bytecode`], but
/// takes the pure/deterministic `force_native_over_real_jdk_bytecode` result
/// as a precomputed input rather than recomputing it. Lets a cached-dispatch
/// call site (which can memoize that ~55-branch check once per invoke-cache
/// entry, see `CachedBytecodeMethod::force_native_cache`) skip straight to
/// the cheap, mutable-state-dependent redefine check.
pub(super) fn should_force_registered_native_over_bytecode_precomputed(
    shared: &SharedVm,
    force_native: bool,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    force_native
        && (!native_shadow_suppressed_by_redefine(shared, class_name)
            || redefine_immune_forced_native(class_name, method_name, method_descriptor))
}

/// [`should_force_registered_native_over_bytecode_precomputed`] with the
/// receiver-aware ZIP waiver applied to the immunity term.
///
/// This is THE gate that decides for `java/util/jar/JarFile.close()V`: the
/// method is a force-native entry, so it never reaches the shadow-drop gates in
/// `invoke_or_native` and `try_stackless_invoke` at all. Patching those two and
/// not this one moves the `[native-shadow]` diagnostic to `dropped=true` and
/// changes nothing about which body runs -- measured 2026-09-10, `mock(JarFile
/// .class).close()` still recorded zero invocations.
///
/// The `real_http_url_connection_native` arm a few hundred lines below is the
/// same idea reached independently: it tells "a genuinely real carrier" from
/// "a mock or synthetic carrier" by checking the receiver's field 0, because an
/// Objenesis-constructed mock never ran a constructor. This asks the archive
/// tables the same question, which is exact rather than a proxy.
pub(super) fn should_force_registered_native_over_bytecode_precomputed_for_receiver(
    shared: &SharedVm,
    force_native: bool,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    receiver: Option<ObjectRef>,
) -> bool {
    force_native
        && (!native_shadow_suppressed_by_redefine(shared, class_name)
            || redefine_immune_forced_native_for_receiver(
                shared,
                class_name,
                method_name,
                method_descriptor,
                receiver,
            ))
}

/// [`should_force_registered_native_over_bytecode`], receiver-aware.
pub(crate) fn should_force_registered_native_over_bytecode_for_receiver(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    receiver: Option<ObjectRef>,
) -> bool {
    let registry = &shared.natives.native_methods;
    let is_intrinsic = registry
        .resolve_id(class_name, method_name, method_descriptor)
        .and_then(|id| registry.kind_of_id(id))
        == Some(cratonvm_native_api::NativeKind::Intrinsic)
        && !(crate::vm::dispatch_policy(shared).is_jdk_only()
            && cratonvm_native_api::unretire::pure_bytecode_for(class_name, method_name));
    should_force_registered_native_over_bytecode_precomputed_for_receiver(
        shared,
        is_intrinsic,
        class_name,
        method_name,
        method_descriptor,
        receiver,
    )
}

/// The receiver of an instance call, for the gates above. `None` for a static
/// call, a null receiver, or a primitive first argument.
pub(super) fn receiver_of(args: &[Value]) -> Option<ObjectRef> {
    match args.first() {
        Some(Value::Object(Some(obj))) => Some(*obj),
        _ => None,
    }
}

/// Route a force-native interception through §7 policy, and count it.
///
/// # The gap this closes
///
/// [`intercept_force_registered_native`] and its cached twin are reached from
/// **seven** call sites across `dispatch_static`, `dispatch_virtual` and
/// `invoke`, and both used to end in a bare `safe_native_call` on a callback
/// from `NativeMethodRegistry::find`. No `dispatch_policy`, no
/// `resolve_native_dispatch_wave1`, no `record_invocation`. Their entire
/// purpose is to make a registered native beat *concrete real-JDK bytecode*,
/// which is precisely the inversion §1.4 forbids under `JdkOnly` — so under
/// `--real-jdk` these were seven unguarded holes in contract §11's *"every
/// strict-mode native dispatch"*, and in `Compatible` they were seven
/// dispatches missing from the §4 census. Every sibling dispatch route
/// (`invoke_or_native`, `try_stackless_invoke` steps 1 and 6,
/// `invoke_on_class_shared_inner`) was routed in wave 1; these two were not.
///
/// # Why `bytecode_available: true`
///
/// Unlike `resolve_step1_native`, which runs before any method resolution and
/// passes `false` because it genuinely does not know, these sites are only
/// reached *because* `force_native_over_real_jdk_bytecode` said this triple's
/// real bytecode must lose. Concrete bytecode existing is the premise of the
/// call, so `true` is the honest input to §7 step 3 — and it is what makes a
/// strict run fall through to that bytecode instead of running the shadow.
///
/// # `Compatible` is bit-for-bit unchanged
///
/// `compat_native_wins` is `true` — exactly the unconditional "a registered
/// native wins here" the `find` call encoded — and in `Compatible` mode
/// `resolve_native_dispatch_wave1` is a pure function of that boolean. The
/// added cost is one relaxed `fetch_add` for the census.
///
/// # The three answers
///
/// * `Ok(Some(cb))` — dispatch it, and the invocation has been counted.
/// * `Ok(None)` — nothing registered, or §7 step 3 sent a `Bridge` to the real
///   bytecode. The interceptor declines, and the call proceeds to that
///   bytecode. The shadow attempt is already recorded by the resolver.
/// * `Err(violation)` — §1.3, a `SyntheticStub` under `--real-jdk`. Raised
///   rather than declined, matching `invoke_on_class_shared_inner`, so it is
///   counted as a `SyntheticNativeInvocation`. Swallowing it would run the real
///   bytecode quietly and leave a strict run reporting zero synthetic-stub
///   invocations for a call that was one — the exact false-green contract §11
///   must be immune to.
///
/// `pub(super)` for the one door outside this file with the same shape:
/// `lambda::try_lambda_dispatch`'s self-typed functional-interface default
/// (`Function.andThen` on a lambda proxy), whose real body is also concrete
/// `java.base` bytecode.
#[inline]
pub(super) fn admit_forced_native(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> Result<Option<cratonvm_native_api::NativeCallback>, cratonvm_types::error::JdkOnlyViolation> {
    let Some(id) =
        shared
            .natives
            .native_methods
            .resolve_id(class_name, method_name, method_descriptor)
    else {
        return Ok(None);
    };
    admit_forced_native_id(shared, id, class_name, method_name, method_descriptor)
}

/// [`admit_forced_native`] for a caller that already holds the resolved
/// [`cratonvm_native_api::NativeMethodId`].
///
/// The cached interceptor gets its id from the call site's generation-keyed
/// `NativeCallSite` memo, which exists because the plain
/// `NativeMethodRegistry::find` it replaced was measured as the #2 hottest
/// symbol (~7% of samples) on `TestResponsePerformance`. Routing that path
/// through the string-hashing sibling would hand that back; taking the id
/// keeps the warm cost at two array indexes, the policy call, and one relaxed
/// `fetch_add`. The lambda receiver-native door
/// (`lambda::lambda_receiver_native_id`) memoises its id for the same reason.
#[inline]
pub(super) fn admit_forced_native_id(
    shared: &SharedVm,
    id: cratonvm_native_api::NativeMethodId,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> Result<Option<cratonvm_native_api::NativeCallback>, cratonvm_types::error::JdkOnlyViolation> {
    let registry = &shared.natives.native_methods;
    let Some(callback) = registry.callback_of(id) else {
        return Ok(None);
    };
    let kind = registry
        .kind_of_id(id)
        .unwrap_or(cratonvm_native_api::NativeKind::Bridge);
    match crate::vm::resolve_native_dispatch_wave1_in(
        &shared.natives.native_shadow_sink,
        crate::vm::DispatchDoor::ForceIntercept,
        crate::vm::dispatch_policy(shared),
        class_name,
        method_name,
        method_descriptor,
        Some((callback, kind)),
        true,
        true,
    ) {
        // §1.3 — a `SyntheticStub` may not be invoked under `--real-jdk`.
        // Raised rather than silently declined, matching
        // `invoke_on_class_shared_inner`: the violation is counted as a
        // `SyntheticNativeInvocation` by the caller that turns it into
        // `VmError::JdkOnly`, and swallowing it here would drop that census
        // entry while quietly running the real bytecode — a strict run would
        // then report zero synthetic-stub invocations for a call that was one.
        Some(crate::vm::DispatchDecision::Reject(violation)) => Err(violation),
        Some(decision) => match decision.native_callback() {
            Some(admitted) => {
                // Counted at the point of actual dispatch, matching every
                // other route: the caller calls `safe_native_call` next.
                registry.record_invocation(id);
                Ok(Some(admitted))
            }
            None => Ok(None),
        },
        // §7 step 3 under `JdkOnly`: concrete bytecode beats this bridge. The
        // shadow attempt was already recorded by the resolver.
        None => Ok(None),
    }
}

// JDK-ONLY-WAVE2, RETIRED 2026-08-06. `THREADPOOL_EXECUTE_RECEIVER_SHAPE_SITES`
// (the census of the eight dispatch sites that carried the
// `ThreadPoolExecutor.execute` receiver-shape probe) and
// `threadpool_executor_has_real_workers` (the probe itself) lived here.
//
// All eight are gone, together with the ninth, receiver-blind arm in
// `force_native_over_real_jdk_bytecode` that they existed to override. What
// replaced them: `native_es_execute` is tagged `NativeKind::SyntheticStub`
// (`native-builtins/src/util_concurrent_ext.rs`) and
// `java/util/concurrent/ThreadPoolExecutor` is on
// `real_protected_stub_class_common`'s allow-list, so the one centralised
// arbitration yields the stub to the real `execute()` body class-scoped, for
// every receiver, on both the warm and the cold dispatch path.
//
// That is only sound because the per-INSTANCE distinction was eliminated
// first, by L10 (2026-08-06) and at REGISTRATION rather than in dispatch:
// `NativeMethodRegistry::register` drops every `Executors` pool factory when
// `drop_real_layout_synthetic` is set, so on a real-JDK image the real
// `java.util.concurrent.Executors` bytecode builds every executor and CratonVM
// has no code path that can mint a fabricated one. The sites are deletable
// because the fabricated receiver CANNOT EXIST -- a statement about the code,
// not the `false=0` reading two workloads produced, which L10 measured as
// already identical before it landed. `every_threadpool_receiver_shape_site_is_gone` below is the gate that
// keeps a copy from growing back.
//
// `native-builtins`' own `executor_has_real_workers` (and the deliberately
// separate twin in `native-collections`) is NOT part of this: it is defence in
// depth *inside the callee*, re-checking and redirecting a genuinely-real
// receiver that reached the native anyway. It stays, and is listed here only so
// a grep does not mistake it for a dispatch site.

// ---------------------------------------------------------------------------
// Per-call-site shape of the cached interception chain
// ---------------------------------------------------------------------------

/// This triple could reach the `ClassLoader` null-resource re-target.
pub(super) const INTERCEPT_SHAPE_CLASSLOADER_RESOURCE: u8 = 1 << 0;
/// This triple could reach the `java/lang/Class` reflection re-target.
pub(super) const INTERCEPT_SHAPE_CLASS_REFLECTION: u8 = 1 << 1;
/// This triple could reach the real-`HttpURLConnection` carrier exemption.
pub(super) const INTERCEPT_SHAPE_HTTP_CARRIER: u8 = 1 << 2;
/// One of the three name-matched intercepts in the cached virtual
/// dispatcher (`ClassLoader.setDefaultAssertionStatus`, the surefire
/// `LazyLauncher.discover` native, the reflective `Method.invoke` /
/// `Constructor.newInstance` override). Consumed only by the invoke fast
/// door, which declines any method with a non-zero shape.
pub(super) const INTERCEPT_SHAPE_NAMED: u8 = 1 << 3;
/// This triple could reach the `FileSystemProvider` base-class re-target
/// ([`file_system_provider_retarget_shape`]).
pub(super) const INTERCEPT_SHAPE_FS_PROVIDER: u8 = 1 << 4;

/// Classify a call site's triple against the three special-case arms of
/// [`intercept_force_registered_native_cached`], once.
///
/// Pure: it reads the triple and nothing else — no VM, no policy, no receiver,
/// no arguments — which is what makes it safe to memoize in
/// `CachedBytecodeMethod::intercept_shape_cache`. See that field's doc for the
/// measurement.
///
/// **Keep this in step with the arms it gates.** A new name-keyed arm that
/// forgets to set a bit here is not slow, it is SKIPPED — a correctness bug.
/// `intercept_shape_agrees_with_the_arms_it_gates` in this module's tests is
/// what fails when they drift.
pub(super) fn intercept_shape_of(class_name: &str, method_name: &str, descriptor: &str) -> u8 {
    let mut shape = 0u8;
    if classloader_resource_shape(method_name, descriptor) {
        shape |= INTERCEPT_SHAPE_CLASSLOADER_RESOURCE;
    }
    if class_reflection_shape(method_name, descriptor) {
        shape |= INTERCEPT_SHAPE_CLASS_REFLECTION;
    }
    if http_carrier_declaring_class(class_name) {
        shape |= INTERCEPT_SHAPE_HTTP_CARRIER;
    }
    if named_intercept_shape(class_name, method_name, descriptor) {
        shape |= INTERCEPT_SHAPE_NAMED;
    }
    if file_system_provider_retarget_shape(class_name, method_name, descriptor) {
        shape |= INTERCEPT_SHAPE_FS_PROVIDER;
    }
    shape
}

/// The triples both interceptors re-target to the `FileSystemProvider` BASE
/// registration: `newFileChannel` and the three link operations, on the base
/// class or either real provider. The fd-backed natives are registered only on
/// the base (`native-builtins/src/phases_late/nio_file.rs`), so a site whose
/// resolved class is `sun/nio/fs/WindowsFileSystemProvider` finds nothing
/// under its own triple.
///
/// One predicate for both twins. The cached twin used to have no such arm at
/// all: once a site warmed into the invoke cache with a real provider as its
/// resolved class, `native_call_site().resolve` probed the receiver's triple,
/// found nothing, and the call ran the provider's bytecode — so the same call
/// site ran the native while cold and the JDK body once warm.
fn file_system_provider_retarget_shape(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    (method_name == "newFileChannel"
        && descriptor
            == "(Ljava/nio/file/Path;Ljava/util/Set;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/channels/FileChannel;"
        && matches!(
            class_name,
            "java/nio/file/spi/FileSystemProvider"
                | "sun/nio/fs/WindowsFileSystemProvider"
                | "sun/nio/fs/UnixFileSystemProvider"
        ))
        || (matches!(
            class_name,
            "java/nio/file/spi/FileSystemProvider"
                | "sun/nio/fs/WindowsFileSystemProvider"
                | "sun/nio/fs/UnixFileSystemProvider"
        ) && matches!(method_name, "createSymbolicLink" | "createLink" | "readSymbolicLink"))
}

/// Dispatch the `FileSystemProvider` base registration for a triple
/// [`file_system_provider_retarget_shape`] accepted. `None` (the interceptor
/// declines, the call runs its bytecode) when nothing is registered — or, under
/// `--real-jdk`, when §7 hands the `Bridge` back to the real body. Admitted
/// through [`admit_forced_native`]: this arm used to be a bare `find`, i.e. one
/// more force door with no policy and no census. `Compatible` is unchanged.
fn intercept_file_system_provider_retarget(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    method_name: &str,
    method_descriptor: &str,
    args: &[Value],
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    let cb = match admit_forced_native(
        shared,
        "java/nio/file/spi/FileSystemProvider",
        method_name,
        method_descriptor,
    ) {
        Ok(Some(cb)) => cb,
        Ok(None) => return None,
        Err(violation) => {
            return Some(Err(MethodCallFailed::InternalError(VmError::JdkOnly(
                violation,
            ))));
        }
    };
    Some((|| {
        let result = crate::vm::safe_native_call(shared, thread, cb, args)?;
        if let Some(value) = result {
            push_invoke_return_value(
                &mut thread.frames[frame_idx].stack,
                coerce_native_return_value(value, crate::jit::return_type(method_descriptor)),
            )?;
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
        Ok(CachedCallResult::Handled)
    })())
}

/// The name triples `intercept_classloader_set_default_assertion_status`,
/// `surefire_lazy_launcher_discover_native` and
/// `native_override_for_cached_reflect_invoke` match on.
fn named_intercept_shape(class_name: &str, method_name: &str, descriptor: &str) -> bool {
    matches!(
        (class_name, method_name, descriptor),
        (_, "setDefaultAssertionStatus", "(Z)V")
            | (
                _,
                "discover",
                "(Lorg/junit/platform/launcher/LauncherDiscoveryRequest;)Lorg/junit/platform/launcher/TestPlan;"
            )
            | (
                "java/lang/reflect/Method",
                "invoke",
                "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;"
            )
            | (
                "java/lang/reflect/Constructor",
                "newInstance",
                "([Ljava/lang/Object;)Ljava/lang/Object;"
            )
    )
}

/// The name-keyed half of the `ClassLoader` null-resource re-target.
fn classloader_resource_shape(method_name: &str, descriptor: &str) -> bool {
    matches!(
        (method_name, descriptor),
        ("getResource", "(Ljava/lang/String;)Ljava/net/URL;")
            | (
                "getResources",
                "(Ljava/lang/String;)Ljava/util/Enumeration;"
            )
            | (
                "getResourceAsStream",
                "(Ljava/lang/String;)Ljava/io/InputStream;"
            )
    )
}

/// The receiver half of the `ClassLoader` null-resource re-target: the call's
/// receiver (`args[0]`) is an instance of `java.lang.ClassLoader`. The triple
/// alone also matches any user type with a `getResource(String)` of its own,
/// whose method must run instead (round 11, `R11RtGetResourceNull` threw NPE on
/// every such call in every tier). And the receiver's class INHERITS
/// `ClassLoader`'s method: a loader that overrides it
/// (`URLClassLoader.getResourceAsStream`, a user loader answering a null
/// name itself) runs its own body, as on HotSpot (interpreter round i2 wave
/// 54, lane L5; `crate::vm::receiver_inherits_class_loader_method`).
///
/// `false` for every receiver once the intercepts are retired
/// (`ClassRealm::classloader_null_name_intercepts`, over a real JDK with
/// `CRATONVM_RETIRE_CLASSLOADER_NULL_NAME_INTERCEPTS`; interpreter round i2
/// wave 55, lane L5): both interceptors then decline and the call runs the
/// body dispatch selected.
fn receiver_is_class_loader(
    shared: &SharedVm,
    args: &[Value],
    method_name: &str,
    descriptor: &str,
) -> bool {
    if !shared.classes.classloader_null_name_intercepts {
        crate::vm::note_classloader_null_name_retired(
            shared,
            Some(args),
            "native_override",
            method_name,
            descriptor,
        );
        return false;
    }
    crate::vm::receiver_inherits_class_loader_method(shared, args, method_name, descriptor)
}

/// The name-keyed half of the `java/lang/Class` reflection re-target.
fn class_reflection_shape(method_name: &str, descriptor: &str) -> bool {
    matches!(
        (method_name, descriptor),
        ("getProtectionDomain", "()Ljava/security/ProtectionDomain;")
            | ("isArray", "()Z")
            | ("getComponentType", "()Ljava/lang/Class;")
            | ("componentType", "()Ljava/lang/Class;")
    )
}

/// The whole `class_name` gate of [`real_http_url_connection_native`], factored
/// out so the memo and the arm cannot disagree about it.
pub(crate) fn http_carrier_declaring_class(class_name: &str) -> bool {
    matches!(
        class_name,
        "java/net/URLConnection"
            | "java/net/HttpURLConnection"
            | "javax/net/ssl/HttpsURLConnection"
            | "sun/net/www/protocol/http/HttpURLConnection"
            | "sun/net/www/protocol/https/HttpsURLConnectionImpl"
    )
}

/// CratonVM's own HTTP carrier classes — the concrete classes its
/// `URL.openConnection()` hands back, and the ones
/// `register_http_url_connection_real` registers natives on. Matched EXACTLY
/// (not by subtype): a user subclass such as
/// `SimpleClientHttpRequestFactoryTests$TestHttpURLConnection` has real
/// bytecode of its own and must keep running it.
const CRATONVM_HTTP_CARRIER_CLASSES: [&str; 4] = [
    "java/net/HttpURLConnection",
    "sun/net/www/protocol/http/HttpURLConnection",
    "sun/net/www/protocol/https/HttpsURLConnectionImpl",
    "javax/net/ssl/HttpsURLConnection",
];

/// Resolve the registered native for a call landing on a genuinely real,
/// `URL.openConnection()`-constructed CratonVM HTTP carrier — the one case
/// where the native must fire even though the class counts as "redefined"
/// somewhere in the process.
///
/// Why the exemption exists: Mockito's mock makers trip the class-wide
/// `class_redefine_generation` counter for EVERY instance of
/// `java/net/HttpURLConnection`, mock or not, for the rest of the process.
/// Without this, `should_force_registered_native_over_bytecode` cedes to the
/// real-JDK bytecode for a real, non-mock connection too — observed as
/// `getResponseCode()` returning 0 and `addRequestProperty`/`getHeaderField`
/// silently no-op'ing, because CratonVM's carrier keeps its request/response
/// state in native side tables the real JDK bytecode never touches. That is
/// `SimpleClientHttpRequestFactoryTests.interceptor()` losing the
/// interceptor's added header.
///
/// Keyed on the RECEIVER, not on `class_name`, and that is load-bearing twice:
///
///  * `class_name` is the resolved method's declaring class, so the very same
///    `connection.addRequestProperty(...)` call site in
///    `SimpleClientHttpRequest.addHeaders` reports `java/net/HttpURLConnection`
///    at first and `java/net/URLConnection` once an unrelated
///    `Mockito.mock(HttpURLConnection.class)` has re-resolved it. The old
///    `class_name == "java/net/HttpURLConnection"` test silently stopped
///    matching at that point — and it was missing from the cached/hot twin
///    entirely, so it also stopped applying as soon as a call site warmed up.
///  * a Mockito mock is Objenesis-constructed (no constructor ever runs), so
///    its inherited `URLConnection.url` field 0 stays unset, while a real
///    carrier's is always populated. The field-0 test therefore never fires
///    for a mock, and mocking `HttpURLConnection` still routes through
///    Mockito's advice for stubbing and verification.
///
/// Returns the callback to force, or `None` to let normal dispatch decide.
pub(crate) fn real_http_url_connection_native(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    args: &[Value],
) -> Option<cratonvm_native_api::registry::NativeCallback> {
    // Cheap gate first: only the connection hierarchy can reach the exemption.
    // Shared with `intercept_shape_of`, which memoizes this exact question per
    // call site, so the two cannot disagree about which classes qualify.
    if !http_carrier_declaring_class(class_name) {
        return None;
    }
    let Some(Value::Object(Some(receiver))) = args.first() else {
        return None;
    };
    // Objenesis-constructed mock => field 0 unset => not a real carrier.
    if !matches!(
        shared.mem.heap.get_field(*receiver, 0),
        Value::Object(Some(_))
    ) {
        return None;
    }
    let receiver_cid = shared.mem.heap.class_id_of(*receiver);
    let receiver_name = shared
        .classes
        .class_manager
        .read()
        .get_class(receiver_cid)
        .map(|class| class.name.to_string())?;
    if !CRATONVM_HTTP_CARRIER_CLASSES.contains(&receiver_name.as_str()) {
        return None;
    }
    shared
        .natives
        .native_methods
        .find(&receiver_name, method_name, method_descriptor)
        .or_else(|| {
            shared.natives.native_methods.find(
                "java/net/HttpURLConnection",
                method_name,
                method_descriptor,
            )
        })
}

pub(super) fn intercept_force_registered_native(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    args: &[Value],
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    // A class loaded in a user-defined class loader possesses its own bytecode
    // (e.g. an isolated library version such as H2 1.4 in TestUpgrade).
    // Application-level native overrides/intrinsics targeting host application
    // classes must never intercept and shadow bytecode defined by a user loader.
    if cratonvm_native_builtins::classloader::any_defining_loader_registered(&shared.loader_identity)
        && !crate::runtime::interpreter::constants::is_global_resolution_namespace(class_name)
    {
        let caller_cid = thread.frames.get(frame_idx).map(|f| f.class_id);
        let receiver_cid = receiver_of(args).map(|r| shared.mem.heap.class_id_of(r));
        let is_user_loader = |cid: ClassId| {
            cratonvm_native_builtins::classloader::defining_loader_for(&shared.loader_identity, cid.as_u32()).is_some()
                || shared.classes.class_manager.read().get_class(cid).map(|c| {
                    matches!(c.loader_id, cratonvm_types::ClassLoaderId::UserDefined(_))
                }).unwrap_or(false)
        };
        if receiver_cid.map_or(false, &is_user_loader) || caller_cid.map_or(false, &is_user_loader) {
            return None;
        }
    }

    // FileChannel.open() invokes FileSystemProvider.newFileChannel through a
    // default-provider receiver (WindowsFileSystemProvider on this host),
    // while the fd-backed native is registered on the JDK base class. Route
    // the forced call to that base registration explicitly so a cached
    // runtime receiver name cannot bypass it and run the JDK's deliberate
    // UnsupportedOperationException stub.
    //
    // `createSymbolicLink`/`createLink`/`readSymbolicLink` ride the same route
    // for the same reason — see `is_file_system_provider_link_native_override`.
    // The predicate is shared with the cached twin's shape memo.
    if file_system_provider_retarget_shape(class_name, method_name, method_descriptor) {
        return intercept_file_system_provider_retarget(
            shared,
            thread,
            frame_idx,
            method_name,
            method_descriptor,
            args,
        );
    }
    // The resource-name argument is specified to be non-null for every
    // ClassLoader resource accessor.  A virtual call whose constant-pool
    // owner is ClassLoader can resolve to an inherited cached method on a
    // custom loader, so the generic force-native lookup below sees the custom
    // class name and misses the callback registered on ClassLoader.  Route
    // only the null-argument contract through that base callback before
    // method-cache dispatch; normal non-null calls retain the custom loader's
    // virtual implementation.
    //
    // The name test is `classloader_resource_shape`, the one the cached twin's
    // per-site memo is computed from, rather than a second literal copy of the
    // same three triples that could drift from it.
    if args.len() == 2
        && matches!(args.get(1), Some(Value::Object(None)))
        && classloader_resource_shape(method_name, method_descriptor)
        && receiver_is_class_loader(shared, args, method_name, method_descriptor)
    {
        let cb = shared.natives.native_methods.find(
            "java/lang/ClassLoader",
            method_name,
            method_descriptor,
        )?;
        return Some((|| {
            let result = crate::vm::safe_native_call(shared, thread, cb, args)?;
            if let Some(value) = result {
                push_invoke_return_value(
                    &mut thread.frames[frame_idx].stack,
                    coerce_native_return_value(value, crate::jit::return_type(method_descriptor)),
                )?;
                crate::vm::native_return_pushed_to_stack(shared, thread);
            }
            Ok(CachedCallResult::Handled)
        })());
    }
    // `Class.getClassLoader()` is a concrete JDK method, but Class mirrors in
    // this VM use an internal layout and their real `classLoader` field can be
    // a stale non-loader object.  Dispatch by the receiver's *runtime* class
    // before the normal declaring-class gate: JDK calls reached through an
    // inherited/cached method reference can otherwise bypass the static
    // allowlist and hand ServiceLoader a String as its loader.
    if method_name == "getClassLoader"
        && method_descriptor == "()Ljava/lang/ClassLoader;"
        && matches!(
            args.first(),
            Some(Value::Object(Some(receiver))) if {
                let receiver_cid = shared.mem.heap.class_id_of(*receiver);
                shared
                    .classes.class_manager
                    .read()
                    .get_class(receiver_cid)
                    .map(|class| &*class.name == "java/lang/Class")
                    .unwrap_or(false)
            }
        )
    {
        // Fully-constant triple: memoized in a file-local cell rather than
        // re-hashing three literals on every call (native-dispatch-memoization
        // §3 Step 1, B1). The memo is keyed on the registry generation, so a
        // native registered later is still picked up and a negative result
        // self-heals — unlike a `OnceLock`.
        //
        // ONE CELL, ONE TRIPLE. The generation is the *only* key: the triple
        // is not re-verified on a warm hit (re-hashing it is the cost this
        // exists to remove), so a cell reached with a second triple can redeem
        // the first's memoized negative and silently report "no native" for a
        // registered one. This cell is reached from exactly one call, with
        // three string literals. The cell is this VM's
        // (`NativeRealm::interp_call_sites`, nio21 wave 9 L1), not a process
        // static.
        let callback = shared
            .natives
            .interp_call_sites
            .site(crate::vm::realms::native_realm::InterpNativeSite::OverrideClassGetClassLoader)
            .callback(
                &shared.natives.native_methods,
                "java/lang/Class",
                "getClassLoader",
                "()Ljava/lang/ClassLoader;",
            )?;
        return Some((|| {
            let result = crate::vm::safe_native_call(shared, thread, callback, args)?;
            if let Some(value) = result {
                push_invoke_return_value(&mut thread.frames[frame_idx].stack, value)?;
                crate::vm::native_return_pushed_to_stack(shared, thread);
            }
            Ok(CachedCallResult::Handled)
        })());
    }
    // These concrete Class methods read VM-private mirror fields in JDK 25.
    // Resolve by the receiver's runtime class so inherited or cached method
    // references cannot bypass CratonVM's class-id-backed native methods.
    // (`class_reflection_shape` is the cached twin's copy of this name test.)
    if class_reflection_shape(method_name, method_descriptor)
        && matches!(
            args.first(),
            Some(Value::Object(Some(receiver))) if {
                let receiver_cid = shared.mem.heap.class_id_of(*receiver);
                shared
                    .classes.class_manager
                    .read()
                    .get_class(receiver_cid)
                    .map(|class| &*class.name == "java/lang/Class")
                    .unwrap_or(false)
            }
        )
    {
        let callback = shared.natives.native_methods.find(
            "java/lang/Class",
            method_name,
            method_descriptor,
        )?;
        return Some((|| {
            let result = crate::vm::safe_native_call(shared, thread, callback, args)?;
            if let Some(value) = result {
                push_invoke_return_value(&mut thread.frames[frame_idx].stack, value)?;
                crate::vm::native_return_pushed_to_stack(shared, thread);
            }
            Ok(CachedCallResult::Handled)
        })());
    }
    // `java/net/HttpURLConnection`'s real-carrier natives (connect,
    // getResponseCode, getHeaderField, addRequestProperty, ...) must keep
    // firing for a genuinely real, `URL.openConnection()`-constructed carrier
    // (its real inherited `URLConnection.url` field 0 populated) even after
    // ANY instance of this class has been JVMTI-redefined elsewhere in the
    // process — e.g. a completely unrelated `Mockito.mock(HttpURLConnection
    // .class)` call. Mockito's default "inline" mock maker redefines the
    // TARGET CLASS's bytecode IN PLACE rather than subclassing it, so the
    // class-wide `class_redefine_generation` counter trips permanently for
    // EVERY instance of the class, mock or not, for the rest of the process.
    // Without this, `should_force_registered_native_over_bytecode`'s redefine
    // check below cedes to the now-Mockito-woven bytecode for a real,
    // non-mock connection too — observed as `getResponseCode()` silently
    // returning 0 and `getHeaderField`/`addRequestProperty` silently no-op'ing
    // instead of touching the real request/response, so
    // `SimpleClientHttpRequestFactoryTests.interceptor()` failed first with
    // "Status code '0' should be a three-digit positive integer" and then
    // (once getResponseCode alone was exempted) with the interceptor's added
    // header missing from the echoed response, simply because an EARLIER,
    // unrelated test method in the same JVM mocked HttpURLConnection.
    //
    // A Mockito mock itself is Objenesis-constructed (no constructor ever
    // runs), so its field 0 stays null — checking for a non-null field 0
    // cheaply distinguishes "genuinely real carrier" from "mock or synthetic
    // carrier" without invoking `toExternalForm`, and this exemption never
    // fires for an actual mock (whose field 0 is always null), so mocking
    // HttpURLConnection still correctly routes through Mockito's advice for
    // stubbing/verification. Deliberately not narrowed to a specific method
    // allowlist: any native registered on this class for a real carrier is
    // safe to force, since the receiver check alone already gates out mocks.
    if let Some(callback) =
        real_http_url_connection_native(shared, class_name, method_name, method_descriptor, args)
    {
        return Some((|| {
            let result = crate::vm::safe_native_call(shared, thread, callback, args)?;
            if let Some(value) = result {
                push_invoke_return_value(
                    &mut thread.frames[frame_idx].stack,
                    coerce_native_return_value(value, crate::jit::return_type(method_descriptor)),
                )?;
                crate::vm::native_return_pushed_to_stack(shared, thread);
            }
            Ok(CachedCallResult::Handled)
        })());
    }
    if method_name == "getTarget" && crate::runtime::env_cache::dbg_ccsprobe() {
        eprintln!(
            "[ccs-probe] intercept_force_registered_native: class={} method={}{} \
             force={}",
            class_name,
            method_name,
            method_descriptor,
            shared.natives.native_methods.kind_of(class_name, method_name, method_descriptor) == Some(cratonvm_native_api::NativeKind::Intrinsic),
        );
    }
    // A JVMTI agent that redefined this class (e.g. a Mockito inline mock)
    // makes its woven bytecode authoritative — cede to it instead of forcing
    // the native, so the instrumentation advice runs. Reflection-metadata
    // natives are exempt (see `redefine_immune_reflection_native`): the real
    // bytecode cannot reproduce them under CratonVM.
    if !should_force_registered_native_over_bytecode_for_receiver(
        shared,
        class_name,
        method_name,
        method_descriptor,
        receiver_of(args),
    ) {
        return None;
    }
    // (Receiver-shape probe deleted 2026-08-06 — see
    // `force_native_over_real_jdk_bytecode`. The ninth, receiver-blind arm this
    // one existed to undo went with it, so there is nothing left to undo.)
    // §7 routing. This was a bare `find`, so the dispatch below ran with no
    // policy check and no census count — see `admit_forced_native` for why
    // that made this one of seven unguarded strict-mode holes. `None` here
    // means the policy refused, and declining to intercept hands the call to
    // the real bytecode this site exists to override, which is §7 step 3's
    // answer.
    let cb = match admit_forced_native(shared, class_name, method_name, method_descriptor) {
        Ok(Some(cb)) => cb,
        Ok(None) => return None,
        Err(violation) => {
            return Some(Err(MethodCallFailed::InternalError(VmError::JdkOnly(
                violation,
            ))));
        }
    };
    if method_name == "getTarget" && crate::runtime::env_cache::dbg_ccsprobe() {
        eprintln!("[ccs-probe] intercept_force_registered_native: dispatching native callback");
    }
    let ret_type = crate::jit::return_type(method_descriptor);
    Some((|| {
        // Under `CRATONVM_NATIVE_SYNC` a forced native answering a
        // synchronized method holds its monitor (one bool load when off).
        let result = super::invoke::safe_forced_native_call(
            shared,
            thread,
            cb,
            args,
            class_name,
            method_name,
            method_descriptor,
        )?;
        if let Some(value) = result.filter(|_| ret_type != b'V') {
            push_invoke_return_value(
                &mut thread.frames[frame_idx].stack,
                coerce_native_return_value(value, ret_type),
            )?;
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
        Ok(CachedCallResult::Handled)
    })())
}

/// Perf variant of [`intercept_force_registered_native`] for the cached/hot
/// dispatch paths (`execute_invokevirtual_cached`, `execute_invokestatic_cached`)
/// that already hold an `Arc<CachedBytecodeMethod>` for this callsite. The
/// original re-evaluated `force_native_over_real_jdk_bytecode`'s ~55-branch
/// sequential string-comparison gauntlet from scratch on *every single*
/// cached-invoke hit -- this was independently identified as a real
/// interpreter-throughput bottleneck (~51% of all executed instructions on
/// method-call-heavy workloads, see `docs/feature-designs/gpu-offload.md`) and reproduced live via `perf`/`gdb`
/// during the `ClientHttpConnectorTests` investigation (2026-07-15): one
/// interpreter thread pegged at ~100% CPU for 25+ seconds cycling through
/// this exact call chain while executing a tight Java-level spin/poll loop
/// typical of Reactor/Netty/Jetty's lock-free scheduling. This variant reads
/// `cached.force_native_cache`, computing the pure part exactly once per
/// invoke-cache entry (memoized `OnceLock`, shared via the entry's `Arc`)
/// instead of on every hit; the mutable-state-dependent redefine check is
/// still re-evaluated every call (cheap, and must stay live).
pub(super) fn intercept_force_registered_native_cached(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cached: &CachedBytecodeMethod,
    args: &[Value],
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    let class_name = cached.class_name.as_ref();
    let method_name = cached.method_name.as_ref();
    let method_descriptor = cached.method_descriptor.as_ref();

    // A class loaded in a user-defined class loader possesses its own bytecode
    // (e.g. an isolated library version such as H2 1.4 in TestUpgrade).
    // Application-level native overrides/intrinsics targeting host application
    // classes must never intercept and shadow bytecode defined by a user loader.
    if cratonvm_native_builtins::classloader::any_defining_loader_registered(&shared.loader_identity)
        && !crate::runtime::interpreter::constants::is_global_resolution_namespace(class_name)
    {
        let caller_cid = thread.frames.get(frame_idx).map(|f| f.class_id);
        let receiver_cid = receiver_of(args).map(|r| shared.mem.heap.class_id_of(r));
        let declaring_cid = Some(cached.declaring_class_id);
        let is_user_loader = |cid: ClassId| {
            cratonvm_native_builtins::classloader::defining_loader_for(&shared.loader_identity, cid.as_u32()).is_some()
                || shared.classes.class_manager.read().get_class(cid).map(|c| {
                    matches!(c.loader_id, cratonvm_types::ClassLoaderId::UserDefined(_))
                }).unwrap_or(false)
        };
        if declaring_cid.map_or(false, &is_user_loader)
            || receiver_cid.map_or(false, &is_user_loader)
            || caller_cid.map_or(false, &is_user_loader)
        {
            return None;
        }
    }
    // Which of the three special-case arms below this triple can reach at all,
    // computed once per call site. Every one of their name/class keys is a
    // function of the triple, and the triple is fixed for this entry — so they
    // were per-call-site constants being re-derived on every cached-invoke hit.
    // `real_http_url_connection_native`'s five-literal `class_name` gate alone
    // measured 1.50% of the interpreted-invoke arm on a probe whose only call
    // is `int callee(int)`. The argument/receiver halves are NOT memoized and
    // are still evaluated in full below. See
    // `CachedBytecodeMethod::intercept_shape_cache`.
    //
    // Deliberately NOT an early `return` on `shape == 0` into a shared tail
    // function: that WAS built and measured (2026-08-11), and the function
    // split cost more than the three bit tests it skipped — entry + tail
    // 1.05% + 1.12% against a single 1.44% function before, with the string
    // work already removed from both. The bits guard the arms where they
    // stand; falling through them IS the fast path.
    let shape = *cached
        .intercept_shape_cache
        .get_or_init(|| intercept_shape_of(class_name, method_name, method_descriptor));
    // The uncached twin's first arm, which this path used to lack: see
    // `file_system_provider_retarget_shape`.
    if shape & INTERCEPT_SHAPE_FS_PROVIDER != 0 {
        return intercept_file_system_provider_retarget(
            shared,
            thread,
            frame_idx,
            method_name,
            method_descriptor,
            args,
        );
    }
    // Keep the cached path aligned with the uncached null-resource contract
    // above.  The cache is keyed by the resolved custom-loader method, while
    // the implementation callback is deliberately registered on ClassLoader.
    if shape & INTERCEPT_SHAPE_CLASSLOADER_RESOURCE != 0
        && args.len() == 2
        && matches!(args.get(1), Some(Value::Object(None)))
        && receiver_is_class_loader(shared, args, method_name, method_descriptor)
    {
        let cb = shared.natives.native_methods.find(
            "java/lang/ClassLoader",
            method_name,
            method_descriptor,
        )?;
        let ret_type = crate::jit::return_type(method_descriptor);
        return Some((|| {
            let result = crate::vm::safe_native_call(shared, thread, cb, args)?;
            if let Some(value) = result.filter(|_| ret_type != b'V') {
                push_invoke_return_value(
                    &mut thread.frames[frame_idx].stack,
                    coerce_native_return_value(value, ret_type),
                )?;
                crate::vm::native_return_pushed_to_stack(shared, thread);
            }
            Ok(CachedCallResult::Handled)
        })());
    }
    if shape & INTERCEPT_SHAPE_CLASS_REFLECTION != 0
        && matches!(
            args.first(),
            Some(Value::Object(Some(receiver))) if {
                let receiver_cid = shared.mem.heap.class_id_of(*receiver);
                shared
                    .classes.class_manager
                    .read()
                    .get_class(receiver_cid)
                    .map(|class| &*class.name == "java/lang/Class")
                    .unwrap_or(false)
            }
        )
    {
        let callback = shared.natives.native_methods.find(
            "java/lang/Class",
            method_name,
            method_descriptor,
        )?;
        return Some((|| {
            let result = crate::vm::safe_native_call(shared, thread, callback, args)?;
            if let Some(value) = result {
                push_invoke_return_value(&mut thread.frames[frame_idx].stack, value)?;
                crate::vm::native_return_pushed_to_stack(shared, thread);
            }
            Ok(CachedCallResult::Handled)
        })());
    }
    // Same real-carrier exemption the uncached twin applies (see
    // `real_http_url_connection_native`). This path used to omit it entirely,
    // so the exemption held only until a call site warmed into the invoke
    // cache and then silently stopped applying — one
    // `Mockito.mock(HttpURLConnection.class)` anywhere in the process then
    // permanently broke every genuinely real connection's
    // `addRequestProperty`/`getResponseCode`/`getHeaderField`.
    if shape & INTERCEPT_SHAPE_HTTP_CARRIER != 0 {
        if let Some(callback) = real_http_url_connection_native(
            shared,
            class_name,
            method_name,
            method_descriptor,
            args,
        ) {
            return Some((|| {
                let result = crate::vm::safe_native_call(shared, thread, callback, args)?;
                if let Some(value) = result {
                    push_invoke_return_value(
                        &mut thread.frames[frame_idx].stack,
                        coerce_native_return_value(value, crate::jit::return_type(method_descriptor)),
                    )?;
                    crate::vm::native_return_pushed_to_stack(shared, thread);
                }
                Ok(CachedCallResult::Handled)
            })());
        }
    }
    // `CRATONVM_USER_LOADER_RESOURCE_BYTECODE` (interpreter round i6 wave 120,
    // lane L7): `force_native_over_real_jdk_bytecode` below answers `true` for
    // `java/lang/ClassLoader`'s resource methods, so a cached hit of the body
    // `populate_virtual_invoke_cache` filed for a user-loader receiver class
    // was re-forced to the `Bridge` native here, which only re-entered that
    // same body: the switch moved nothing. A receiver the native would route
    // to the body runs the body now (the native's own predicate, and its
    // per-call `--compatible` count). A null name (the arm above) and every
    // other receiver keep the native. Off: one per-VM bool load.
    // WARNING(i6-user-flip): CRATONVM_USER_LOADER_RESOURCE_BYTECODE default ON by user decision 2026-10-19; =0 restores the old path
    if class_name == "java/lang/ClassLoader"
        && args.len() == 2
        && super::dispatch_virtual::class_loader_resource_triple(method_name, method_descriptor)
    {
        if let Some(Value::Object(Some(receiver))) = args.first() {
            let receiver = *receiver;
            let mut ctx = crate::vm::NativeContextImpl {
                shared,
                thread: &mut *thread,
            };
            if cratonvm_native_builtins::classloader::user_loader_resource_body_for_receiver(
                &mut ctx, receiver,
            ) {
                return None;
            }
        }
    }
    let force_native = *cached.force_native_cache.get_or_init(|| {
        let registry = &shared.natives.native_methods;
        cached
            .native_call_site()
            .resolve(registry, class_name, method_name, method_descriptor)
            .and_then(|id| registry.kind_of_id(id))
            == Some(cratonvm_native_api::NativeKind::Intrinsic)
            && !(crate::vm::dispatch_policy(shared).is_jdk_only()
                && cratonvm_native_api::unretire::pure_bytecode_for(class_name, method_name))
    });
    if method_name == "getTarget" && crate::runtime::env_cache::dbg_ccsprobe() {
        eprintln!(
            "[ccs-probe] intercept_force_registered_native_cached: class={} method={}{} \
             force={}",
            class_name, method_name, method_descriptor, force_native,
        );
    }
    // A JVMTI agent that redefined this class (e.g. a Mockito inline mock)
    // makes its woven bytecode authoritative — cede to it instead of forcing
    // the native, so the instrumentation advice runs. Reflection-metadata
    // natives are exempt (see `redefine_immune_reflection_native`): the real
    // bytecode cannot reproduce them under CratonVM.
    if !should_force_registered_native_over_bytecode_precomputed_for_receiver(
        shared,
        force_native,
        class_name,
        method_name,
        method_descriptor,
        receiver_of(args),
    ) {
        return None;
    }
    // (Receiver-shape probe deleted 2026-08-06 — see
    // `force_native_over_real_jdk_bytecode`. The ninth, receiver-blind arm this
    // one existed to undo went with it, so there is nothing left to undo.)
    // Site A1 of `native-dispatch-memoization.md`
    // §3 Step 2. Perf (2026-07-19, TestResponsePerformance residual): memoize
    // the resolved callback per invoke-cache entry, same shape as
    // `force_native_cache` above -- `NativeMethodRegistry::find` was the #2
    // hottest symbol (~7% of samples) on that benchmark.
    //
    // This was a `OnceLock<Option<NativeCallback>>` and is now a
    // generation-keyed `NativeCallSite`, which also FIXES A LATENT BUG: the
    // `OnceLock` memoized a *negative* permanently, on the argument that
    // native registration is immutable after boot. That holds for the steady
    // state but not for boot itself, nor for `alias_class` / the lazy
    // `register_*` passes that run after the first bytecode executes -- a
    // native registered by a later pass was invisible here forever, while
    // dispatching fine through `find`. The generation check re-resolves
    // exactly when a new slot is appended.
    //
    // ONE CELL, ONE TRIPLE: `class_name`/`method_name`/`method_descriptor` are
    // `cached.{class,method}_name` / `cached.method_descriptor` verbatim (bound
    // at the top of this function), so this cell only ever sees this entry's
    // own triple. The `java/lang/ClassLoader` re-target earlier in this
    // function deliberately stays on plain `find` for that reason.
    //
    // §7 routing (2026-08-04). This ended in a bare `safe_native_call` on the
    // memoized callback — no `dispatch_policy`, no
    // `resolve_native_dispatch_wave1`, no census count — which made it one of
    // seven unguarded strict-mode holes; see `admit_forced_native`. The memo
    // is kept: `resolve` hands back the `NativeMethodId` without re-hashing,
    // and `admit_forced_native_id` takes it from there, so the perf argument
    // above survives intact.
    let id = cached.native_call_site().resolve(
        &shared.natives.native_methods,
        class_name,
        method_name,
        method_descriptor,
    )?;
    let cb = match admit_forced_native_id(shared, id, class_name, method_name, method_descriptor) {
        Ok(Some(cb)) => cb,
        Ok(None) => return None,
        Err(violation) => {
            return Some(Err(MethodCallFailed::InternalError(VmError::JdkOnly(
                violation,
            ))));
        }
    };
    if method_name == "getTarget" && crate::runtime::env_cache::dbg_ccsprobe() {
        eprintln!(
            "[ccs-probe] intercept_force_registered_native_cached: dispatching native callback"
        );
    }
    let ret_type = crate::jit::return_type(method_descriptor);
    // JVMS §2.11.10, as the uncached intercept (`safe_forced_native_call`):
    // under `native_sync_enabled` the forced native of a synchronized method
    // runs holding its monitor. This entry is a bytecode one, which the fills
    // still cache for a synchronized method while armed, and the hit path
    // takes the monitor only after this intercept has declined.
    let sync = (cached.is_synchronized && super::invoke::native_sync_enabled(shared))
        .then_some((cached.is_static, cached.declaring_class_id));
    Some((|| {
        let result = super::invoke::safe_native_call_synchronized(shared, thread, cb, args, sync)?;
        if let Some(value) = result.filter(|_| ret_type != b'V') {
            push_invoke_return_value(
                &mut thread.frames[frame_idx].stack,
                coerce_native_return_value(value, ret_type),
            )?;
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
        Ok(CachedCallResult::Handled)
    })())
}

#[inline]
pub(super) fn intercept_jython_pyjavatype_findattr_ex(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    method_name: &str,
    method_descriptor: &str,
    receiver_class_id: Option<ClassId>,
    is_special: bool,
    args: &[Value],
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    if is_special {
        return None;
    }
    if method_name != "__findattr_ex__"
        || method_descriptor != "(Ljava/lang/String;)Lorg/python/core/PyObject;"
    {
        return None;
    }
    let recv_cid = receiver_class_id?;
    let recv_name = {
        let cm = shared.classes.class_manager.read();
        cm.get_class(recv_cid).map(|c| c.name.to_string())?
    };
    if !shared.natives.native_methods.has_app_workaround(
        cratonvm_native_api::AppWorkaroundKind::ReceiverSubtypeNativeRetarget,
        &recv_name,
        method_name,
        method_descriptor,
    ) {
        return None;
    }
    let cb = shared
        .natives
        .native_methods
        .find(&recv_name, method_name, method_descriptor)?;
    let ret_type = crate::jit::return_type(method_descriptor);
    Some((|| {
        let result = crate::vm::safe_native_call(shared, thread, cb, args)?;
        if let Some(value) = result.filter(|_| ret_type != b'V') {
            push_invoke_return_value(
                &mut thread.frames[frame_idx].stack,
                coerce_native_return_value(value, ret_type),
            )?;
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
        Ok(CachedCallResult::Handled)
    })())
}

#[inline]
pub(super) fn intercept_jython_pymodule_findattr(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    method_name: &str,
    method_descriptor: &str,
    receiver_class_id: Option<ClassId>,
    args: &[Value],
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    if method_name != "__findattr__"
        || method_descriptor != "(Ljava/lang/String;)Lorg/python/core/PyObject;"
    {
        return None;
    }
    let recv_cid = receiver_class_id?;
    let recv_name = {
        let cm = shared.classes.class_manager.read();
        cm.get_class(recv_cid).map(|c| c.name.to_string())?
    };
    if !shared.natives.native_methods.has_app_workaround(
        cratonvm_native_api::AppWorkaroundKind::ReceiverSubtypeNativeRetarget,
        &recv_name,
        method_name,
        method_descriptor,
    ) {
        return None;
    }
    let cb = shared
        .natives
        .native_methods
        .find(&recv_name, method_name, method_descriptor)?;
    let ret_type = crate::jit::return_type(method_descriptor);
    Some((|| {
        let result = crate::vm::safe_native_call(shared, thread, cb, args)?;
        if let Some(value) = result.filter(|_| ret_type != b'V') {
            push_invoke_return_value(
                &mut thread.frames[frame_idx].stack,
                coerce_native_return_value(value, ret_type),
            )?;
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
        Ok(CachedCallResult::Handled)
    })())
}

/// `URLClassLoader.findClass(String)` invoked on a SUBCLASS receiver whose CP
/// methodref names that subclass (so the static-class force-native gate, keyed
/// on the methodref class, never matches). The canonical case is Jasper's
/// `JasperLoader`, which overrides BOTH `loadClass` overloads and calls
/// `findClass(name)` directly from inside `loadClass` to load the
/// runtime-compiled `org.apache.jsp.*_jsp` servlet from its scratch-dir URL.
/// Because the override runs, CratonVM's `cl_load_class` native (which would
/// resolve the class from the global classpath) is bypassed, and dispatch lands
/// on the real `URLClassLoader.findClass` bytecode — whose shimmed
/// `ucp.getResource` returns null → `ClassNotFoundException`, 500-ing every
/// compiled JSP/tag (TestPageContext, TestScopedAttributeELResolver, …).
///
/// Resolve `findClass` from the actual receiver class; only force the native
/// when it lands on `java/net/URLClassLoader` itself — a subclass that declares
/// its OWN `findClass` keeps its bytecode. `ucl_find_class` delegates to the
/// base classpath, where the loader's `<init>` already registered its URLs,
/// matching HotSpot. This handles the FIRST (uncached) dispatch; the cache
/// populator (`populate_virtual_invoke_cache`) independently force-caches the
/// native via the `declaring_name`-keyed `force_native_over_real_jdk_bytecode`
/// entry, so repeat dispatches stay native too.
#[inline]
pub(super) fn intercept_urlclassloader_subclass_native_method(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    method_name: &str,
    method_descriptor: &str,
    receiver_class_id: Option<ClassId>,
    is_special: bool,
    args: &[Value],
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    if is_special
        || !matches!(
            (method_name, method_descriptor),
            ("findClass", "(Ljava/lang/String;)Ljava/lang/Class;")
                | ("addURL", "(Ljava/net/URL;)V")
        )
    {
        return None;
    }
    let recv_cid = receiver_class_id?;
    let declaring_name = {
        let cm = shared.classes.class_manager.read();
        let store = &cm.class_store;
        let (_m, declaring_id) = crate::classloading::find_method_recursive(
            recv_cid,
            method_name,
            method_descriptor,
            store,
        )?;
        store.get(declaring_id).map(|c| c.name.to_string())?
    };
    if declaring_name != "java/net/URLClassLoader" {
        return None;
    }
    // A JVMTI agent that redefined URLClassLoader makes its woven bytecode
    // authoritative — cede to it.
    if native_shadow_suppressed_by_redefine(shared, "java/net/URLClassLoader") {
        return None;
    }
    let cb = shared.natives.native_methods.find(
        "java/net/URLClassLoader",
        method_name,
        method_descriptor,
    )?;
    let ret_type = crate::jit::return_type(method_descriptor);
    Some((|| {
        let result = crate::vm::safe_native_call(shared, thread, cb, args)?;
        if let Some(value) = result.filter(|_| ret_type != b'V') {
            push_invoke_return_value(
                &mut thread.frames[frame_idx].stack,
                coerce_native_return_value(value, ret_type),
            )?;
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
        Ok(CachedCallResult::Handled)
    })())
}

/// A class may invoke an inherited ClassLoader resource method through a
/// constant-pool reference to its concrete subclass.  The regular force-native
/// gate is keyed by that symbolic class, so it misses the native registered on
/// ClassLoader and the real JDK body silently accepts null names. Resolve the
/// actual declaration and dispatch the shared ClassLoader native instead.
#[inline]
pub(super) fn intercept_classloader_subclass_resource_native(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    method_name: &str,
    method_descriptor: &str,
    receiver_class_id: Option<ClassId>,
    is_special: bool,
    args: &[Value],
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    if is_special
        || !matches!(
            (method_name, method_descriptor),
            ("getResource", "(Ljava/lang/String;)Ljava/net/URL;")
                | (
                    "getResources",
                    "(Ljava/lang/String;)Ljava/util/Enumeration;"
                )
                | (
                    "getResourceAsStream",
                    "(Ljava/lang/String;)Ljava/io/InputStream;"
                )
        )
    {
        return None;
    }
    let recv_cid = receiver_class_id?;
    let declaring_name = {
        let cm = shared.classes.class_manager.read();
        let store = &cm.class_store;
        let (_m, declaring_id) = crate::classloading::find_method_recursive(
            recv_cid,
            method_name,
            method_descriptor,
            store,
        )?;
        store.get(declaring_id).map(|c| c.name.to_string())?
    };
    if declaring_name != "java/lang/ClassLoader"
        || native_shadow_suppressed_by_redefine(shared, "java/lang/ClassLoader")
    {
        return None;
    }
    let cb = shared.natives.native_methods.find(
        "java/lang/ClassLoader",
        method_name,
        method_descriptor,
    )?;
    let ret_type = crate::jit::return_type(method_descriptor);
    Some((|| {
        let result = crate::vm::safe_native_call(shared, thread, cb, args)?;
        if let Some(value) = result.filter(|_| ret_type != b'V') {
            push_invoke_return_value(
                &mut thread.frames[frame_idx].stack,
                coerce_native_return_value(value, ret_type),
            )?;
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
        Ok(CachedCallResult::Handled)
    })())
}

/// Surefire's fork calls `ClassLoader.setDefaultAssertionStatus` before the JDK
/// static `assertionLock` is assigned; the real bytecode does
/// `synchronized (assertionLock)` and NPEs. Monomorphic inline caches and the
/// vtable fast path can push that bytecode without visiting `execute_invoke`, so
/// any site about to run this body must consult the Rust no-op first.
#[inline]
pub(super) fn intercept_classloader_set_default_assertion_status(
    shared: &SharedVm,
    thread: &mut JvmThread,
    method_name: &str,
    method_descriptor: &str,
    args: &[Value],
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    if method_name != "setDefaultAssertionStatus" || method_descriptor != "(Z)V" {
        return None;
    }
    // A ClassLoader receiver only: a user type's own
    // `setDefaultAssertionStatus(boolean)` must run (wave 13, lane L5).
    if !crate::vm::receiver_is_class_loader(shared, args) {
        return None;
    }
    // Fully-constant triple — memoized (native-dispatch-memoization §3, B2).
    // ONE CELL, ONE TRIPLE: reached from exactly this one call, with three
    // literals. See the `OverrideClassGetClassLoader` cell for why sharing a
    // cell across triples silently mis-answers. Per VM (nio21 wave 9 L1).
    let cb = shared
        .natives
        .interp_call_sites
        .site(
            crate::vm::realms::native_realm::InterpNativeSite::OverrideClSetDefaultAssertionStatus,
        )
        .callback(
            &shared.natives.native_methods,
            "java/lang/ClassLoader",
            "setDefaultAssertionStatus",
            "(Z)V",
        )?;
    Some(crate::vm::safe_native_call(shared, thread, cb, args).map(|_| CachedCallResult::Handled))
}

/// Surefire `LazyLauncher` implements `Launcher`. Some dispatch paths key the
/// lookup by the constant-pool interface (`org/junit/platform/launcher/Launcher`)
/// while the Rust override is registered on the concrete class. When the heap
/// receiver is actually `LazyLauncher`, return that native so we never execute
/// the JDK `discover` body (null delegate → `Cannot invoke discover on null`).
#[inline]
pub(super) fn surefire_lazy_launcher_discover_native(
    shared: &SharedVm,
    method_name: &str,
    descriptor: &str,
    recv_obj: ObjectRef,
) -> Option<cratonvm_native_api::NativeCallback> {
    if !matches!(
        method_name,
        "discover" | "__getattr__" | "__findattr__" | "__findattr_ex__"
    ) {
        return None;
    }
    let cid = shared.mem.heap.class_id_of(recv_obj);
    let cm = shared.classes.class_manager.read_recursive();
    let recv_name = cm.get_class(cid).map(|c| c.name.clone());
    drop(cm);
    let recv_name = recv_name?;
    if !shared.natives.native_methods.has_app_workaround(
        cratonvm_native_api::AppWorkaroundKind::ReceiverSubtypeNativeRetarget,
        &recv_name,
        method_name,
        descriptor,
    ) {
        return None;
    }
    shared
        .natives
        .native_methods
        .find(&recv_name, method_name, descriptor)
}

/// Will a native registered for this triple ACTUALLY run?
///
/// `native_methods.find(..).is_some()` is used all over the VM as a stand-in for
/// "this call is native-shadowed", and for most triples the two are the same
/// thing. They are NEVER the same thing for a `SyntheticStub` on a
/// [`real_protected_stub_class`] whose real body is loaded: that native loses
/// the arbitration at every dispatch site
/// ([`synthetic_stub_should_yield_to_real_bytecode`]), so the bytecode is what
/// executes and the "shadow" does not exist.
///
/// # What the difference costs
///
/// Each consumer of the raw probe pays a different penalty for the wrong
/// answer, and they compound:
///
/// * the JIT's four compile gates refuse to compile the method at all, so it
///   never reaches a tier and the megamorphic inline cache has nothing to
///   publish (`hit_entry=0`, `pub_published=0` for 506 000 consecutive calls);
/// * `jit_invoke_targets_native_shadow` then seals every CALLER of it out of
///   the JIT for the same reason;
/// * with no compiled callee to enter, every call from compiled code takes the
///   `invoke_or_native` -> `invoke_on_class_shared_inner` tail, which resolves
///   the callee from a class NAME on every single call.
///
/// MEASURED 2026-08-22 on `perf/webclient-reactive-20260821`, 200k-iteration
/// loops, HotSpot 25 control in brackets:
///
/// | call | CratonVM | HotSpot |
/// |---|---:|---:|
/// | `Instant.getNano()` - native registered, always yielded | 1627 ns | 2.3 ns |
/// | `Instant.compareTo()` - same class, no native | 39 ns | 2.9 ns |
/// | `ReentrantLock.lock()` + `unlock()` | 1760 ns | 11.9 ns |
/// | `AtomicBoolean.compareAndSet()` | 4325 ns | 5.5 ns |
/// | `LinkedBlockingDeque.peek()` | 1820 ns | 11.3 ns |
/// | `StringJoiner.length()` | 2924 ns | 5.2 ns |
/// | `Duration.getSeconds()` - control, no native | 24 ns | 2.3 ns |
///
/// Two methods of the SAME class, 40x apart, is the whole defect: `getNano` has
/// a registered native and `compareTo` does not. `java.time.Instant.now()`
/// alone runs 1 240 308 times in one `WebClientIntegrationTests` run - 475x the
/// next hottest method in that run.
///
/// # Why relaxing the gates is sound
///
/// The gates exist because "a compiled direct call bypasses the interpreter's
/// native-vs-bytecode decision". That is exactly the premise this predicate
/// checks: when the arbitration says the bytecode wins, compiling the bytecode
/// IS the interpreter's decision, not a bypass of it.
///
/// The relaxation is also one-way-safe. Every term
/// [`synthetic_stub_yields_with_cm`] reads is monotone in the direction that
/// matters - a class becomes loaded, a `Code` attribute becomes decoded - so a
/// `yield = true` verdict cannot revert while the process runs. The one thing
/// that could revert it, a JVMTI redefine, already quiesces and invalidates
/// compiled code through `any_class_redefined` /
/// `JitCache::invalidate_matching`.
///
/// Returning `true` reproduces the previous behaviour exactly for every triple
/// outside the twelve-class allow-list, and the two cheap terms in
/// [`synthetic_stub_kind_should_yield_to_real_bytecode`] short-circuit before
/// anything touches the class manager, so that is also the cost.
pub(crate) fn registered_native_will_run(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let Some((callback, kind)) =
        shared
            .natives
            .native_methods
            .find_with_kind(class_name, method_name, descriptor)
    else {
        return false;
    };
    // JIT round 21 wave 4 (lane compat, COMPAT21-3,
    // `CRATONVM_JIT_WILL_RUN_ASKS_RESOLVER`, default ON since r23): since `0dac905fcf` every
    // dispatch door yields a `Bridge` / `SyntheticStub` in front of loaded
    // concrete bytecode to that bytecode (`resolve_dispatch` step 3, both
    // modes), so such a native does not run and is no shadow for the compile
    // gates. Off, the answer below is the allow-list one: a `Bridge` always
    // "runs", which keeps its JDK body interpreted and its callers sealed.
    if will_run_asks_resolver_enabled(shared)
        && registration_yields_to_loaded_bytecode(
            shared,
            class_name,
            method_name,
            descriptor,
            callback,
            kind,
        )
    {
        return false;
    }
    // `CRATONVM_INTRINSIC_BODY_COMPILES` (interpreter round i3 wave 79, lane
    // L5; default off): a marked "worth it interpreted" `Intrinsic` stands in
    // front of its JDK body only at an INTERPRETED call (`resolve_dispatch`
    // step 2 is unchanged). For the compile gates it is no shadow: its answer
    // is the body's (a reviewed intrinsic is semantics-preserving), so the
    // body may compile, a compiled caller is not sealed for calling it and may
    // bind the compiled body, and the intrinsic's declines
    // (`invoke_special_bytecode_only` -> `execute`) reach that compiled body.
    if kind == cratonvm_native_api::NativeKind::Intrinsic
        && body_compilable_intrinsic(shared, class_name, method_name, descriptor)
    {
        return false;
    }
    // Opt-in `CRATONVM_PURE_BYTECODE` probe: every `--real-jdk` dispatch door
    // yields a registered `Bridge`/`Intrinsic` to real bytecode, so such a
    // native does NOT run and there is no shadow for the JIT's compile gates to
    // seal against. Without this the lever left every method that merely CALLS
    // a registered row (hashmap.put, Math.sin, ...) uncompiled: measured
    // 5x-300x slower than the same bytecode compiled. Lever off: the first
    // `&&` operand is a memoised-bool-free constant `false` path (one OnceLock
    // load), i.e. the historical answer.
    if cratonvm_native_api::unretire::pure_bytecode_for(class_name, method_name)
        && crate::vm::dispatch_policy(shared).is_jdk_only()
        && crate::runtime::env_cache::dial_applies_to_kind(kind)
        && dial_yields_to_bytecode_slow(shared, class_name, method_name, descriptor)
    {
        return false;
    }
    // JIT round 21 (lane refs2, `CRATONVM_JIT_GATES_YIELDED_BRIDGE`): a
    // `Bridge` over concrete bytecode loses at every `--real-jdk` dispatch door
    // (`resolve_dispatch` step 3), so it does not run either.
    if bridge_yields_to_loaded_bytecode_for_gates(
        shared,
        None,
        class_name,
        method_name,
        descriptor,
        kind,
    ) {
        return false;
    }
    !synthetic_stub_should_yield_to_real_bytecode(shared, class_name, method_name, descriptor)
}

/// JIT round 21 lane reflect3/4 (REFLECT4-21-1, page
/// `jr21-reflect3-bytecode-only-fallback-bodies-always-interpret`;
/// `CRATONVM_JIT_BYTECODE_ONLY_BODY`, perf, default ON, `=0` seals as before):
/// `interpreter::execute` was asked to run the bytecode of a method that a
/// registered native shadows: a native running its own JDK body
/// (`invoke_virtual_bytecode_only`, `invoke_special_bytecode_only*`).
/// Whoever asked, `execute` runs that bytecode, so a compiled body of the
/// same bytecode entered from `execute` is the same answer. When the shadow is a reviewed
/// `Intrinsic`, the method is marked re-entry-only in this VM's JIT cache
/// (`JitCache::mark_reentry_only_row`, the interpreter round i6 wave 117
/// publication) in place of `execute`'s `calls-native-shadowed-method` seal.
/// The body then compiles and OSRs for those re-entries alone. Every binding
/// door still refuses it (`JitCache::get`, `try_jit_compile_callee_hinted`),
/// so every caller keeps reaching the intrinsic and its extra steps.
///
/// This generalizes the one-row mark of JIT round 23 wave 8h
/// (`CRATONVM_JIT_LOADER_REENTRY_BODIES`, `BuiltinClassLoader.loadClassOrNull`)
/// to every intrinsic whose own body is re-entered, marked when the first
/// re-entry happens rather than from a list. A `Bridge` / `SyntheticStub`
/// that still runs is left sealed: it is not reviewed as standing in front of
/// its body. Answers whether it marked (the caller's `native_skip` is then
/// `false`). Called on `execute`'s gate path only, which `jit_gate_pass`
/// memoizes once the method passes.
pub(crate) fn mark_intrinsic_own_body_reentry_only(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let is_intrinsic = shared
        .natives
        .native_methods
        .find_with_kind(class_name, method_name, descriptor)
        .is_some_and(|(_, kind)| kind == cratonvm_native_api::NativeKind::Intrinsic);
    if !is_intrinsic {
        return false;
    }
    shared
        .jit
        .jit_cache
        .mark_reentry_only_row(class_name, method_name, descriptor);
    if None::<std::ffi::OsString>.is_some() {
        eprintln!(
            "[cratonvm-jitc] reentry-only-mark site=intrinsic-own-body {class_name}.{method_name}{descriptor}"
        );
    }
    true
}

/// COMPAT21-3's question, without its switch: is this registration a
/// non-`Intrinsic` that `resolve_dispatch` yields to the loaded declaration's
/// concrete bytecode (`registered_native_yields_to_loaded_bytecode`, the
/// generic tail's own `has_real`)? An `Intrinsic` is never asked: it may shadow
/// bytecode (step 2), and the pure-bytecode probe has its own arm in
/// [`registered_native_will_run`].
pub(crate) fn registration_yields_to_loaded_bytecode(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    callback: cratonvm_native_api::NativeCallback,
    kind: cratonvm_native_api::NativeKind,
) -> bool {
    kind != cratonvm_native_api::NativeKind::Intrinsic
        && crate::vm::registered_native_yields_to_loaded_bytecode(
            shared,
            class_name,
            method_name,
            descriptor,
            callback,
            kind,
        )
}

/// `CRATONVM_JIT_WILL_RUN_ASKS_RESOLVER` (JIT round 21 wave 4, lane compat,
/// COMPAT21-3; perf, default ON since the r23 A/B (R23-AB-FLIP), `=0` off):
/// [`registered_native_will_run`] answers "does not
/// run" for a non-`Intrinsic` registration the resolver yields to loaded
/// bytecode. Read once per VM at construction
/// (`SharedVm::jit_will_run_asks_resolver`).
#[inline]
pub(crate) fn will_run_asks_resolver_enabled(shared: &SharedVm) -> bool {
    shared.jit_will_run_asks_resolver
}

/// `CRATONVM_JIT_FORCED_SEAL_ASKS_RESOLVER` (JIT round 21 wave 4, lane compat,
/// COMPAT21-4; perf, opt-in): `jit_bridge::jit_invoke_targets_native_shadow`'s
/// `forced` arm seals a caller for a `force_native_over_real_jdk_bytecode`
/// triple only when the force interceptor would actually run its native
/// ([`forced_native_would_run`]). Read once per VM at construction
/// (`SharedVm::jit_forced_seal_asks_resolver`).
#[inline]
pub(crate) fn forced_seal_asks_resolver_enabled(shared: &SharedVm) -> bool {
    shared.jit_forced_seal_asks_resolver
}

/// Would the force interceptor (`intercept_force_registered_native{,_cached}`
/// -> [`admit_forced_native`]) run a native for `class_name.method_name
/// descriptor`? The same resolver call with the same `(true, true)` inputs, but
/// without its side effects (no `record_invocation`, no shadow census): a
/// registration that exists and is an `Intrinsic` (any other kind yields to the
/// concrete bytecode the interceptor's premise names, in both modes) and is not
/// yielded by the opt-in `CRATONVM_PURE_BYTECODE` probe under `--real-jdk`.
/// Compile-gate use only (COMPAT21-4).
pub(crate) fn forced_native_would_run(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let Some(kind) = shared
        .natives
        .native_methods
        .kind_of(class_name, method_name, descriptor)
    else {
        return false;
    };
    forced_kind_would_run(
        crate::vm::dispatch_policy(shared).is_jdk_only(),
        kind,
        cratonvm_native_api::unretire::pure_bytecode_for(class_name, method_name),
    )
}

/// [`forced_native_would_run`]'s decision on its three inputs, mirroring
/// `resolve_native_dispatch_wave1(.., compat_native_wins: true,
/// bytecode_available: true)` arm for arm: a non-`Intrinsic` returns "no
/// opinion" on its first test; an `Intrinsic` is yielded only by the pure
/// probe under `--real-jdk`.
pub(crate) fn forced_kind_would_run(
    jdk_only: bool,
    kind: cratonvm_native_api::NativeKind,
    pure_bytecode: bool,
) -> bool {
    kind == cratonvm_native_api::NativeKind::Intrinsic && !(jdk_only && pure_bytecode)
}

/// JIT round 21 (lane refs2, `CRATONVM_JIT_GATES_YIELDED_BRIDGE`, default ON,
/// `=0` restores the gates' "a registration exists" answer; page
/// `docs/feature-designs/zgc-architecture.md`):
/// does the JIT's compile / inline / callee gates' native-shadow question
/// have to answer "no shadow" for this `Bridge`, because the dispatch
/// arbitration runs the bytecode instead?
///
/// # The disagreement it closes
///
/// Under `--real-jdk` every dispatch door asks `resolve_dispatch` (directly or
/// through `registered_native_yields_to_loaded_bytecode`), whose step 3 runs
/// the selected method's `Code` over any `Bridge` registered for the triple.
/// The compile gates ([`registered_native_will_run`], the inline resolver's
/// `inline_screen_native_shadows`, the callee door's inherited-native check)
/// asked only whether a registration EXISTS. For a `Bridge` registered on a
/// class whose selected method has `Code` -- `SoftReference.refersTo` /
/// `PhantomReference.refersTo` (inherited from `Reference.refersTo`, final
/// bytecode), and `Reference.refersTo` itself -- the gates said "the native
/// runs" while every door ran the bytecode: no tier compiled
/// `Reference.refersTo` for those receivers, no caller spliced it, and every
/// call from compiled code took the by-name `invoke_or_native` cascade into an
/// interpreted `refersTo` -> `refersToImpl` -> `refersTo0` (~2.3 us a call,
/// `R21Tl3Rows softref-refersto` / `phantom-refersto` 37 000 ns per 16 vs
/// HotSpot 4-5).
///
/// # Why answering "yields" is sound
///
/// It is step 3 of `resolve_dispatch`, asked through the by-name doors' own
/// walk without its census recording
/// (`vm_exec::registered_bridge_yields_to_loaded_bytecode_census_free`; the
/// census counts DISPATCHES, a compile gate asking would inflate it; until
/// lane refs221 this function restated the step with a walk of its own):
/// `Bridge` only (an `Intrinsic` may win at step 2
/// and is the reviewed exception; a `SyntheticStub` has its own yield rule),
/// `--real-jdk` only, the registration's class loaded, and the method the
/// receiver class selects (`find_method_recursive`) non-native with `Code`.
/// Then the bytecode IS what the interpreter runs, so compiling or splicing
/// it is the interpreter's decision, not a bypass of it (the premise
/// [`registered_native_will_run`]'s stub arm already rests on). Monotone like
/// that arm: a class becomes loaded, never unloaded while a name resolves to
/// it here; a JVMTI redefine invalidates compiled code itself.
///
/// `cm`: the caller's class-manager guard when it holds one; otherwise a
/// recursive read is taken (callers may hold a guard they do not pass).
pub(crate) fn bridge_yields_to_loaded_bytecode_for_gates(
    shared: &SharedVm,
    cm: Option<&crate::classloading::ClassManager>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    kind: cratonvm_native_api::NativeKind,
) -> bool {
    if kind != cratonvm_native_api::NativeKind::Bridge
        || !crate::vm::dispatch_policy(shared).is_jdk_only()
    {
        return false;
    }
    // JIT round 21 (lane refs221, REFS221-3): the walk and the verdict are
    // the by-name doors' own (`vm_exec::selected_body_dispatches_bytecode`,
    // `resolve_dispatch`), not a restatement of step 3 -- one rule, so the
    // gates cannot drift from dispatch, and one hierarchy walk fewer here.
    crate::vm::registered_bridge_yields_to_loaded_bytecode_census_free(
        shared,
        cm,
        class_name,
        method_name,
        descriptor,
    )
}

/// [`registered_native_will_run`]'s `CRATONVM_INTRINSIC_BODY_COMPILES` arm,
/// out of line (a field read and, with the switch on, a scan of at most a
/// handful of marked rows).
#[cold]
#[inline(never)]
fn body_compilable_intrinsic(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    shared
        .natives
        .native_methods
        .is_body_compilable_intrinsic(class_name, method_name, descriptor)
}

/// Synthetic stubs are fallback implementations for fake or incomplete JDK
/// classes. When the real class bytecode is loaded and explicitly protected,
/// dispatch must prefer that bytecode over the approximate stub.
pub(crate) fn synthetic_stub_should_yield_to_real_bytecode(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let kind = shared
        .natives
        .native_methods
        .kind_of(class_name, method_name, descriptor);
    synthetic_stub_kind_should_yield_to_real_bytecode(
        shared,
        class_name,
        method_name,
        descriptor,
        kind,
    )
}

/// Variant for callers that already resolved and cached the native category in
/// method metadata. Keeping the selection predicate separate prevents a second
/// full triple hash on the first invocation of every constant-pool reference.
pub(super) fn synthetic_stub_kind_should_yield_to_real_bytecode(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    kind: Option<cratonvm_native_api::NativeKind>,
) -> bool {
    if kind != Some(cratonvm_native_api::NativeKind::SyntheticStub) {
        return false;
    }

    if !real_protected_stub_class(class_name) {
        return false;
    }

    let cm = shared.classes.class_manager.read();
    synthetic_stub_yields_with_cm(&cm, class_name, method_name, descriptor)
}

/// The class-manager half of [`synthetic_stub_kind_should_yield_to_real_bytecode`],
/// for callers that are **already holding** the read guard.
///
/// `populate_virtual_invoke_cache` is one: it takes `class_manager.read()` to
/// resolve the receiver's name and must ask this question before it publishes a
/// `VirtualNative` target. Re-acquiring the lock there would be a nested
/// `read()` — a lock-order panic in debug builds and a possible deadlock in
/// release once a writer is queued (the same trap
/// `threadpool_executor_has_real_workers` documented before it was deleted).
///
/// Split out rather than copied: this predicate has five terms and five
/// different remedies, and the whole point of the 2026-08-04 centralisation is
/// that no dispatch path gets to answer it differently from another.
///
/// The `kind`/allow-list gate is the CALLER's, deliberately: both terms are
/// string/enum work that must short-circuit before anyone touches the class
/// manager at all.
pub(super) fn synthetic_stub_yields_with_cm(
    cm: &crate::classloading::ClassManager,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    // Triples explicitly designated in `force_native_over_real_jdk_bytecode`
    // (such as carrier iterators that stash snapshot fields outside declared
    // layout) must not yield to real bytecode — real bytecode would walk fields
    // nothing populated.
    //
    // Memoized: the dispatch doors that arbitrate an allow-listed class's
    // `SyntheticStub` (the `ArrayList` family among them; `invoke.rs`,
    // `dispatch_static.rs`, `dispatch_virtual.rs`, `jit/helpers.rs`) reach this
    // per dispatch, and the unmemoized table is a sequential scan of hundreds
    // of string comparisons. The answer is a pure function of the triple.
    // Real-protected `SyntheticStub`s never override Intrinsics (only `SyntheticStub`s reach here).
    let (verdict, why) = match cm.get_loaded_class_id(class_name) {
        None => (false, "class not loaded"),
        Some(cid) => match cm.get_class(cid) {
            None => (false, "class id not in the store"),
            Some(cls) if cls.origin.is_compatibility_stub() => {
                (false, "loaded class is itself a synthetic stub")
            }
            Some(_) => match crate::classloading::find_method_recursive(
                cid,
                method_name,
                descriptor,
                &cm.class_store,
            ) {
                None => (false, "method not found on the real class or its supers"),
                Some((m, _)) if m.is_native() => (false, "real method is ACC_NATIVE"),
                Some((m, _)) if m.is_abstract() => (false, "real method is ACC_ABSTRACT"),
                Some((m, _)) if m.code().is_none() => (false, "Code attribute NOT YET DECODED"),
                Some(_) => (true, "real bytecode wins"),
            },
        },
    };
    if dbg_stub_yield() {
        eprintln!("[STUB-YIELD] {class_name}.{method_name}{descriptor} yield={verdict} — {why}");
    }
    verdict
}

/// `CRATONVM_DBG_STUB_YIELD` — trace every allow-listed `SyntheticStub`
/// arbitration and the term that decided it.
///
/// A refusal here is silent (the stub simply runs), so "the allow-list says
/// protect this class, and the census says its stub ran anyway" had no way to
/// name which of the five terms disagreed — and those five have five different
/// fixes.
pub(crate) fn dbg_stub_yield() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| None::<std::ffi::OsString>.is_some())}

/// JDK-ONLY-WAVE2: real-protected-stub class allow-list — **one predicate, both
/// dispatch paths** as of 2026-08-04.
///
/// It used to be two independently-maintained copies — this one and an inline
/// `matches!` in `vm_exec::invoke_or_native` — which is how they came to
/// differ: the cold copy listed `java/util/StringJoiner` and this one
/// deliberately omitted it, so a `SyntheticStub` native's yield-to-real-bytecode
/// verdict depended on how many times its call site had executed. The copies
/// were first centralised into one list plus one stated exception
/// (`real_protected_stub_class_cold`), and the exception was retired once the
/// defect that forced it was measured not to reproduce — see
/// [`real_protected_stub_class_common`] for that measurement.
///
/// What must ultimately replace this list: `NativeKind` alone. Under
/// `--real-jdk` a `SyntheticStub` never dispatches, so no class needs
/// protecting from one and the entire list becomes dead.
#[track_caller]
pub(crate) fn real_protected_stub_class(class_name: &str) -> bool {
    let v = crate::runtime::env_cache::real_bytecode_selector().prefers_real(class_name)
        || real_protected_stub_class_common(class_name);
    // DIAGNOSTIC (diag/which-door-arbitrates-20260824).
    //
    // Two attempts at making `java/util/ArrayList.get` yield to real bytecode
    // both ended with `invocations` unchanged at 127, in the JIT arm and under
    // `--nojit` alike, with every precondition satisfied. That is only possible
    // if the arbitration is never REACHED for this triple, and there are six
    // call sites that could reach it -- two on the static path, two in
    // `dispatch_virtual`, one inside the yield predicate itself, and one in
    // `vm_exec`.
    //
    // `#[track_caller]` rather than six hand-placed trace lines: one edit, and
    // the report cannot drift out of sync with the call sites it describes. A
    // door that never appears in this output is a door that never asks.
    if dbg_stub_door() {
        let l = std::panic::Location::caller();
        stub_door_note(l.file(), l.line(), class_name, v);    }
    v
}

/// `CRATONVM_DBG_STUB_DOOR` — tally every `real_protected_stub_class` question
/// by CALL SITE and class, dumped at exit.
///
/// A tally, not a print-per-call: this predicate is on the dispatch path and
/// `ArrayList.get` alone asks it thousands of times, so a line per call would
/// change the thing it measures and bury the one row that matters.
pub(crate) fn dbg_stub_door() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| None::<std::ffi::OsString>.is_some())}

type StubDoorKey = (&'static str, u32, String, bool);
static STUB_DOOR_TALLY: std::sync::OnceLock<
    std::sync::Mutex<std::collections::BTreeMap<StubDoorKey, u64>>,
> = std::sync::OnceLock::new();

fn stub_door_note(file: &'static str, line: u32, class_name: &str, verdict: bool) {
    let m =
        STUB_DOOR_TALLY.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeMap::new()));
    if let Ok(mut g) = m.lock() {
        *g.entry((file, line, class_name.to_string(), verdict))
            .or_insert(0) += 1;
    }
}

/// `CRATONVM_DBG_NATIVE_ENTRY` — tally native-funnel entries by CALL SITE.
///
/// Companion to [`dbg_stub_door`]: that one says which doors ASK the
/// `SyntheticStub` arbitration, this one says which code path actually INVOKES
/// a native. A triple entered millions of times that appears at no door at all
/// is a dispatch path with no arbitration in it — which is the shape both were
/// built to hunt.
///
/// It earned its keep immediately. Instrumenting only `safe_native_call` showed
/// ~10 000 entries where the probe makes 600 000 native calls; the hot paths
/// use `safe_native_call_prevalidated_objects`, and once that was counted too
/// the answer was one line: `vm/src/jit/helpers.rs` x598 302, i.e. the JIT's
/// site-cached native dispatch serves essentially all of it.
pub(crate) fn dbg_native_entry() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| None::<std::ffi::OsString>.is_some())}

static NATIVE_ENTRY_TALLY: std::sync::OnceLock<
    std::sync::Mutex<std::collections::BTreeMap<(&'static str, u32), u64>>,
> = std::sync::OnceLock::new();

pub fn native_entry_note(file: &'static str, line: u32) {
    let m =
        NATIVE_ENTRY_TALLY.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeMap::new()));
    if let Ok(mut g) = m.lock() {
        *g.entry((file, line)).or_insert(0) += 1;
    }
}

/// Print the [`dbg_native_entry`] tally, busiest call site first.
pub fn report_native_entry_tally_at_exit() {
    if !dbg_native_entry() {
        return;
    }
    let Some(m) = NATIVE_ENTRY_TALLY.get() else {
        return;
    };
    let Ok(g) = m.lock() else { return };
    let mut v: Vec<_> = g.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1));
    eprintln!("[NATIVE-ENTRY] native-funnel entries, by call site:");
    for ((file, line), n) in v.into_iter().take(12) {
        eprintln!("[NATIVE-ENTRY]   {file}:{line}  x{n}");
    }
}

/// Print the [`dbg_stub_door`] tally. Called from the same exit path as the
/// other census dumps.
pub fn report_stub_door_tally_at_exit() {
    if !dbg_stub_door() {
        return;
    }
    let Some(m) = STUB_DOOR_TALLY.get() else {
        eprintln!("[STUB-DOOR] no call recorded — the predicate was never asked at all");
        return;
    };
    let Ok(g) = m.lock() else { return };
    if g.is_empty() {
        eprintln!("[STUB-DOOR] no call recorded — the predicate was never asked at all");
        return;
    }
    eprintln!("[STUB-DOOR] real_protected_stub_class questions, by call site:");
    for ((file, line, class, verdict), n) in g.iter() {
        eprintln!("[STUB-DOOR]   {file}:{line}  {class}  -> {verdict}  x{n}");
    }
}

/// The classes **both** dispatch paths yield to real bytecode.
///
/// Kept as a `matches!` over string literals rather than a slice scan: this is
/// on the native-dispatch path, and `matches!` compiles to a length-bucketed
/// comparison chain rather than one `str` equality call per entry.
#[inline]
fn real_protected_stub_class_common(class_name: &str) -> bool {
    // `java/util/ArrayList` is the ONE conditional member of this list, and it
    // sits here rather than in the `matches!` below because of it. The reason
    // the class is allow-listed at all is in the comment down there, with its
    // measurements; the reason it needs a condition is this:
    //
    // `force_native_over_real_jdk_bytecode`'s ArrayList entry says a
    // `Map.values()` view IS a plain `java/util/ArrayList` that stashes its
    // source map in the last capacity slot of its element array and must
    // re-sync against it on read. That stopped being true on 2026-08-13 —
    // views are minted under `MAP_VIEW_CARRIERS`, which are NOT on this list
    // and ARE force-listed, so they keep their natives — and that is what makes
    // allow-listing `java/util/ArrayList` safe. But `alloc_view_carrier` still
    // has a last-resort arm that degrades to `java/util/ArrayList` when the
    // carrier class cannot be had at all. A view minted THERE, served by real
    // `ArrayList` bytecode, returns whatever its source held at creation: the
    // `Schema.getAllSequences()` shape (H2 `TestAlter`).
    //
    // So that arm publishes, and this asks. Default is "no such view exists",
    // i.e. the yield is ON — the opposite default was built first and measured
    // INERT, because a program that never calls `values()` never publishes
    // anything and the answer is memoized per call site as each site warms.
    // `cratonvm_types::arraylist_view` states the residual this leaves, and why
    // it is not reachable on any image a JDK ships.
    if class_name == "java/util/ArrayList" {
        return !cratonvm_types::arraylist_view::arraylist_classed_view_possible();
    }
    // `java/util/ArrayList$Itr`, for `hasNext`/`next`, retagged `SyntheticStub`
    // at their registration. 6.2x on `probes/KeySetBench iterList`, and it also
    // FIXES `cmeClear` -- the native did not throw `ConcurrentModificationException`
    // when a list was cleared mid-iteration.
    //
    // BOTH HALVES ARE REQUIRED AND NEITHER WORKS ALONE. Two earlier arms read as
    // failures for two different reasons, and both were the measurement rather
    // than the fix: allow-list alone does nothing (term 1 is
    // `kind != SyntheticStub -> refuse`), and retag alone is 1.56x SLOWER
    // (`resolve_native_site` refuses to cache ANY `SyntheticStub`, so the site
    // falls out of the JIT's native cache onto the generic path -- still running
    // the native, by the expensive route).
    //
    // AND IT IS SOUND ONLY BECAUSE OF THE MINT SPLIT.
    // `itr_backing_has_real_mod_count` gives any backing WITHOUT a real
    // `modCount` a different class (`cratonvm/internal/ArrayListViewItr`), so
    // this name is a guarantee. Before that split, `for (v : map.values())`
    // threw a spurious CME: HotSpot's `checkForComodification` read a view
    // carrier's first declared REFERENCE field as an int.
    //
    // Do NOT re-derive that guarantee from a class census. `probes/ItrClassProbe`
    // said `IdentityHashMap`'s values view minted its own iterator, matching
    // HotSpot -- and it still arrived here and threw. The class a receiver mints
    // in isolation is not the class it reaches the `al_itr_*` natives with.
    if class_name == "java/util/ArrayList$Itr" {
        return true;
    }
    // The AWT event trio (round 12 wave 8, lane compat2). Their natives are
    // `SyntheticStub`s (`native-awt/src/lib.rs` `register_awt_natives`), and
    // the synthetic `invokeLater` / `postEvent` post to a Rust queue that only
    // the JDK's `EventDispatchThread` pumps -- a thread only the REAL
    // `EventQueue.postEventPrivate` starts. So under `--compatible` an
    // `invokeLater` Runnable never ran, `invokeAndWait` hung, and
    // `isDispatchThread()` was false on the dispatch thread
    // (`docs/feature-designs/real-jdk-mode.md`,
    // `docs/feature-designs/native-dispatch-architecture.md`).
    // Yielding to the loaded JDK bytecode is what `--real-jdk` already does;
    // a synthetic-JDK image keeps the stubs (no real `Code`, see
    // `synthetic_stub_yields_with_cm`). Kill switch
    // `CRATONVM_AWT_EVENTS_REAL_BYTECODE=0`.
    if matches!(
        class_name,
        "java/awt/EventQueue" | "javax/swing/SwingUtilities" | "java/awt/event/InvocationEvent"
    ) {
        return true;
    }
    // The throwable family (JIT round 21 page
    // `jr21-caught-throw-cost-split`, row 1's `--compatible` residue). Lane T
    // retired its bridges whole, which re-tags them `SyntheticStub`; off this
    // list a stub still WINS over the real bytecode under `--compatible`, and
    // a `SyntheticStub` is never site-cached (`resolve_native_site`), so every
    // compiled `getMessage()` / `getCause()` / `<init>` paid the whole by-name
    // `invoke_or_native` walk: `Throwable.getMessage()` 2700-4500 ns against
    // 9-17 under `--real-jdk` (HotSpot 2), and a caught `AssertionError` whose
    // handler reads the message 2.7x its `--real-jdk` cost. Yielding is what
    // `--real-jdk` already runs (the real `Throwable.<init>` chain and
    // accessors, lane T's measured corpus); a synthetic-JDK image keeps the
    // stubs (`synthetic_stub_yields_with_cm`: no real `Code`). Kill switch
    // `CRATONVM_COMPAT_THROWABLE_REAL_BYTECODE=0`.
    if cratonvm_native_api::retired_shadow::is_lane_t_throwable_class(class_name) {
        return true;
    }
    matches!(
        class_name,
        "java/util/concurrent/locks/ReentrantLock"
            | "java/util/concurrent/LinkedBlockingDeque"
            | "java/util/concurrent/atomic/AtomicBoolean"
            | "java/util/EnumSet"
            // The permission family. `register_essential_natives` replaces
            // their `(String)` / `(String,String)` constructors with a closure
            // that writes `name` and returns — correct when this VM FABRICATED
            // these classes (`b448f2039`), and a silent mutilation of the real
            // ones now that those are loaded: `BasicPermission.init(name)` and
            // `PropertyPermission.init(getMask(actions))` never ran, leaving
            // `path` null and `mask` 0, so `implies()` NPEd, an invalid actions
            // string was accepted where the JDK throws, and a serialization
            // round trip died with "invalid actions mask".
            //
            // Correctness, not throughput — and the whole family, because the
            // closure is registered over all five in one loop and every one of
            // them inherits a real `init` from `BasicPermission`.
            | "java/security/Permission"
            | "java/security/BasicPermission"
            | "java/lang/RuntimePermission"
            | "java/util/PropertyPermission"
            | "java/util/logging/LoggingPermission"
            // `java/util/Objects` is registered TWICE, and only the
            // real-JDK-path registration is a `SyntheticStub`:
            // `register_synthetic_overrides` installs it `Intrinsic` (on a
            // synthetic image those bodies ARE the implementation and this
            // entry cannot reach them, because the kind gate runs first),
            // while `register_annotation_overrides` -- the real-JDK essentials
            // path -- installs it `SyntheticStub` as a partial-stub-boot
            // fallback. This entry arms the yield for that second one.
            //
            // Unlike its neighbours the motive is throughput, not correctness:
            // the stub answers the same as the real bytecode, but winning
            // meant `Objects.equals` / `hashCode` / `requireNonNull` /
            // `isNull` could never be JIT-compiled, because a static's
            // native-vs-bytecode arbitration is `NativeKind` alone.
            // `requireNonNull` is among the most-called methods in the JDK and
            // in Spring, so it was a VM-wide tax. Measured 10M calls,
            // nativized against a byte-identical local static: equals
            // 203.5 -> 47.2 ns, hashCode 174.1 -> 47.8, requireNonNull
            // 124.3 -> 48.6, isNull 91.8 -> 24.6.
            | "java/util/Objects"
            // `java/util/ArrayList`, for `get`/`size`/`isEmpty` -- whose natives
            // are already `SyntheticStub` (the census says so; the ambient
            // `set_category(Bridge)` around their registration is NOT what
            // lands, because `register` adjudicates the kind itself). Like the
            // `Objects` entry above, the motive is throughput: the native cost
            // 11x its own JDK bytecode, because pinning the native also pins
            // the method out of the JIT.
            //
            // MEASURED, `probes/KeySetBench idxList` (1000 `list.get(i)` calls
            // per row), us/call, three interleaved rounds, against the same
            // binary with this one entry removed:
            //
            //     idxList        752 -> 68     11.1x
            //     toArrHoisted   110 -> 96      1.15x   (a bonus; Spring's door)
            //     rawArr          19 -> 18      flat    (control: no collection)
            //     iterList       987 -> 971     flat    (the iterator does not
            //                                            go through `get`)
            //
            // Two earlier attempts concluded this "did not engage" and were
            // reverted. Both were wrong, and both were wrong for the same
            // reason: they were scored on the native census's `invocations`,
            // which SATURATES -- it reads the same 127 at 5 000 and 50 000
            // iterations, and prints `invocations_complete: false` beside the
            // number to say so. The arbitration was always reached (a
            // `#[track_caller]` tally on this predicate shows this class asked
            // at all three virtual doors) and the yield always worked; only the
            // instrument was blind. Score this class by TIME.
            //
            // The entry itself is NOT here: it is the guarded early return at
            // the top of this function, because it is the one member of this
            // list that is conditional. See there, and see
            // `cratonvm_types::arraylist_view`.
            // JDK-ONLY-WAVE2, 2026-08-06. `native_es_execute` (retagged
            // `SyntheticStub` in `native-builtins/src/util_concurrent_ext.rs`)
            // is the compatibility stand-in for CratonVM's synthetic 2-field
            // `Executors.new*ThreadPool()` receiver. Every factory shortcut now
            // drives the real `ThreadPoolExecutor.<init>`
            // (`initialize_real_thread_pool_executor`), so on an image where
            // the real class bytes are loaded there is no synthetic receiver
            // left and the real `execute()` body is right for ALL of them.
            //
            // This entry is what replaced eight hand-written receiver-shape
            // probes ("does the receiver's `workers` field hold an object?")
            // spread across four files in `vm`. It answers the same question
            // class-scoped instead of per-instance, which is only sound
            // *because* the per-instance distinction was eliminated first —
            // reinstating a synthetic-layout `ThreadPoolExecutor` producer
            // without also reinstating those probes would send it to real
            // bytecode that dereferences a null `ctl`/`mainLock`.
            //
            // On a synthetic-JDK image the class IS a compatibility stub, the
            // predicate below finds no real `execute()` body, and the native
            // still runs — which is the only mode that still needs it.
            | "java/util/concurrent/ThreadPoolExecutor"
            // The fallback bridge is needed only if bootstrap had to
            // synthesize Instant.  With a loaded real JDK Instant, every
            // factory must run its real bytecode so the result has the
            // real field layout and ISO-8601 `toString()` semantics.
            | "java/time/Instant"
            // Spring Boot's loader decodes central-directory DOS times via
            // ZonedDateTime.of(...). The synthetic bridge stores its
            // fields in a compact layout that is incompatible with the
            // loaded JDK class, turning historical ZIP timestamps into
            // the current clock value when converted to an Instant.
            | "java/time/ZonedDateTime"
            | "java/io/FileInputStream"
            | "java/lang/ref/Cleaner"
            | "java/lang/ref/Cleaner$Cleanable"
            | "java/lang/management/ManagementFactory"
            // Protected on BOTH paths since 2026-08-04. It was cold-path-only
            // from 2026-07-10, because yielding this class's SyntheticStub
            // natives to real bytecode on the warm path once tripped a
            // deterministic heap-reference-integrity defect: the
            // `gen_heap::read_slot` "corrupt Value cell" / HIB-CV-32 guard
            // fired reading `StringJoiner`'s own `size` / `elts` back after a
            // `putfield`, from the SECOND `add()` onward. Re-measured
            // 2026-08-04 with the merge applied — 40k `add()` calls under
            // `-Xmx64m` with per-iteration allocation churn, seven intermediate
            // consistency checks, against a HotSpot control — and it did not
            // reproduce on either `--real-jdk` or `--fast-boot`.
            //
            // Dropping it from the cold path instead is the other half of the
            // trap and is NOT an option: the synthetic `add()` writes a 5-field
            // fake layout (`delim/prefix/suffix/elements-ArrayList/emptyValue`)
            // over the real 7-field class
            // (`prefix/delimiter/suffix/elts[]/size/len/emptyValue`), reads
            // slot 3 — real `elts`, null — and no-ops, so `size` never moves
            // and `toString()` renders just prefix+suffix. A silently empty
            // join, not a crash. See
            // `stringjoiner-synthetic-native-real-jdk-field-mismatch-FIXED.md`.
            | "java/util/StringJoiner"
    )
}





/// Every class the allow-list protects, as a floor corpus for
/// `every_allowlisted_class_is_protected`.
///
/// Not a dispatch input — the predicate above stays a `matches!`. It exists so
/// that deleting a class from the allow-list fails a test instead of silently
/// handing that class's `SyntheticStub` natives back to a synthetic layout.
#[cfg(test)]
pub(crate) const REAL_PROTECTED_STUB_CORPUS: &[&str] = &[
    "java/util/concurrent/locks/ReentrantLock",
    "java/util/concurrent/LinkedBlockingDeque",
    "java/util/concurrent/atomic/AtomicBoolean",
    "java/util/EnumSet",
    "java/util/concurrent/ThreadPoolExecutor",
    "java/time/Instant",
    "java/time/ZonedDateTime",
    "java/io/FileInputStream",
    "java/lang/ref/Cleaner",
    "java/lang/ref/Cleaner$Cleanable",
    "java/lang/management/ManagementFactory",
    "java/util/StringJoiner",
    // Round 12 wave 8 (lane compat2), under the default-on
    // `CRATONVM_AWT_EVENTS_REAL_BYTECODE`.
    "java/awt/EventQueue",
    "javax/swing/SwingUtilities",
    "java/awt/event/InvocationEvent",
    // JIT round 21 page `jr21-caught-throw-cost-split`, under the default-on
    // `CRATONVM_COMPAT_THROWABLE_REAL_BYTECODE`: two of lane T's 62.
    "java/lang/Throwable",
    "java/lang/AssertionError",
];

#[cfg(test)]
mod real_protected_stub_tests;

#[cfg(test)]
mod threadpool_receiver_shape_tests;

/// [`try_stackless_invoke`] step 1's primary native lookup, routed through the
/// §7 policy (`docs/book/src/user-guide/real-jdk-mode.md`).
///
/// This name-only door defers Bridge and SyntheticStub registrations until the
/// declaring method has been resolved. Concrete bytecode is authoritative in
/// both policies without a class-name allow-list. A reviewed Intrinsic is the
/// only registration that can win before method resolution.
#[inline]
pub(super) fn resolve_step1_native(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    dispatch_class_override: Option<crate::classloading::ClassId>,
    id_out: &mut Option<cratonvm_native_api::NativeMethodId>,
    _refusal: &mut Option<cratonvm_types::error::JdkOnlyViolation>,
) -> Option<cratonvm_native_api::NativeCallback> {
    let registry = &shared.natives.native_methods;
    let id = registry.resolve_id(class_name, method_name, descriptor)?;
    let callback = registry.callback_of(id)?;
    // `kind_of_id` reports the slot's true kind. `find_with_kind` would report
    // `Bridge` on its descriptor-quirk cold path; the difference is invisible
    // in `Compatible` mode (every kind yields the same callback) and strictly
    // more accurate under `JdkOnly`.
    let kind = registry
        .kind_of_id(id)
        .unwrap_or(cratonvm_native_api::NativeKind::Bridge);
    // This door runs before the declaring method has been resolved. A name-only
    // Bridge or SyntheticStub is therefore not enough to overtake concrete JDK
    // bytecode. Defer both to the resolved-method path, which applies
    // `resolve_dispatch` with the actual declaring class and method. A reviewed
    // Intrinsic is the sole registration allowed to win without that lookup.
    if kind == cratonvm_native_api::NativeKind::Intrinsic {
        // Intrinsics targeting host application classes must not intercept classes defined by user loaders.
        if cratonvm_native_builtins::classloader::any_defining_loader_registered(&shared.loader_identity)
            && !crate::runtime::interpreter::constants::is_global_resolution_namespace(class_name)
        {
            if let Some(cid) = dispatch_class_override {
                let is_user_loader = cratonvm_native_builtins::classloader::defining_loader_for(
                    &shared.loader_identity,
                    cid.as_u32(),
                )
                .is_some()
                    || shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(cid)
                        .map(|c| matches!(c.loader_id, cratonvm_types::ClassLoaderId::UserDefined(_)))
                        .unwrap_or(false);
                if is_user_loader {
                    return None;
                }
            }
        }
        *id_out = Some(id);
        Some(callback)
    } else {
        None
    }
}

/// The method-code lookup shared by the strict-mode retirement dial, with lock acquisition made a parameter.
///
/// # Why `reentrant` is not a style choice
///
/// `class_manager` is an [`OrderedPlRwLock`](cratonvm_types::lock_order::OrderedPlRwLock),
/// and `parking_lot`'s plain `read()` is **not reentrant**: a thread that
/// already holds a read guard and takes another one deadlocks against a queued
/// writer. Under lock-order enforcement that is a panic, which is loud and
/// findable; enforcement is compiled out of a release build, where the same
/// code is a silent hang instead.
///
/// [`jdk_only_dial_yields_to_bytecode`] is called from doors that DO hold the
/// guard -- `invoke_or_native`'s superclass walk holds it across the whole walk
/// -- so it passes `true` and gets `read_recursive()`, which is documented as
/// existing for exactly this case. Re-entering a lock the thread already holds
/// adds no edge to the wait-for graph.
fn dispatch_has_code(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    dispatch_class_override: Option<crate::classloading::ClassId>,
    reentrant: bool,
) -> bool {
    let cm = if reentrant {
        shared.classes.class_manager.read_recursive()
    } else {
        shared.classes.class_manager.read()
    };
    // Same start class the call site's own superclass walk uses: the
    // loader-precise override when the caller resolved one, else the flat
    // name lookup.
    let Some(start) = dispatch_class_override.or_else(|| cm.get_loaded_class_id(class_name)) else {
        return false;
    };
    // Routed through `MemberResolver` rather than a bare
    // `find_method_recursive`, which is what `runtime::resolve::guard`'s
    // metadata-table gate asks of a NEW site: the allowlist is a migration
    // ledger that only ever shrinks, so adding a row (and raising the
    // interpreter's per-needle budget with it) would have inverted the ratchet
    // this very decision is meant to respect.
    //
    // Same walk, same answer, and it populates the per-VM `LinkResolver` with
    // the resolution the invoke about to happen will ask for anyway. `cm` is
    // passed in, per `declared_method`'s contract, so the read guard this
    // function already holds stays the lock the call site established.
    //
    // An unresolvable method is `false`, exactly as the bare walk's `None` was:
    // no `Code` means the bridge keeps the call.
    let resolver = crate::runtime::resolve::MemberResolver::new(shared);
    let Ok(scoped) = resolver.declared_method(&cm, resolver.scope(start), method_name, descriptor)
    else {
        return false;
    };
    let Ok((declaring_id, index)) = resolver.adopt(scoped) else {
        return false;
    };
    cm.class_store
        .get(declaring_id)
        .and_then(|declaring| declaring.methods.get(index as usize))
        .is_some_and(|method| method.code().is_some())
}

/// The enforcement dial (`CRATONVM_ENFORCE_NATIVE_SHADOW`), asked at a
/// dispatch door **other than** step 1.
///
/// # What was wrong
///
/// `env_cache::jdk_only_enforce_shadow_for` had exactly one live call site
/// tree-wide -- `resolve_step1_native`, a few hundred lines above -- which
/// `H17-2` §5 established by grep over the whole repository rather than by
/// inference from behaviour. So "arming a class" armed only the subset of that
/// class's dispatches that reached step 1 **cold**. A dispatch served by a warm
/// invoke-cache entry, or by `invoke_or_native`'s registry-first probe, or by
/// that probe's superclass walk, ran the native regardless of the dial.
///
/// That is not a small discrepancy in a measurement. Four records
/// (`H0-3`, `H0-4`, `H14-3`, `H15`) priced retirements with this instrument,
/// and a retirement removes the *registration*, so under a retirement **every**
/// door misses. The dial priced a hybrid no retirement can reach: `H16-3`
/// photographed a real `Node[]` holding one real node and two fabrications.
/// The asymmetry that survives is the one to quote: **an armed FAILURE is real;
/// an armed ZERO is unreliable.**
///
/// # What this answers
///
/// The same three-way question step 1 asks, in the same order, with the
/// cheapest test first so that the doors this is called from -- one of which is
/// the hottest native path in the VM -- pay a `Copy` field read and an enum
/// compare on a default `--fast-boot` run and nothing else:
///
/// 1. `Bridge` only. `Intrinsic` is §1.4's reviewed exception and is taken
///    regardless; `SyntheticStub` is refused by §1.3 before any of this.
/// 2. `--real-jdk` only. The dial has never had meaning in `Compatible` mode
///    and must not acquire one here.
/// 3. The dial must cover THIS receiver class. `jdk_only_enforce_shadow()`
///    alone answers "is anything armed", which under a prefix list is true for
///    every class in the VM -- asking it here would enforce one subsystem's
///    dial across the whole process, which is the 32/17 -> 3/46 collapse the
///    scoping exists to avoid.
/// 4. And only then the hierarchy walk, which is the expensive part.
///
/// # The conservatism is deliberate and is NOT a full retirement
///
/// A yield needs concrete bytecode to yield *to*. A real retirement of a
/// registration whose method has no `Code` produces an `AbstractMethodError` or
/// a `MissingNative`; this dial produces the native, exactly as step 1 always
/// has. So an armed run still under-prices a retirement of a triple with no
/// bytecode behind it, and it must be read that way. What it no longer does is
/// under-price by an unknown factor that depends on which door the call
/// happened to arrive through.
///
/// # Per-call-site drift, the hazard this file has already paid for once
///
/// A `java/lang/String` force-native arm was deleted on 2026-08-04 because "a
/// method's behaviour started depending on how many times its call site had
/// run", and `forced_native_string_arm_stays_deleted` in this module is the
/// gate that keeps it deleted. Consulting a dial at a **memoized** door
/// recreates that defect unless the memo is dial-aware.
///
/// It is handled here by asking on every dispatch rather than at publication:
/// `revalidate_cached_native` calls this on every warm hit, not once at fill
/// time, and answers `None` (the eviction signal every caller already
/// implements) when the dial yields; `populate_invoke_cache` calls it before
/// deciding what to cache, so an armed class's bridge is never published to a
/// call site in the first place. Cold and warm therefore give the same answer
/// for the same triple, which is the property whose absence was the 2026-08-04
/// bug.
#[inline]
pub(crate) fn jdk_only_dial_yields_to_bytecode(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    kind: cratonvm_native_api::NativeKind,
) -> bool {
    if !crate::runtime::env_cache::dial_applies_to_kind(kind) {
        return false;
    }
    if !crate::vm::dispatch_policy(shared).is_jdk_only() {
        return false;
    }
    if !crate::runtime::env_cache::jdk_only_enforce_shadow_for(class_name) {
        return false;
    }
    // The lever's method-level exemptions (`unretire::lever_keeps_hotspot_intrinsic`).
    if cratonvm_native_api::unretire::pure_bytecode()
        && cratonvm_native_api::unretire::lever_keeps_hotspot_intrinsic(class_name, method_name)
    {
        return false;
    }
    dial_yields_to_bytecode_slow(shared, class_name, method_name, descriptor)
}

/// [`jdk_only_dial_yields_to_bytecode`]'s tail, out of line so the three
/// early-outs above can inline into the hot doors without dragging a
/// hierarchy walk in with them.
#[cold]
#[inline(never)]
fn dial_yields_to_bytecode_slow(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    dispatch_has_code(shared, class_name, method_name, descriptor, None, true)
}

/// `--real-jdk`: must a superclass walk that started at `start` and found a
/// native registered on `ancestor` ignore it, because the walk passed a class
/// whose row for `(method_name, descriptor)` is RETIRED?
///
/// A retired row says the real bytecode is authoritative for that class; strict
/// mode refused it, and the ancestor walks (`invoke_or_native`, the virtual
/// cache populate, the vtable shadow probe, the stackless step-1 walk, the JIT
/// site cache) then ran a live `Bridge` further up for every receiver of the
/// retired class -- `r13w13-shadow5-ancestor-bridges-over-retired-rows`, 19
/// pairs such as `TreeSet.toArray()` answered by `AbstractCollection`'s
/// native. With this, a `Bridge` above a retired row is treated as absent and
/// the walk goes on exactly as it does for an ancestor with no registration
/// (a declaring ancestor ends it in bytecode). Only `Bridge` is masked: an
/// `Intrinsic` is the reviewed exception and still wins.
///
/// Every class from the walk's start up to, NOT including, `ancestor` is asked
/// (the start included: the walk only runs when it does not declare the
/// method, and its own row was the one refused). `from` is `(start,
/// receiver)`: a walk that starts at the call site's class rather than at the
/// receiver's passes the receiver's runtime class too, and its chain is asked
/// first -- the retired row may sit BELOW the call site's class (a
/// `TreeSet` receiver of an `AbstractSet`-typed call). A receiver whose chain
/// does not reach `ancestor` is ignored. Asked only when a native was actually
/// found, so a walk that finds nothing pays nothing, and `Compatible` mode
/// pays one field compare inside the registry (see
/// `NativeMethodRegistry::retired_row_masks_ancestor_bridges` for the kill
/// switch). Takes the caller's class-manager guard: every walk holds one.
pub(crate) fn retired_row_masks_ancestor_bridge(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    from: (
        crate::classloading::ClassId,
        Option<crate::classloading::ClassId>,
    ),
    ancestor: crate::classloading::ClassId,
    method_name: &str,
    descriptor: &str,
    kind: cratonvm_native_api::NativeKind,
) -> bool {
    if kind != cratonvm_native_api::NativeKind::Bridge {
        return false;
    }
    let registry = &shared.natives.native_methods;
    if registry.compatibility_mode() != cratonvm_types::compat::CompatibilityMode::JdkOnly {
        return false;
    }
    // An `ACC_NATIVE` method on the ancestor (`Object.hashCode`): the
    // registered native IS its implementation, so the bytecode path would
    // reach it anyway, the slow way. Not a shadow; not masked.
    if cm
        .get_class(ancestor)
        .and_then(|class| class.find_method(method_name, descriptor))
        .is_some_and(|method| method.is_native())
    {
        return false;
    }
    // `Some(found)` when the chain from `begin` reaches `ancestor`, `None`
    // when it does not. Bounded like every walk here: by the superclass
    // chain, which ends.
    let chain_passes_retired_row = |begin: crate::classloading::ClassId| -> Option<bool> {
        let mut found = false;
        let mut cid = begin;
        while cid != ancestor {
            let class = cm.get_class(cid)?;
            if !found
                && registry.retired_row_masks_ancestor_bridges(&class.name, method_name, descriptor)
            {
                found = true;
            }
            cid = class.superclass?;
        }
        Some(found)
    };
    let (start, receiver) = from;
    if let Some(receiver) = receiver {
        if chain_passes_retired_row(receiver) == Some(true) {
            return true;
        }
    }
    chain_passes_retired_row(start).unwrap_or(false)
}

/// What [`ancestor_native`] found above a class that does not declare the
/// method itself.
pub(crate) enum AncestorNative<'cm> {
    /// `owner` registers native `id` (`kind`), and neither the retired-row
    /// mask nor the caller's `accept` refused it.
    Native {
        owner: crate::classloading::ClassId,
        owner_name: &'cm str,
        id: cratonvm_native_api::NativeMethodId,
        kind: cratonvm_native_api::NativeKind,
    },
    /// An ancestor declares the method in bytecode and has no accepted
    /// native: its bytecode wins over every native further up.
    Bytecode,
    /// The caller's `guard` ended the walk.
    Stopped,
    /// An ancestor's `ClassId` did not resolve in the class manager.
    Unresolved,
    /// The superclass chain ended with neither.
    ChainEnd,
}

/// The caller's per-ancestor redefine guard for [`ancestor_native`].
pub(crate) enum AncestorGuard {
    /// Look the ancestor's native up.
    Look,
    /// Treat the ancestor as registering nothing (its bytecode still ends
    /// the walk).
    SkipNative,
    /// End the walk here.
    EndWalk,
}

/// SH14-1 (round 15 wave 2, lane jdkonly): the superclass walk for a
/// registered native, written once. Five dispatch doors carry their own copy
/// (`vm_exec::invoke_or_native`, `dispatch_virtual`'s populate walk and vtable
/// shadow probe, `invoke::try_stackless_invoke` step 1, the JIT site cache's
/// `resolve_native_owner_for_receiver`), and the copies drifted: the redefine
/// guard, the enforcement dial and the retired-row mask each had to be added
/// per copy.
///
/// The rule every copy shares: walk up from `from.0` (which does not declare
/// the method); at each ancestor, a native that `guard` lets it look up,
/// that the retired-row mask does not mask ([`retired_row_masks_ancestor_bridge`]
/// with `from`) and that `accept` accepts ends the walk as
/// [`AncestorNative::Native`]; otherwise an ancestor that declares the method
/// in bytecode ends it as [`AncestorNative::Bytecode`] (the S107 rule: that
/// bytecode wins over deeper natives). The lookup is `resolve_id` +
/// `kind_of_id`, the copies' own; `accept` is where a door adds what only it
/// asks (the dial, a callback check), and it runs only for an unmasked
/// native, as every copy's short-circuit did. Takes the caller's class-manager
/// guard.
///
/// Converted (behind `NativeMethodRegistry::ancestor_walk_helper`,
/// `CRATONVM_ANCESTOR_WALK_HELPER`): the JIT site cache and the stackless
/// walk. The rest, with what keeps them apart, are listed in
/// `docs/feature-designs/jit-ir-optimizations.md`.
pub(crate) fn ancestor_native<'cm>(
    shared: &SharedVm,
    cm: &'cm crate::classloading::ClassManager,
    from: (
        crate::classloading::ClassId,
        Option<crate::classloading::ClassId>,
    ),
    method_name: &str,
    descriptor: &str,
    mut guard: impl FnMut(crate::classloading::ClassId, &crate::classloading::Class) -> AncestorGuard,
    mut accept: impl FnMut(
        &crate::classloading::Class,
        cratonvm_native_api::NativeMethodId,
        cratonvm_native_api::NativeKind,
    ) -> bool,
) -> AncestorNative<'cm> {
    let registry = &shared.natives.native_methods;
    let mut cid = from.0;
    loop {
        let Some(parent_id) = cm.get_class(cid).and_then(|class| class.superclass) else {
            return AncestorNative::ChainEnd;
        };
        let Some(parent) = cm.get_class(parent_id) else {
            return AncestorNative::Unresolved;
        };
        let look = match guard(parent_id, parent) {
            AncestorGuard::Look => true,
            AncestorGuard::SkipNative => false,
            AncestorGuard::EndWalk => return AncestorNative::Stopped,
        };
        if look {
            if let Some(id) = registry.resolve_id(&parent.name, method_name, descriptor) {
                let kind = registry
                    .kind_of_id(id)
                    .unwrap_or(cratonvm_native_api::NativeKind::Bridge);
                if !retired_row_masks_ancestor_bridge(
                    shared,
                    cm,
                    from,
                    parent_id,
                    method_name,
                    descriptor,
                    kind,
                ) && accept(parent, id, kind)
                {
                    return AncestorNative::Native {
                        owner: parent_id,
                        owner_name: &*parent.name,
                        id,
                        kind,
                    };
                }
            }
        }
        if parent.find_method(method_name, descriptor).is_some() {
            return AncestorNative::Bytecode;
        }
        cid = parent_id;
    }
}

/// Resolve the complete identity stored by a warmed native invoke target.
///
/// `find` alone discards both the stable registry slot and `NativeKind`, which
/// used to force cache hits back through constant-pool resolution, a
/// class-manager lock and a second triple hash. Keep all three values together
/// at population time so the steady state remains genuinely O(1).
#[inline]
pub(super) fn resolve_cached_native_registration(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<(
    cratonvm_native_api::NativeCallback,
    cratonvm_native_api::NativeMethodId,
    cratonvm_native_api::NativeKind,
)> {
    let registry = &shared.natives.native_methods;
    let id = registry.resolve_id(class_name, method_name, descriptor)?;
    Some((registry.callback_of(id)?, id, registry.kind_of_id(id)?))
}

/// Admit a virtual native cache entry using the already-held class manager.
/// The selected Java body remains authoritative in both launch policies. Keeping
/// the guard explicit avoids recursively acquiring a normal read lock while a
/// class-loading writer is waiting.
pub(crate) fn resolve_cached_native_registration_with_cm(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<(
    cratonvm_native_api::NativeCallback,
    cratonvm_native_api::NativeMethodId,
    cratonvm_native_api::NativeKind,
)> {
    let registration =
        resolve_cached_native_registration(shared, class_name, method_name, descriptor)?;
    let Some(start) = cm.get_loaded_class_id(class_name) else {
        return Some(registration);
    };
    let resolver = crate::runtime::resolve::MemberResolver::new(shared);
    let Ok(scoped) = resolver.declared_method(cm, resolver.scope(start), method_name, descriptor)
    else {
        return Some(registration);
    };
    let Ok((declaring_id, index)) = resolver.adopt(scoped) else {
        return Some(registration);
    };
    let class = cm.get_class(declaring_id)?;
    let method = class.methods.get(index as usize)?;
    crate::vm::resolve_dispatch_in(
        &shared.natives.native_shadow_sink,
        crate::vm::dispatch_policy(shared),
        class,
        method,
        Some((registration.0, registration.2)),
    )
    .native_callback()?;
    Some(registration)
}

/// Revalidate and count a warmed native target without a registry triple hash.
/// Both policies redeem the live slot and yield to an authoritative Java body.
/// A class loaded after cache publication must not keep running an older stub.
#[inline]
pub(super) fn revalidate_cached_native(
    shared: &SharedVm,
    id: cratonvm_native_api::NativeMethodId,
    cached_callback: cratonvm_native_api::NativeCallback,
    cached_kind: cratonvm_native_api::NativeKind,
) -> Option<cratonvm_native_api::NativeCallback> {
    let registry = &shared.natives.native_methods;
    let policy = crate::vm::dispatch_policy(shared);

    // Native slots are updated in place on re-registration. Redeeming both
    // values before the compatible fast return prevents any warmed entry from
    // defeating last-write-wins while retaining indexed O(1) access.
    let callback = registry.callback_of(id).unwrap_or(cached_callback);
    let kind = registry.kind_of_id(id).unwrap_or(cached_kind);
    if let Some((class_name, method_name, descriptor)) = registry.triple_of(id) {
        if crate::vm::registered_native_yields_to_loaded_bytecode(
            shared,
            class_name,
            method_name,
            descriptor,
            callback,
            kind,
        ) {
            return None;
        }
    }

    if !policy.is_jdk_only() {
        // REVALIDATE MEANS REVALIDATE. A warmed target is published once and
        // then redeemed forever, and until 2026-08-05 the Compatible arm
        // redeemed it without ever re-asking whether a `SyntheticStub` should
        // now yield to real bytecode. The arbitration answers "yield" every
        // time it is ASKED — measured, on both an isolated probe and a Spring
        // Boot run — and the census still counted 2 889
        // `AtomicBoolean.compareAndSet` stub dispatches, because the warmed
        // path is the one that never asked.
        //
        // `AtomicBoolean` and `java/time/Instant` are on
        // `real_protected_stub_class_common`'s allow-list, so the stated policy
        // is that their real bytecode wins once loaded. A target published
        // before that class finished loading pinned the stub for the rest of
        // the run.
        //
        // Returning `None` is the eviction signal every caller already
        // implements (`invoke_cache.evict(...)` then `CacheMiss`), so the site
        // re-resolves through a path that does arbitrate. Cost on the hot path
        // is one integer compare: only a `SyntheticStub` gets as far as
        // materialising its triple, and only the eleven allow-listed classes
        // reach the class manager.
        if kind == cratonvm_native_api::NativeKind::SyntheticStub {
            if let Some((class_name, method_name, descriptor)) = registry.triple_of(id) {
                if synthetic_stub_kind_should_yield_to_real_bytecode(
                    shared,
                    class_name,
                    method_name,
                    descriptor,
                    Some(kind),
                ) {
                    return None;
                }
            }
        }
        registry.record_invocation(id);
        return Some(callback);
    }

    let (class_name, method_name, descriptor) = registry.triple_of(id)?;
    match crate::vm::resolve_native_dispatch_wave1_in(
        &shared.natives.native_shadow_sink,
        crate::vm::DispatchDoor::CacheRevalidate,
        policy,
        class_name,
        method_name,
        descriptor,
        Some((callback, kind)),
        // A published native target has already won this call site's
        // compatibility decision.
        true,
        // Concrete bytecode precedence was decided before publication. If
        // redefinition can change that fact, the RedefineGate is checked and
        // evicts this target before we get here. (Round 13 wave 7: "decided"
        // means the same answer step 1 gives a cold call, and for an unarmed
        // `Bridge` under `--real-jdk` that answer is the bridge, at every
        // door. §1.4 is enforced per family by `retired_shadow.rs`, not here;
        // forcing step 3 at this door alone would make a warm call run
        // different code from a cold one.)
        //
        // ...with ONE exception, and it is the whole of `H17-3` §6: the
        // enforcement dial is process-static configuration that was not
        // consulted at publication either, because until now nothing but step 1
        // consulted it anywhere. A warmed entry published before the dial was
        // read would otherwise pin the bridge for the life of the process,
        // which is precisely how an armed run came to price a hybrid. Asking
        // here -- on every warm hit, not once at fill time -- is what makes the
        // warm and cold answers agree for the same triple. `None` from the
        // resolver is the eviction signal every caller of this function already
        // implements, so the site re-resolves through step 1, which asks the
        // same question and reaches the same bytecode.
        jdk_only_dial_yields_to_bytecode(shared, class_name, method_name, descriptor, kind),
    ) {
        Some(decision) => {
            let callback = decision.native_callback()?;
            registry.record_invocation(id);
            Some(callback)
        }
        None => None,
    }
}

#[cfg(test)]
mod enforcement_dial_door_tests;

#[cfg(test)]
mod string_builder_layout_override_tests;

#[cfg(test)]
mod redefine_immunity_tests;

#[cfg(test)]
mod intercept_shape_tests;

/// G34-1 — the deliberate ABSENCES from [`force_native_over_real_jdk_bytecode`].
///
/// A test module for things that are not there needs a reason to exist, and
/// this is it: `G29-1` §6 recorded that its author could not tell whether the
/// newly registered `java/net/http/HttpHeaders` readers would ever run, because
/// the class is a real one with real bytecode and is not on the force list. The
/// answer (MEASURED, see the banner on that function) is that they run anyway —
/// the force list is not the gate. Someone who re-derives the question from
/// reading alone will reach for "add HttpHeaders to the list" as the fix, and
/// that would convert every `HttpHeaders` inline cache in the VM to
/// `VirtualNative` for real receivers too. These tests make the absence
/// deliberate rather than accidental.
/// JIT round 21, lane refs2 (`CRATONVM_JIT_GATES_YIELDED_BRIDGE`): the gates'
/// yielded-bridge rule answers only for a `--real-jdk` `Bridge` over loaded
/// `Code`, and the three gates ask it.
#[cfg(test)]
mod jr21_refs2_yielded_bridge_gate_tests;
