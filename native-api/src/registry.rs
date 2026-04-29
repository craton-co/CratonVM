//! Registry for native method implementations.
//!
//! Maps (class, method, descriptor) triples to Rust function callbacks
//! that implement the native method behavior.

use std::collections::HashMap;
use std::sync::Arc;

use rustc_hash::FxHashMap;

use rustjvm_types::ClassId;
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::{ArrayElementType, ObjectKind};
use rustjvm_types::{ObjectRef, Value};

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
}

/// WP2.3 — defineClass options carried through `NativeContext::define_class_full`.
///
/// Mirrors `rustjvm_classloading::DefineClassOptions` but without the
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
    /// If `true`, run `<clinit>` on the new class before returning.
    pub initialize: bool,
}

/// Compute a fast hash key for a native method triple.
/// Uses FNV-1a for speed and low collision rate.
#[inline]
fn native_method_hash(class: &str, method: &str, descriptor: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325; // FNV offset basis
    for b in class.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3); // FNV prime
    }
    h ^= b'.' as u64;
    h = h.wrapping_mul(0x100000001b3);
    for b in method.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h ^= b'.' as u64;
    h = h.wrapping_mul(0x100000001b3);
    for b in descriptor.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Trait providing VM capabilities needed by native method implementations.
///
/// The `Vm` struct implements this trait. Using a trait here avoids circular
/// module dependencies between `native` and `vm`.
pub trait NativeContext {
    /// Load a class by name. Returns the ClassId.
    fn load_class(&mut self, name: &str) -> MethodCallResult;

    /// Create a new object of the given class.
    /// Returns an ObjectRef wrapped as `Value::Object(Some(ref))`.
    fn new_object(&mut self, class_name: &str) -> MethodCallResult;

    /// Invoke a method by class name, method name, descriptor, and arguments.
    fn invoke(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult;

    /// Get the identity hash code of an ObjectRef.
    fn identity_hash_code(&self, obj: ObjectRef) -> i32;

    /// Store a value into the test output buffer (for `tempPrint`).
    fn record_printed_value(&mut self, value: Value);

    /// Get the class name for a ClassId.
    fn class_name_of_id(&self, class_id: ClassId) -> Option<String>;

    /// Get the class id of a heap object.
    fn class_id_of_object(&self, obj: ObjectRef) -> ClassId;

    /// Capture the current Java call stack for a throwable's `fillInStackTrace`.
    /// Returns a unique key for later retrieval.
    fn capture_stack_trace(&mut self, throwable_hash: i32) -> Vec<StackTraceEntry>;

    /// Retrieve a previously captured stack trace.
    fn get_stack_trace(&self, throwable_hash: i32) -> Option<&[StackTraceEntry]>;

    // -- Heap access methods (for native method implementations) --

    /// Read an object field by slot index.
    fn get_field(&self, obj: ObjectRef, index: usize) -> Value;

    /// Write an object field by slot index.
    fn set_field(&self, obj: ObjectRef, index: usize, value: Value);

    /// Read an object field by name. Resolves the field name to a slot index
    /// by searching the object's class hierarchy. Returns `Value::Object(None)`
    /// if the field is not found.
    fn get_field_by_name(&self, obj: ObjectRef, field_name: &str) -> Value;

    /// Write an object field by name. Resolves the field name to a slot index
    /// by searching the object's class hierarchy. No-op if the field is not found.
    fn set_field_by_name(&self, obj: ObjectRef, field_name: &str, value: Value);

    /// Resolve a field name to its slot index for a given class.
    /// Returns `None` if the field is not found in the class hierarchy.
    fn resolve_field_index(&self, class_name: &str, field_name: &str) -> Option<usize>;

    /// Check if a method exists in a class (searches the class hierarchy).
    /// Returns `true` if the method is found.
    fn method_exists(&self, class_name: &str, method_name: &str, descriptor: &str) -> bool;

    /// Allocate a primitive array (element_type: Boolean=4..Long=11).
    fn new_array(&mut self, element_type: ArrayElementType, length: usize) -> ObjectRef;

    /// Allocate a reference array for the given component class.
    fn new_ref_array(&mut self, class_id: ClassId, length: usize) -> ObjectRef;

    /// Get the length of an array object.
    fn array_length(&self, obj: ObjectRef) -> usize;

    /// Read an array element by index.
    fn get_array_element(&self, obj: ObjectRef, index: usize) -> Value;

    /// Write an array element by index.
    fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value);

    /// Get the ObjectKind (Object or Array) of a heap object.
    fn heap_kind_of(&self, obj: ObjectRef) -> ObjectKind;

    /// Get the ArrayElementType of an array object.
    /// Returns `ArrayElementType::Reference` for non-array objects or reference arrays.
    fn heap_element_type_of(&self, obj: ObjectRef) -> ArrayElementType;

    /// Create a Java String object from a Rust &str. Returns the ObjectRef.
    fn create_string(&mut self, text: &str) -> ObjectRef;

    /// Read a Java String object back to a Rust String.
    fn read_string(&self, obj: ObjectRef) -> Option<String>;

    /// Get or create the java.lang.Class mirror for the given ClassId.
    fn get_class_mirror(&mut self, class_id: ClassId) -> ObjectRef;

    /// Record a printed line (for System.out.println capture in tests).
    fn record_printed_line(&mut self, text: String);

    /// Get a system stream object (stdout or stderr).
    fn get_system_stream(&self, name: &str) -> Option<ObjectRef>;

    /// Get a system property by key.
    fn get_system_property(&self, key: &str) -> Option<String>;

    /// Snapshot every system property as a `(key, value)` list.  Used by
    /// `System.getProperties()` to materialise a populated Properties
    /// object when real-JDK's `System.props` static field is null.
    fn list_system_properties(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Set a system property. Returns the old value if any.
    fn set_system_property(&mut self, key: &str, value: &str) -> Option<String>;

    /// Allocate an object with the given class_id and number of fields,
    /// without loading a class (for synthetic objects).
    fn alloc_object(&mut self, class_id: ClassId, num_fields: usize) -> ObjectRef;

    /// Ensure a class is loaded and initialized. Returns the ClassId.
    fn ensure_class_initialized(
        &mut self,
        name: &str,
    ) -> Result<ClassId, rustjvm_types::error::MethodCallFailed>;

    /// Check if child_class is a subclass of parent_class.
    fn is_subclass(&self, child: ClassId, parent: ClassId) -> bool;

    /// Get the superclass ClassId. Returns None for java/lang/Object.
    fn superclass_of(&self, class_id: ClassId) -> Option<ClassId>;

    /// Check if a ClassId represents an interface.
    fn is_interface_class(&self, class_id: ClassId) -> bool;

    /// Get the ClassId for a loaded class by name. Returns None if not loaded.
    fn class_id_by_name(&self, name: &str) -> Option<ClassId>;

    /// Get the ClassLoaderId for a loaded class.
    /// Returns 0 = Bootstrap, 1 = Extension, 2 = Application, 3+ = UserDefined(id).
    fn loader_id_of_class(&self, class_id: ClassId) -> i32;

    /// Check if a class is a record (has Record attribute, JEP 395).
    fn is_record_class(&self, class_id: ClassId) -> bool;

    /// Get the record components (name, descriptor) for a record class.
    fn record_components(&self, class_id: ClassId) -> Vec<(String, String)>;

    /// Check if a class is sealed (has PermittedSubclasses attribute, JEP 409).
    fn is_sealed_class(&self, class_id: ClassId) -> bool;

    /// Get the permitted subclass names for a sealed class.
    fn permitted_subclasses(&self, class_id: ClassId) -> Vec<String>;

    /// Get the number of fields (slots) of a heap object.
    fn object_num_fields(&self, obj: ObjectRef) -> usize;

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

    // -- Threading methods --

    /// Get the current thread's ThreadId (as a u64).
    fn thread_id(&self) -> u64;

    /// Acquire the monitor (synchronized) on the given object.
    fn monitor_enter(&mut self, obj: ObjectRef);

    /// Release the monitor (synchronized) on the given object.
    fn monitor_exit(&mut self, obj: ObjectRef);

    /// T1.6.7 — `Thread.holdsLock(Object)`. Returns `true` iff the
    /// current thread currently holds the monitor for `obj`. Default
    /// implementation returns `false` so non-monitor-aware contexts
    /// (mocks, stubs) fall back to the spec-permitted "no" answer.
    fn current_thread_holds_lock(&self, _obj: ObjectRef) -> bool {
        false
    }

    /// Perform Object.wait() on the given object's monitor.
    fn monitor_wait(
        &mut self,
        obj: ObjectRef,
        timeout_ms: Option<u64>,
    ) -> rustjvm_types::error::MethodCallResult;

    /// Perform Object.notify() on the given object's monitor.
    fn monitor_notify(&mut self, obj: ObjectRef) -> rustjvm_types::error::MethodCallResult;

    /// Perform Object.notifyAll() on the given object's monitor.
    fn monitor_notify_all(&mut self, obj: ObjectRef) -> rustjvm_types::error::MethodCallResult;

    /// Spawn a new OS thread to run Thread.run() on the given Java Thread object.
    fn thread_start(&mut self, thread_obj: ObjectRef) -> rustjvm_types::error::MethodCallResult;

    /// T19_K2 — Register a native-spawned OS thread with the VM's
    /// `ThreadRegistry`.
    ///
    /// Used by event-loop schedulers (Vert.x / Netty / XNIO) that spawn
    /// their own carrier OS threads via `std::thread::spawn` rather than
    /// going through `Thread.start0`. Returning the thread ids these
    /// schedulers create through this entry point ensures:
    ///
    /// * the CLI's `wait_for_non_daemon_threads()` waits for them when
    ///   `daemon == false` (otherwise the VM exits as soon as `main()`
    ///   returns even though Quarkus / Keycloak's HTTP listeners are
    ///   still alive),
    /// * GC root scanning sees their stacks (T1.5.1 path),
    /// * JVMTI thread-list APIs see them.
    ///
    /// Parameters:
    /// * `name`     — thread label (shown in `ThreadInfo`, panic logs)
    /// * `daemon`   — `false` for Vert.x / Netty event loops, `true` for
    ///                truly background schedulers (XNIO IO threads, GC
    ///                workers)
    /// * `join_handle_ptr` — opaque `Box<JoinHandle<()>>` raw pointer.
    ///                The VM takes ownership and arranges for it to be
    ///                joined when `wait_for_non_daemon_threads()` runs.
    ///                Pass `0` to register without a join handle (the
    ///                caller is responsible for ensuring the thread
    ///                eventually terminates on its own).
    ///
    /// Returns the registered `ThreadId.0` (a u64) on success. A value
    /// of `0` indicates the call was a no-op (mock context or registry
    /// not available); callers should treat this as "thread spawned but
    /// not VM-tracked" — the OS thread still runs, it just won't keep
    /// the process alive.
    ///
    /// The default implementation is a no-op so mock contexts and
    /// any future trait consumers don't need to implement registry
    /// plumbing. The VM override (`vm/src/vm/vm_exec.rs`) wires it
    /// into `ThreadRegistry::register_with_daemon` + `set_join_handle`.
    fn register_native_thread(
        &mut self,
        _name: &str,
        _daemon: bool,
        _join_handle_ptr: usize,
    ) -> u64 {
        0
    }

    /// T19_K2 — Mark a previously-registered native thread dead.
    ///
    /// Called from a native-spawned OS thread's exit path right before
    /// the OS thread's `JoinHandle` returns. Flips the registry's
    /// `alive` flag for the given id so `is_alive()` returns false and
    /// `alive_non_daemon_thread_ids()` no longer reports it. The
    /// `JoinHandle` is still kept by the registry — `join()` will
    /// observe the dead flag and return immediately.
    ///
    /// Default impl is a no-op.
    fn unregister_native_thread(&mut self, _thread_id: u64) {}

    /// T19_K2 — Attach a `Box<JoinHandle<()>>` raw pointer to an
    /// already-registered native thread.
    ///
    /// Used by event-loop schedulers that need to know the assigned
    /// `ThreadId` BEFORE spawning the OS thread (so the spawned closure
    /// can capture it and use it on exit). The two-phase API is:
    ///
    ///   1. Call `register_native_thread(name, daemon, 0)` to get the
    ///      `ThreadId.0` without an attached handle.
    ///   2. Spawn the OS thread; capture the id in its closure.
    ///   3. Call `attach_join_handle_to_native_thread(id, raw_ptr)` to
    ///      hand the `JoinHandle<()>` over to the registry.
    ///
    /// `join_handle_ptr` follows the same ownership protocol as
    /// `register_native_thread`: a raw `Box<JoinHandle<()>>` pointer
    /// the VM takes back as `Box::from_raw`.
    ///
    /// Returns `true` if the attach succeeded, `false` if `thread_id`
    /// was unknown (in which case the caller MUST reclaim the
    /// `Box<JoinHandle<()>>` via `Box::from_raw` or it leaks the OS
    /// thread).
    fn attach_join_handle_to_native_thread(
        &mut self,
        _thread_id: u64,
        _join_handle_ptr: usize,
    ) -> bool {
        false
    }

    /// T19_K4 — Attach a `java.lang.Thread` mirror to an
    /// already-registered native thread.
    ///
    /// Used by event-loop schedulers (Vert.x / Netty / XNIO) that
    /// register their carrier OS thread via
    /// [`Self::register_native_thread`] but also need a real
    /// `java.lang.Thread` mirror so:
    ///
    ///  * `Thread.currentThread()` resolves to the right object when
    ///    Java-side code runs on the event-loop carrier (e.g. a
    ///    Runnable that consults the thread name),
    ///  * `ThreadRegistry::find_thread_id_by_thread_obj` works
    ///    cross-thread (so other code that has the mirror handle can
    ///    locate the `ThreadId`),
    ///  * the carrier thread shows up in
    ///    `ThreadRegistry::alive_thread_objects()` (and therefore in
    ///    `Thread.enumerate()` / JVMTI thread-list listings).
    ///
    /// `thread_id` must be a value previously returned by
    /// [`Self::register_native_thread`] / the two-phase
    /// [`Self::attach_join_handle_to_native_thread`] flow. The
    /// `java_thread_obj` should be a freshly-allocated synthetic
    /// `java.lang.Thread` mirror with `name` set on slot 0 and
    /// (optionally) `tid` on slot 2 — `register_native_thread`
    /// already stamped the registry entry, this call just attaches
    /// the mirror so look-ups by `ObjectRef` succeed.
    ///
    /// Returns `true` if the thread id was found and the mirror was
    /// stored, `false` if the id was unknown. Default impl is a
    /// no-op so mock contexts don't need to model a heap.
    fn set_native_thread_java_obj(
        &mut self,
        _thread_id: u64,
        _java_thread_obj: ObjectRef,
    ) -> bool {
        false
    }

    /// Block until the target thread (identified by Java Thread object) finishes.
    fn thread_join(&mut self, thread_obj: ObjectRef) -> rustjvm_types::error::MethodCallResult;

    /// Check if the target thread (identified by Java Thread object) is alive.
    fn thread_is_alive(&self, thread_obj: ObjectRef) -> bool;

    /// Get the Java Thread object for the current thread.
    fn current_thread_object(&mut self) -> ObjectRef;

    /// Interrupt the target thread (identified by Java Thread object).
    fn thread_interrupt(&mut self, thread_obj: ObjectRef);

    /// T1.5.1 — post an asynchronous `Throwable` to the target
    /// thread (identified by its Java `Thread` object). The target
    /// will raise the exception at its next safepoint.
    ///
    /// Returns `true` if the post succeeded (target found, slot
    /// written), `false` if the target is not alive or not
    /// registered. Default impl is a no-op for mock contexts.
    fn thread_post_async_exception(
        &mut self,
        _thread_obj: ObjectRef,
        _throwable: ObjectRef,
    ) -> bool {
        false
    }

    /// Check and optionally clear the current thread's interrupted status.
    fn is_interrupted(&self, clear: bool) -> bool;

    // -- Virtual-thread / Loom (JEP 444/491) --
    //
    // Default implementations make these no-ops so platform native code (and
    // test mocks) don't need to implement them. The VM overrides them in
    // `vm_exec.rs` to drive the carrier-thread semaphore and pin tracking.

    /// Returns `true` if the current thread is a virtual thread.
    fn is_current_virtual(&self) -> bool {
        false
    }

    /// Return the current thread's pin depth (0 = not pinned).
    fn vt_pin_count(&self) -> u32 {
        0
    }

    /// Increment the current thread's pin count with the given reason.
    /// Called from `monitor_enter` / JNI entry. No-op for platform threads.
    fn vt_pin(&mut self, _reason: &'static str) {}

    /// Decrement the current thread's pin count.
    /// No-op for platform threads or when pin_count is already zero.
    fn vt_unpin(&mut self) {}

    /// Release the carrier-thread permit so another virtual thread can run.
    /// Called before a blocking syscall (sleep, park, NIO wait) in a VT.
    /// No-op for platform threads.
    fn vt_release_carrier(&mut self) {}

    /// Reacquire a carrier-thread permit after a blocking operation completes.
    /// Must be paired with `vt_release_carrier`. No-op for platform threads.
    fn vt_acquire_carrier(&mut self) {}

    /// Emit a `jdk.VirtualThreadPinned` JFR event for the current thread.
    /// Called when a pinned virtual thread is about to block its carrier.
    fn emit_virtual_thread_pinned_jfr(&mut self, _reason: &str) {}

    /// Get the number of alive threads in the VM.
    fn active_thread_count(&self) -> i32;

    /// Get the Java Thread objects for all alive threads (up to `max` entries).
    /// Returns the number of thread objects written.
    fn enumerate_threads(&self, max: usize) -> Vec<ObjectRef>;

    // -- VM stats methods (for JMX) --

    /// Returns the total number of bytes allocated on the heap.
    fn heap_allocated_bytes(&self) -> usize;

    /// Returns the number of classes currently loaded in the VM.
    fn loaded_class_count(&self) -> usize;

    /// Returns the cumulative number of GC collections that have occurred.
    fn gc_collection_count(&self) -> u64;

    /// Force a garbage collection cycle and run pending finalizers.
    /// Used by `System.gc()` / `Runtime.gc()`.
    fn force_gc(&mut self);

    // -- Reflection metadata methods --

    /// Get metadata for all fields declared in this class (not inherited).
    fn declared_fields(&self, class_id: ClassId) -> Vec<FieldMetadata>;

    /// Get metadata for all methods declared in this class (not inherited).
    fn declared_methods(&self, class_id: ClassId) -> Vec<MethodMetadata>;

    /// Get the ClassIds of directly implemented/extended interfaces.
    fn class_interfaces(&self, class_id: ClassId) -> Vec<ClassId>;

    /// Get the raw access_flags bits for a class.
    fn class_access_flags(&self, class_id: ClassId) -> u16;

    /// Read a static field value by class and field index.
    fn get_static_field(&self, class_id: ClassId, field_index: usize) -> Value;

    /// Write a static field value by class and field index.
    fn set_static_field(&mut self, class_id: ClassId, field_index: usize, value: Value);

    /// Write a static field by class name and field name.
    /// Resolves the class and finds the static field index by name.
    /// No-op if the class or field cannot be found.
    fn set_static_field_by_name(&mut self, class_name: &str, field_name: &str, value: Value) {
        if let Some(class_id) = self.class_id_by_name(class_name) {
            if let Some(idx) = self.static_field_index_by_name(class_id, field_name) {
                self.set_static_field(class_id, idx, value);
            }
        }
    }

    /// Find the static field index for a field by name.  Returns `None` if the
    /// class doesn't have a static field with that name.
    fn static_field_index_by_name(&self, class_id: ClassId, field_name: &str) -> Option<usize> {
        let _ = (class_id, field_name);
        None
    }

    /// Get or create a Class mirror for a primitive type (e.g. "int", "boolean").
    fn primitive_class_mirror(&mut self, name: &str) -> ObjectRef;

    /// Get the file descriptor table for I/O operations.
    fn fd_table(&self) -> &crate::fd_table::FileDescriptorTable;

    // -- ObjectStreamClass descriptor cache (WP0.2) --
    //
    // Backs `java.io.ObjectStreamClass.lookup(Class)`. See
    // `vm::runtime::serialization::oscache` for the rationale.

    /// Look up a previously-built `ObjectStreamClass` descriptor for
    /// `class_id`. Returns `None` if `lookup` has never been called for
    /// this class yet (the native then allocates a fresh descriptor and
    /// installs it via `osc_cache_put`).
    ///
    /// Default implementation returns `None` — mock contexts and other
    /// simple impls just never cache.
    fn osc_cache_get(&self, _class_id: ClassId) -> Option<ObjectRef> {
        None
    }

    /// Install a freshly-built `ObjectStreamClass` descriptor in the
    /// cache. Returns the `ObjectRef` that ends up cached (either
    /// `desc` on a successful insert, or the pre-existing entry if
    /// another thread raced us). Callers MUST use the returned ref as
    /// the result of `lookup` — discarding it would break the
    /// identity contract.
    ///
    /// Default implementation ignores the descriptor and returns `desc`
    /// unchanged — non-caching contexts behave as if every lookup
    /// builds a fresh descriptor.
    fn osc_cache_put(&self, _class_id: ClassId, desc: ObjectRef) -> ObjectRef {
        desc
    }

    // -- Volatile field access (for sun.misc.Unsafe / Atomics) --

    /// Read an object field with volatile (sequentially consistent) semantics.
    fn get_field_volatile(&self, obj: ObjectRef, index: usize) -> Value;

    /// Write an object field with volatile (sequentially consistent) semantics.
    fn set_field_volatile(&self, obj: ObjectRef, index: usize, value: Value);

    // -- Compare-and-swap --

    /// Compare-and-swap on an object field. Returns true if field contained
    /// `expected` and was updated to `new_val`.
    fn compare_and_swap_field(
        &mut self,
        obj: ObjectRef,
        index: usize,
        expected: Value,
        new_val: Value,
    ) -> bool;

    // -- Park/Unpark (LockSupport) --

    /// Park the current thread (block until unparked or timeout).
    fn park(&mut self, timeout: Option<std::time::Duration>);

    /// Unpark a thread identified by its Java Thread object.
    fn unpark(&self, thread_obj: ObjectRef);

    // -- Object allocation without constructor --

    /// Allocate an uninitialized object instance (for Unsafe.allocateInstance).
    /// Returns None if the class cannot be loaded.
    fn allocate_instance(&mut self, class_name: &str) -> Option<ObjectRef>;

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

    // -- Scoped Values (JEP 446, Java 25) --

    /// Look up a scoped value binding by key_id on the current thread's stack.
    fn get_scoped_value(&self, key_id: u64) -> Option<Value>;

    /// Push a scoped value binding onto the current thread's stack.
    fn push_scoped_value(&mut self, key_id: u64, value: Value);

    /// Pop the most recent scoped value binding from the current thread's stack.
    fn pop_scoped_value(&mut self);

    /// Return the current depth (number of entries) of the scoped value binding stack.
    fn scoped_value_depth(&self) -> usize;

    // -- Panama FFI (JEP 454, Java 25) --

    /// Allocate off-heap memory. Returns (alloc_id, raw_pointer) or None on failure.
    fn allocate_native_memory(&mut self, size: usize, align: usize) -> Option<(i64, *mut u8)>;

    /// Free off-heap memory by allocation ID.
    fn free_native_memory(&mut self, alloc_id: i64);

    /// Load a native library. Returns library index or error.
    fn load_native_library(&mut self, path: &str) -> Result<i64, rustjvm_types::error::MethodCallFailed>;

    /// Find a symbol in a loaded library. Returns the symbol address.
    /// lib_index -1 means search the default/system library.
    fn find_native_symbol(&self, lib_index: i64, name: &str) -> Option<usize>;

    /// Register an upcall entry (Java callback for C). Returns the slot index.
    fn register_upcall(&mut self, entry: crate::ffi::UpcallEntry) -> usize;

    /// Get upcall info by slot index. Returns (target, param_kinds, return_kind).
    fn get_upcall_info(&self, slot: usize) -> Option<(ObjectRef, Vec<i32>, i32)>;

    // -- JPMS Module support (N3) --

    /// Return the module name (JPMS) of the class with the given ClassId.
    ///
    /// Returns `None` for classes in the unnamed module.
    fn module_name_of_class(&self, class_id: ClassId) -> Option<String>;

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

    /// Return a URL string (e.g. `file:/...` or `jar:file:/...!/...`) for
    /// every classpath entry that contains a resource with the given name.
    /// Used by `ClassLoader.getResources` / `getSystemResources`.
    /// Default implementation returns an empty vector so test mocks compile.
    fn find_all_resource_urls(&self, name: &str) -> Vec<String> {
        let _ = name;
        Vec::new()
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

    /// Define a new class from raw bytecode (Phase 23.2).
    ///
    /// Parses the class bytes, registers the class with the ClassManager under
    /// the application loader, and returns the ClassId of the newly defined class.
    /// Returns `None` if parsing fails.
    fn define_class_from_bytes(
        &mut self,
        name: &str,
        bytes: &[u8],
    ) -> Option<ClassId>;

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
    /// `loader_id == 0` means use the application loader; non-zero
    /// values are user-defined loader namespaces.
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

    /// WP2.4 — redefine the bytecode of an already-loaded class.
    ///
    /// Used by `Instrumentation.redefineClasses` /
    /// `retransformClasses`. The class must already exist; otherwise
    /// returns `Err("class not loaded")`. On success, the JIT cache
    /// for the old class is invalidated and any new method lookups
    /// resolve through the new bytecode.
    fn redefine_class(
        &mut self,
        class_id: ClassId,
        new_bytes: &[u8],
    ) -> Result<(), String> {
        let _ = (class_id, new_bytes);
        Err("redefine_class not implemented".to_string())
    }

    /// WP2.4 — list all loaded classes.
    fn list_loaded_class_ids(&self) -> Vec<ClassId> {
        Vec::new()
    }

    /// WP2.4 — list all classes whose initiating loader was the
    /// application loader (or, if `loader_id != 0`, the user-defined
    /// loader with that id).
    fn list_initiated_class_ids(&self, _loader_id: u32) -> Vec<ClassId> {
        Vec::new()
    }

    /// Look up a class by name within a specific user-defined loader's namespace.
    /// Falls back to the standard delegation chain if not found.
    fn class_id_by_name_and_loader(&self, name: &str, loader_id: u32) -> Option<ClassId>;

    /// Allocate a unique classloader ID for a new user-defined classloader instance.
    fn allocate_loader_id(&mut self) -> u32;

    /// Register a discovered weak/soft/phantom reference with the GC's ReferenceProcessor.
    /// `ref_type`: 0=Weak, 1=Soft, 2=Phantom
    /// `reference_obj`: the Reference object itself
    /// `referent`: the referred-to object
    /// `queue`: optional ReferenceQueue object
    fn discover_reference(
        &mut self,
        ref_type: u8,
        reference_obj: ObjectRef,
        referent: ObjectRef,
        queue: Option<ObjectRef>,
    );

    /// Record a JFR thread sleep event. Called by Thread.sleep implementations.
    /// Default is no-op; the VM overrides this with the real JFR recorder.
    fn record_thread_sleep(&mut self, _sleep_nanos: i64, _actual_duration_nanos: u64) {}

    /// Record a JFR file read event.
    fn record_file_read(&mut self, _fd: i32, _bytes_read: i64, _eof: bool, _duration_nanos: u64) {}

    /// Record a JFR file write event.
    fn record_file_write(&mut self, _fd: i32, _bytes_written: i64, _duration_nanos: u64) {}

    // -- JPMS module queries (Phase B) --

    /// Check if module `reader` reads module `provider`.
    fn reads_module(&self, reader: &str, provider: &str) -> bool {
        // Default: all modules can read each other (classpath-only mode).
        let _ = (reader, provider);
        true
    }

    /// Check if `module_name` exports `pkg` unconditionally (to all modules).
    fn is_package_exported_unqualified(&self, module_name: &str, pkg: &str) -> bool {
        let _ = (module_name, pkg);
        true
    }

    /// Check if `module_name` exports `pkg` to `to_module`.
    fn is_package_exported_to(&self, module_name: &str, pkg: &str, to_module: &str) -> bool {
        let _ = (module_name, pkg, to_module);
        true
    }

    /// Check if `module_name` opens `pkg` unconditionally.
    fn is_package_open_unqualified(&self, module_name: &str, pkg: &str) -> bool {
        let _ = (module_name, pkg);
        true
    }

    /// Check if `module_name` opens `pkg` to `to_module`.
    fn is_package_open_to(&self, module_name: &str, pkg: &str, to_module: &str) -> bool {
        let _ = (module_name, pkg, to_module);
        true
    }

    /// Add a dynamic read edge: `reader` reads `provider`.
    fn module_add_reads(&mut self, reader: &str, provider: &str) {
        let _ = (reader, provider);
    }

    /// Add a dynamic export: `module_name` exports `pkg` to `target`.
    fn module_add_exports(&mut self, module_name: &str, pkg: &str, target: &str) {
        let _ = (module_name, pkg, target);
    }

    /// Add a dynamic open: `module_name` opens `pkg` to `target`.
    fn module_add_opens(&mut self, module_name: &str, pkg: &str, target: &str) {
        let _ = (module_name, pkg, target);
    }

    /// Return all packages owned by `module_name`.
    fn module_packages(&self, module_name: &str) -> Vec<String> {
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
    /// `defineHiddenClass` when the `initialize` flag is `true`. The
    /// default is a no-op — callers that care about deterministic init
    /// must override this in their NativeContext impl.
    fn initialize_class(&mut self, class_id: ClassId) -> Result<(), String> {
        let _ = class_id;
        Ok(())
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
    fn check_deep_reflection_access(
        &self,
        _accessor_class_id: ClassId,
        _target_class_id: ClassId,
    ) -> Result<(), String> {
        // Default: allow (classpath-only mode, or VM without JPMS configured).
        Ok(())
    }

    // -- T13 java/lang/Class reflection metadata --

    /// Get the class file version (major number) for a class.
    fn class_file_version(&self, _class_id: ClassId) -> u16 {
        65 // Default: Java 21
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

/// Annotation data extracted from class file attributes.
#[derive(Debug, Clone)]
pub struct AnnotationData {
    /// The annotation type descriptor (e.g. "Ljava/lang/Override;")
    pub type_descriptor: String,
    /// Element-value pairs: (name, value_representation)
    pub elements: Vec<(String, AnnotationElementValue)>,
}

/// A simplified representation of an annotation element value.
#[derive(Debug, Clone)]
pub enum AnnotationElementValue {
    /// A constant int/byte/char/short/boolean value.
    Int(i32),
    /// A constant long value.
    Long(i64),
    /// A constant float value.
    Float(f32),
    /// A constant double value.
    Double(f64),
    /// A string value.
    StringVal(String),
    /// An enum constant: (type_descriptor, const_name).
    Enum(String, String),
    /// A class literal: descriptor string.
    Class(String),
    /// A nested annotation.
    Annotation(AnnotationData),
    /// An array of values.
    Array(Vec<AnnotationElementValue>),
}

/// An entry in a captured Java stack trace.
///
/// WP1.9: `byte_code_index` (-1 when unknown, e.g. for native frames or
/// synthetic bootstrap frames) is populated from each frame's `last_instr_pc`
/// when available and used by `java.lang.StackWalker.StackFrame.getByteCodeIndex()`.
#[derive(Debug, Clone)]
pub struct StackTraceEntry {
    pub class_name: Arc<str>,
    pub method_name: Arc<str>,
    pub source_file: Option<Arc<str>>,
    pub line_number: i32, // -1 for unknown, -2 for native methods
    /// Bytecode index of the last-executed instruction in the frame's method.
    /// `-1` for unknown / native. Used by `StackFrame.getByteCodeIndex()`.
    pub byte_code_index: i32,
}

/// Callback signature for native method implementations.
///
/// # Arguments
/// - `ctx` — mutable reference to the VM context (implements `NativeContext`)
/// - `args` — method arguments: for instance methods, `args[0]` is the receiver (`this`)
///
/// # Returns
/// - `Ok(Some(value))` — method returned a value
/// - `Ok(None)` — method returned void
/// - `Err(MethodCallFailed)` — method threw an exception or had an internal error
pub type NativeCallback = fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult;

/// Registry of native method implementations.
///
/// Maps (class, method, descriptor) triples to Rust function callbacks.
/// Uses pre-computed FNV-1a hash keys for zero-allocation lookups.
/// T10.9.B: FxHashMap — keys are FNV-1a hashes of internal class/method/desc triples.
pub struct NativeMethodRegistry {
    methods: FxHashMap<u64, NativeCallback>,
    /// Store the triples so we can detect hash collisions.
    /// With 3000+ registrations, FNV-1a u64 collision probability is non-trivial.
    keys: FxHashMap<u64, String>,
}

impl NativeMethodRegistry {
    pub fn new() -> Self {
        Self {
            methods: FxHashMap::default(),
            keys: FxHashMap::default(),
        }
    }

    /// Register a native method implementation.
    pub fn register(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        callback: NativeCallback,
    ) {
        let key = native_method_hash(class_name, method_name, descriptor);
        let triple = format!("{class_name}.{method_name}{descriptor}");
        if let Some(existing) = self.keys.get(&key) {
            if *existing != triple {
                // Hash collision is a fatal bug — the hash function must be
                // collision-free for correctness.  Panic is deliberate.
                panic!(
                    "NativeMethodRegistry hash collision!\n  existing: {existing}\n  new: {triple}\n  hash: {key:#018x}"
                );
            }
        }
        self.keys.insert(key, triple);
        self.methods.insert(key, callback);
    }

    /// Look up a native method implementation (zero allocation).
    #[inline]
    pub fn find(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeCallback> {
        let key = native_method_hash(class_name, method_name, descriptor);
        self.methods.get(&key).copied()
    }

    /// NEW-14: copy every native method currently registered under
    /// `from_class` to also be reachable under `to_class`.
    ///
    /// Used by the JDBC registration path to make every
    /// `PreparedStatement` method also dispatch when invoked on a
    /// `CallableStatement` instance (the JDK's `CallableStatement`
    /// interface extends `PreparedStatement`, so every PS method is
    /// valid on a CS instance — but native dispatch is keyed by class
    /// name, not Java inheritance, so we have to populate both keys
    /// explicitly). The function:
    ///
    ///   1. Walks `keys` (the `hash → "class.method descriptor"`
    ///      reverse map) to find every entry whose class prefix
    ///      matches `from_class`.
    ///   2. For each match, re-registers the same callback under
    ///      `to_class` with the same method name and descriptor.
    ///
    /// Idempotent: calling twice produces the same final state. Any
    /// existing registration on `to_class` is overwritten (matching
    /// the behavior of `register` itself, which re-registers silently
    /// when the triple is identical).
    pub fn alias_class(&mut self, from_class: &str, to_class: &str) {
        // Collect first to avoid mutating while iterating.
        let prefix = format!("{from_class}.");
        let entries: Vec<(String, String, NativeCallback)> = self
            .keys
            .iter()
            .filter_map(|(hash, triple)| {
                if !triple.starts_with(&prefix) {
                    return None;
                }
                let after = &triple[prefix.len()..];
                // Split `methodName(descriptor)` — the method name
                // ends at the first `(`.
                let paren = after.find('(')?;
                let method_name = &after[..paren];
                let descriptor = &after[paren..];
                let callback = *self.methods.get(hash)?;
                Some((
                    method_name.to_string(),
                    descriptor.to_string(),
                    callback,
                ))
            })
            .collect();
        for (method_name, descriptor, callback) in entries {
            self.register(to_class, &method_name, &descriptor, callback);
        }
    }

    /// The number of registered native methods.
    pub fn len(&self) -> usize {
        self.methods.len()
    }

    /// Returns true if no native methods are registered.
    pub fn is_empty(&self) -> bool {
        self.methods.is_empty()
    }

}

impl Default for NativeMethodRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for NativeMethodRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeMethodRegistry")
            .field("count", &self.methods.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_native(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        Ok(None)
    }

    fn dummy_native_2(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        Ok(Some(Value::Int(42)))
    }

    // -----------------------------------------------------------------------
    // NativeMethodRegistry basics
    // -----------------------------------------------------------------------

    #[test]
    fn register_and_find() {
        let mut registry = NativeMethodRegistry::new();
        registry.register("java/lang/Object", "registerNatives", "()V", dummy_native);
        assert!(registry
            .find("java/lang/Object", "registerNatives", "()V")
            .is_some());
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn find_missing_returns_none() {
        let registry = NativeMethodRegistry::new();
        assert!(registry
            .find("java/lang/Object", "hashCode", "()I")
            .is_none());
        assert!(registry.is_empty());
    }

    #[test]
    fn new_registry_is_empty() {
        let registry = NativeMethodRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn default_registry_is_empty() {
        let registry = NativeMethodRegistry::default();
        assert!(registry.is_empty());
    }

    #[test]
    fn register_multiple_methods() {
        let mut registry = NativeMethodRegistry::new();
        registry.register("java/lang/Object", "registerNatives", "()V", dummy_native);
        registry.register("java/lang/Object", "hashCode", "()I", dummy_native_2);
        registry.register("java/lang/System", "currentTimeMillis", "()J", dummy_native);
        assert_eq!(registry.len(), 3);
        assert!(!registry.is_empty());
    }

    #[test]
    fn find_distinguishes_by_class() {
        let mut registry = NativeMethodRegistry::new();
        registry.register("java/lang/Object", "toString", "()Ljava/lang/String;", dummy_native);
        registry.register("java/lang/String", "toString", "()Ljava/lang/String;", dummy_native_2);
        assert!(registry.find("java/lang/Object", "toString", "()Ljava/lang/String;").is_some());
        assert!(registry.find("java/lang/String", "toString", "()Ljava/lang/String;").is_some());
        // Different class, same method + descriptor
        assert!(registry.find("java/lang/Integer", "toString", "()Ljava/lang/String;").is_none());
    }

    #[test]
    fn find_distinguishes_by_descriptor() {
        let mut registry = NativeMethodRegistry::new();
        registry.register("java/lang/Math", "abs", "(I)I", dummy_native);
        registry.register("java/lang/Math", "abs", "(J)J", dummy_native_2);
        assert!(registry.find("java/lang/Math", "abs", "(I)I").is_some());
        assert!(registry.find("java/lang/Math", "abs", "(J)J").is_some());
        assert!(registry.find("java/lang/Math", "abs", "(D)D").is_none());
    }

    #[test]
    fn overwrite_same_triple() {
        let mut registry = NativeMethodRegistry::new();
        registry.register("A", "b", "()V", dummy_native);
        // Re-registering the same triple should overwrite without panic
        registry.register("A", "b", "()V", dummy_native_2);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn debug_format_shows_count() {
        let mut registry = NativeMethodRegistry::new();
        registry.register("A", "b", "()V", dummy_native);
        let dbg = format!("{:?}", registry);
        assert!(dbg.contains("count: 1"));
    }

    // -----------------------------------------------------------------------
    // FNV hash function tests
    // -----------------------------------------------------------------------

    #[test]
    fn hash_deterministic() {
        let h1 = native_method_hash("java/lang/Object", "hashCode", "()I");
        let h2 = native_method_hash("java/lang/Object", "hashCode", "()I");
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_differs_for_different_inputs() {
        let h1 = native_method_hash("java/lang/Object", "hashCode", "()I");
        let h2 = native_method_hash("java/lang/Object", "toString", "()Ljava/lang/String;");
        let h3 = native_method_hash("java/lang/String", "hashCode", "()I");
        assert_ne!(h1, h2);
        assert_ne!(h1, h3);
    }

    #[test]
    fn hash_differs_for_swapped_components() {
        // "A.B.()V" vs "B.A.()V" — the separator dots are included in hashing
        let h1 = native_method_hash("A", "B", "()V");
        let h2 = native_method_hash("B", "A", "()V");
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_empty_strings() {
        // Edge case: empty strings should not panic
        let h = native_method_hash("", "", "");
        assert!(h != 0); // FNV offset basis with just dots
    }

    #[test]
    #[should_panic(expected = "hash collision")]
    fn hash_collision_panics() {
        // We can't easily force a real collision, so we test the detection path
        // by using internal knowledge: insert two different triples with the same hash.
        // Since we can't easily find a collision, we just verify the panic message
        // by creating a registry and manually inserting a conflicting key.
        let mut registry = NativeMethodRegistry::new();
        registry.register("A", "b", "()V", dummy_native);
        // Manually force a collision by inserting a different triple with same hash
        let key = native_method_hash("A", "b", "()V");
        // Overwrite the keys map entry to simulate a collision
        registry.keys.insert(key, "X.y()Z".to_string());
        // Now registering the original triple again will see a mismatch
        registry.register("A", "b", "()V", dummy_native);
    }

    // -----------------------------------------------------------------------
    // AnnotationData / AnnotationElementValue
    // -----------------------------------------------------------------------

    #[test]
    fn annotation_data_clone_and_debug() {
        let ann = AnnotationData {
            type_descriptor: "Ljava/lang/Override;".to_string(),
            elements: vec![
                ("value".to_string(), AnnotationElementValue::Int(42)),
                ("name".to_string(), AnnotationElementValue::StringVal("test".to_string())),
            ],
        };
        let cloned = ann.clone();
        assert_eq!(cloned.type_descriptor, "Ljava/lang/Override;");
        assert_eq!(cloned.elements.len(), 2);
        // Debug should not panic
        let _ = format!("{:?}", ann);
    }

    #[test]
    fn annotation_element_value_variants() {
        let values: Vec<AnnotationElementValue> = vec![
            AnnotationElementValue::Int(1),
            AnnotationElementValue::Long(2),
            AnnotationElementValue::Float(3.0),
            AnnotationElementValue::Double(4.0),
            AnnotationElementValue::StringVal("s".to_string()),
            AnnotationElementValue::Enum("Lp;".to_string(), "A".to_string()),
            AnnotationElementValue::Class("Lc;".to_string()),
            AnnotationElementValue::Annotation(AnnotationData {
                type_descriptor: "Linner;".to_string(),
                elements: vec![],
            }),
            AnnotationElementValue::Array(vec![AnnotationElementValue::Int(10)]),
        ];
        // All variants should be clonable and debuggable
        for v in &values {
            let _ = v.clone();
            let _ = format!("{:?}", v);
        }
        assert_eq!(values.len(), 9);
    }

    // -----------------------------------------------------------------------
    // StackTraceEntry
    // -----------------------------------------------------------------------

    #[test]
    fn stack_trace_entry_clone_and_debug() {
        let entry = StackTraceEntry {
            class_name: Arc::from("java/lang/Object"),
            method_name: Arc::from("hashCode"),
            source_file: Some(Arc::from("Object.java")),
            line_number: 42,
            byte_code_index: 17,
        };
        let cloned = entry.clone();
        assert_eq!(&*cloned.class_name, "java/lang/Object");
        assert_eq!(cloned.line_number, 42);
        assert_eq!(cloned.byte_code_index, 17);
        let _ = format!("{:?}", entry);
    }

    #[test]
    fn stack_trace_entry_native_method() {
        let entry = StackTraceEntry {
            class_name: Arc::from("java/lang/System"),
            method_name: Arc::from("arraycopy"),
            source_file: None,
            line_number: -2, // native method
            byte_code_index: -1,
        };
        assert_eq!(entry.line_number, -2);
        assert!(entry.source_file.is_none());
        assert_eq!(entry.byte_code_index, -1);
    }
}
