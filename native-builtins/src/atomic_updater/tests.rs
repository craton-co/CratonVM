use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Build a Class mirror in the mock and return both the mirror ref
/// and the underlying ClassId for assertion convenience.
fn make_class_mirror(ctx: &mut MockNativeContext, name: &str) -> (ObjectRef, ClassId) {
    let cid = ctx.ensure_class_initialized(name).unwrap();
    let mirror = ctx.get_class_mirror(cid);
    (mirror, cid)
}

/// The mock's `declared_fields` returns empty; tests that need
/// FieldMetadata install a custom override via the mock's hooks.
/// Since the mock doesn't currently expose declared_fields override,
/// we wrap by sub-classing in a dedicated sub-mock for these tests.
/// To keep the diff minimal, the tests use a bypass mock.
use cratonvm_native_api::FieldMetadata;
use std::cell::UnsafeCell;
use std::collections::HashMap;

/// Test wrapper that supplements the standard MockNativeContext with
/// per-class FieldMetadata so the updater natives can find a real
/// field at a deterministic slot.
struct UpdaterMock {
    inner: MockNativeContext,
    field_table: UnsafeCell<HashMap<u32, Vec<(String, String, u16, usize, bool)>>>,
    /// r21 review item 4: `class_id_by_name_and_loader` answers, by name
    /// (any loader), before the inner mock's `None`. Since r21 wave 7
    /// (lane updaters) also the declaring class's resolver view
    /// (`class_id_seen_by_class_without_loading`): a name absent here is
    /// one the resolver does not know without loading.
    by_loader: UnsafeCell<HashMap<String, ClassId>>,
    /// r21 wave 7, lane updaters: every class `new_object` allocated, in
    /// order, so a test can see which throwable a refusal built.
    allocated: UnsafeCell<Vec<String>>,
}

impl UpdaterMock {
    fn new() -> Self {
        Self {
            inner: mock_ctx(),
            field_table: UnsafeCell::new(HashMap::new()),
            by_loader: UnsafeCell::new(HashMap::new()),
            allocated: UnsafeCell::new(Vec::new()),
        }
    }

    fn allocated(&self) -> Vec<String> {
        // SAFETY: single-threaded test code.
        unsafe { &*self.allocated.get() }.clone()
    }

    fn add_field(
        &self,
        cid: ClassId,
        name: &str,
        descriptor: &str,
        access_flags: u16,
        slot_index: usize,
        is_static: bool,
    ) {
        // SAFETY: single-threaded test code.
        let table = unsafe { &mut *self.field_table.get() };
        table.entry(cid.as_u32()).or_default().push((
            name.to_string(),
            descriptor.to_string(),
            access_flags,
            slot_index,
            is_static,
        ));
    }
}

// Forward every NativeContext method to `inner` except declared_fields.
impl cratonvm_native_api::NativeClassAccess for UpdaterMock {
    fn load_class(&mut self, name: &str) -> MethodCallResult {
        self.inner.load_class(name)
    }
    fn class_name_of_id(&self, class_id: ClassId) -> Option<String> {
        self.inner.class_name_of_id(class_id)
    }
    fn class_id_of_object(&self, obj: ObjectRef) -> ClassId {
        self.inner.class_id_of_object(obj)
    }
    fn method_exists(&self, c: &str, m: &str, d: &str) -> bool {
        self.inner.method_exists(c, m, d)
    }
    fn ensure_class_initialized(&mut self, name: &str) -> Result<ClassId, MethodCallFailed> {
        self.inner.ensure_class_initialized(name)
    }
    fn is_subclass(&self, c: ClassId, p: ClassId) -> bool {
        self.inner.is_subclass(c, p)
    }
    fn superclass_of(&self, class_id: ClassId) -> Option<ClassId> {
        self.inner.superclass_of(class_id)
    }
    fn class_id_by_name(&self, name: &str) -> Option<ClassId> {
        self.inner.class_id_by_name(name)
    }
    fn loader_id_of_class(&self, class_id: ClassId) -> i32 {
        self.inner.loader_id_of_class(class_id)
    }
    fn is_record_class(&self, class_id: ClassId) -> bool {
        self.inner.is_record_class(class_id)
    }
    fn record_components(&self, class_id: ClassId) -> Vec<(String, String)> {
        self.inner.record_components(class_id)
    }
    fn is_sealed_class(&self, class_id: ClassId) -> bool {
        self.inner.is_sealed_class(class_id)
    }
    fn permitted_subclasses(&self, class_id: ClassId) -> Vec<String> {
        self.inner.permitted_subclasses(class_id)
    }
    fn declared_fields(&self, class_id: ClassId) -> Vec<FieldMetadata> {
        // SAFETY: single-threaded test code.
        let table = unsafe { &*self.field_table.get() };
        match table.get(&class_id.as_u32()) {
            Some(entries) => entries
                .iter()
                .map(|(name, desc, flags, slot, is_static)| FieldMetadata {
                    name: name.clone(),
                    descriptor: desc.clone(),
                    access_flags: *flags,
                    slot_index: *slot,
                    declaring_class_id: class_id,
                    is_static: *is_static,
                })
                .collect(),
            None => Vec::new(),
        }
    }
    fn declared_methods(&self, class_id: ClassId) -> Vec<cratonvm_native_api::MethodMetadata> {
        self.inner.declared_methods(class_id)
    }
    fn class_interfaces(&self, class_id: ClassId) -> Vec<ClassId> {
        self.inner.class_interfaces(class_id)
    }
    fn class_access_flags(&self, class_id: ClassId) -> u16 {
        self.inner.class_access_flags(class_id)
    }
    fn primitive_class_mirror(&mut self, name: &str) -> ObjectRef {
        self.inner.primitive_class_mirror(name)
    }
    fn class_annotations(&self, class_id: ClassId) -> Vec<cratonvm_native_api::AnnotationData> {
        self.inner.class_annotations(class_id)
    }
    fn method_annotations(
        &self,
        class_id: ClassId,
        m: &str,
        d: &str,
    ) -> Vec<cratonvm_native_api::AnnotationData> {
        self.inner.method_annotations(class_id, m, d)
    }
    fn field_annotations(
        &self,
        class_id: ClassId,
        f: &str,
    ) -> Vec<cratonvm_native_api::AnnotationData> {
        self.inner.field_annotations(class_id, f)
    }
    fn method_parameter_annotations(
        &self,
        class_id: ClassId,
        m: &str,
        d: &str,
    ) -> Vec<Vec<cratonvm_native_api::AnnotationData>> {
        self.inner.method_parameter_annotations(class_id, m, d)
    }
    fn class_signature(&self, class_id: ClassId) -> Option<String> {
        self.inner.class_signature(class_id)
    }
    fn method_signature(&self, class_id: ClassId, m: &str, d: &str) -> Option<String> {
        self.inner.method_signature(class_id, m, d)
    }
    fn field_signature(&self, class_id: ClassId, f: &str) -> Option<String> {
        self.inner.field_signature(class_id, f)
    }
    fn method_annotation_default(
        &self,
        class_id: ClassId,
        m: &str,
        d: &str,
    ) -> Option<cratonvm_native_api::AnnotationElementValue> {
        self.inner.method_annotation_default(class_id, m, d)
    }
    fn module_name_of_class(&self, class_id: ClassId) -> Option<String> {
        self.inner.module_name_of_class(class_id)
    }
    fn find_resource(&self, name: &str) -> Option<Vec<u8>> {
        self.inner.find_resource(name)
    }
    fn list_application_class_names(&self) -> Vec<String> {
        self.inner.list_application_class_names()
    }
    fn register_dynamic_classpath(&mut self, paths: &[String]) {
        self.inner.register_dynamic_classpath(paths)
    }
    fn define_class_from_bytes(&mut self, name: &str, bytes: &[u8]) -> Option<ClassId> {
        self.inner.define_class_from_bytes(name, bytes)
    }
    fn define_class_with_loader(
        &mut self,
        name: &str,
        bytes: &[u8],
        loader_id: u32,
    ) -> Option<ClassId> {
        self.inner.define_class_with_loader(name, bytes, loader_id)
    }
    fn class_id_by_name_and_loader(&self, name: &str, loader_id: u32) -> Option<ClassId> {
        // SAFETY: single-threaded test code.
        let table = unsafe { &*self.by_loader.get() };
        table
            .get(name)
            .copied()
            .or_else(|| self.inner.class_id_by_name_and_loader(name, loader_id))
    }
    fn class_id_seen_by_class_without_loading(
        &self,
        name: &str,
        _referencing: ClassId,
    ) -> Option<ClassId> {
        // SAFETY: single-threaded test code.
        unsafe { &*self.by_loader.get() }.get(name).copied()
    }
    fn allocate_loader_id(&mut self) -> u32 {
        self.inner.allocate_loader_id()
    }
    fn is_package_exported_unqualified(&self, module_name: &str, pkg: &str) -> bool {
        self.inner.is_package_exported_unqualified(module_name, pkg)
    }
    fn is_package_exported_to(&self, module_name: &str, pkg: &str, to_module: &str) -> bool {
        self.inner
            .is_package_exported_to(module_name, pkg, to_module)
    }
    fn is_package_open_unqualified(&self, module_name: &str, pkg: &str) -> bool {
        self.inner.is_package_open_unqualified(module_name, pkg)
    }
    fn is_package_open_to(&self, module_name: &str, pkg: &str, to_module: &str) -> bool {
        self.inner.is_package_open_to(module_name, pkg, to_module)
    }
    fn check_deep_reflection_access(
        &self,
        accessor_class_id: ClassId,
        target_class_id: ClassId,
    ) -> Result<(), String> {
        self.inner
            .check_deep_reflection_access(accessor_class_id, target_class_id)
    }
}

impl cratonvm_native_api::NativeInvokeAccess for UpdaterMock {
    fn invoke(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        self.inner.invoke(class_name, method_name, descriptor, args)
    }
    fn invoke_virtual(
        &mut self,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        self.inner
            .invoke_virtual(receiver, method_name, descriptor, args)
    }
}

impl cratonvm_native_api::NativeHeapAccess for UpdaterMock {
    fn new_object(&mut self, class_name: &str) -> MethodCallResult {
        // SAFETY: single-threaded test code.
        unsafe { &mut *self.allocated.get() }.push(class_name.to_string());
        self.inner.new_object(class_name)
    }
    fn identity_hash_code(&self, obj: ObjectRef) -> i32 {
        self.inner.identity_hash_code(obj)
    }
    fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
        self.inner.get_field(obj, index)
    }
    fn set_field(&self, obj: ObjectRef, index: usize, value: Value) {
        self.inner.set_field(obj, index, value)
    }
    fn get_field_by_name(&self, obj: ObjectRef, field_name: &str) -> Value {
        self.inner.get_field_by_name(obj, field_name)
    }
    fn set_field_by_name(&self, obj: ObjectRef, field_name: &str, value: Value) {
        self.inner.set_field_by_name(obj, field_name, value)
    }
    fn resolve_field_index(&self, c: &str, f: &str) -> Option<usize> {
        self.inner.resolve_field_index(c, f)
    }
    fn resolve_field_index_by_class_id(&self, c: ClassId, f: &str) -> Option<usize> {
        self.inner.resolve_field_index_by_class_id(c, f)
    }
    fn new_array(
        &mut self,
        element_type: cratonvm_types::ArrayElementType,
        length: usize,
    ) -> ObjectRef {
        self.inner.new_array(element_type, length)
    }
    fn new_ref_array(&mut self, class_id: ClassId, length: usize) -> ObjectRef {
        self.inner.new_ref_array(class_id, length)
    }
    fn array_length(&self, obj: ObjectRef) -> usize {
        self.inner.array_length(obj)
    }
    fn get_array_element(&self, obj: ObjectRef, index: usize) -> Value {
        self.inner.get_array_element(obj, index)
    }
    fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) {
        self.inner.set_array_element(obj, index, value)
    }
    fn heap_kind_of(&self, obj: ObjectRef) -> cratonvm_types::ObjectKind {
        self.inner.heap_kind_of(obj)
    }
    fn heap_element_type_of(&self, obj: ObjectRef) -> cratonvm_types::ArrayElementType {
        self.inner.heap_element_type_of(obj)
    }
    fn create_string(&mut self, text: &str) -> ObjectRef {
        self.inner.create_string(text)
    }
    fn read_string(&self, obj: ObjectRef) -> Option<String> {
        self.inner.read_string(obj)
    }
    fn get_class_mirror(&mut self, class_id: ClassId) -> ObjectRef {
        self.inner.get_class_mirror(class_id)
    }
    fn alloc_object(&mut self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        self.inner.alloc_object(class_id, num_fields)
    }
    fn object_num_fields(&self, obj: ObjectRef) -> usize {
        self.inner.object_num_fields(obj)
    }
    fn get_field_volatile(&self, obj: ObjectRef, index: usize) -> Value {
        self.inner.get_field_volatile(obj, index)
    }
    fn set_field_volatile(&self, obj: ObjectRef, index: usize, value: Value) {
        self.inner.set_field_volatile(obj, index, value)
    }
    fn compare_and_swap_field(
        &mut self,
        obj: ObjectRef,
        index: usize,
        expected: Value,
        new_val: Value,
    ) -> bool {
        self.inner
            .compare_and_swap_field(obj, index, expected, new_val)
    }
    fn allocate_instance(&mut self, class_name: &str) -> Option<ObjectRef> {
        self.inner.allocate_instance(class_name)
    }
    fn discover_reference(
        &mut self,
        ref_type: u8,
        reference_obj: ObjectRef,
        referent: ObjectRef,
        queue: Option<ObjectRef>,
    ) {
        self.inner
            .discover_reference(ref_type, reference_obj, referent, queue)
    }
    fn heap_allocated_bytes(&self) -> usize {
        self.inner.heap_allocated_bytes()
    }
}

impl cratonvm_native_api::NativeThreadAccess for UpdaterMock {
    fn thread_id(&self) -> u64 {
        self.inner.thread_id()
    }
    fn monitor_enter(&mut self, obj: ObjectRef) {
        self.inner.monitor_enter(obj)
    }
    fn monitor_exit(&mut self, obj: ObjectRef) {
        self.inner.monitor_exit(obj)
    }
    fn monitor_wait(&mut self, obj: ObjectRef, timeout_ms: Option<u64>) -> MethodCallResult {
        self.inner.monitor_wait(obj, timeout_ms)
    }
    fn monitor_notify(&mut self, obj: ObjectRef) -> MethodCallResult {
        self.inner.monitor_notify(obj)
    }
    fn monitor_notify_all(&mut self, obj: ObjectRef) -> MethodCallResult {
        self.inner.monitor_notify_all(obj)
    }
    fn thread_start(&mut self, thread_obj: ObjectRef) -> MethodCallResult {
        self.inner.thread_start(thread_obj)
    }
    fn thread_join(&mut self, thread_obj: ObjectRef) -> MethodCallResult {
        self.inner.thread_join(thread_obj)
    }
    fn thread_is_alive(&self, thread_obj: ObjectRef) -> bool {
        self.inner.thread_is_alive(thread_obj)
    }
    fn current_thread_object(&mut self) -> ObjectRef {
        self.inner.current_thread_object()
    }
    fn thread_interrupt(&mut self, thread_obj: ObjectRef) {
        self.inner.thread_interrupt(thread_obj)
    }
    fn is_interrupted(&self, clear: bool) -> bool {
        self.inner.is_interrupted(clear)
    }
    fn park(&mut self, timeout: Option<std::time::Duration>) {
        self.inner.park(timeout)
    }
    fn unpark(&self, thread_obj: ObjectRef) {
        self.inner.unpark(thread_obj)
    }
    fn get_scoped_value(&self, key_id: u64) -> Option<Value> {
        self.inner.get_scoped_value(key_id)
    }
    fn push_scoped_value(&mut self, key_id: u64, value: Value) {
        self.inner.push_scoped_value(key_id, value)
    }
    fn pop_scoped_value(&mut self) {
        self.inner.pop_scoped_value()
    }
    fn scoped_value_depth(&self) -> usize {
        self.inner.scoped_value_depth()
    }
    fn active_thread_count(&self) -> i32 {
        self.inner.active_thread_count()
    }
    fn enumerate_threads(&self, max: usize) -> Vec<ObjectRef> {
        self.inner.enumerate_threads(max)
    }
}

impl cratonvm_native_api::NativeExceptionAccess for UpdaterMock {
    fn capture_stack_trace(&mut self) -> Vec<cratonvm_native_api::StackTraceEntry> {
        self.inner.capture_stack_trace()
    }
    fn get_throwable_stack_trace(
        &self,
        throwable: ObjectRef,
    ) -> Option<Vec<cratonvm_native_api::StackTraceEntry>> {
        self.inner.get_throwable_stack_trace(throwable)
    }
}

impl cratonvm_native_api::NativeGpuAccess for UpdaterMock {}

impl cratonvm_native_api::NativeSystemAccess for UpdaterMock {
    fn record_printed_value(&mut self, value: Value) {
        self.inner.record_printed_value(value)
    }
    fn record_printed_line(&mut self, text: String) {
        self.inner.record_printed_line(text)
    }
    fn get_system_stream(&self, name: &str) -> Option<ObjectRef> {
        self.inner.get_system_stream(name)
    }
    fn get_system_property(&self, key: &str) -> Option<String> {
        self.inner.get_system_property(key)
    }
    fn set_system_property(&mut self, key: &str, value: &str) -> Option<String> {
        self.inner.set_system_property(key, value)
    }
    fn is_interface_class(&self, class_id: ClassId) -> bool {
        self.inner.is_interface_class(class_id)
    }
    fn get_static_field(&self, class_id: ClassId, field_index: usize) -> Value {
        self.inner.get_static_field(class_id, field_index)
    }
    fn set_static_field(&mut self, class_id: ClassId, field_index: usize, value: Value) {
        self.inner.set_static_field(class_id, field_index, value)
    }
    fn fd_table(&self) -> &cratonvm_native_api::fd_table::FileDescriptorTable {
        self.inner.fd_table()
    }
    fn allocate_native_memory(&mut self, size: usize, align: usize) -> Option<(i64, *mut u8)> {
        self.inner.allocate_native_memory(size, align)
    }
    fn free_native_memory(&mut self, alloc_id: i64) {
        self.inner.free_native_memory(alloc_id)
    }
    fn load_native_library(&mut self, path: &str) -> Result<i64, MethodCallFailed> {
        self.inner.load_native_library(path)
    }
    fn find_native_symbol(&self, lib_index: i64, name: &str) -> Option<usize> {
        self.inner.find_native_symbol(lib_index, name)
    }
    fn register_upcall(&mut self, entry: cratonvm_native_api::ffi::UpcallEntry) -> usize {
        self.inner.register_upcall(entry)
    }
    fn get_upcall_info(&self, slot: usize) -> Option<(ObjectRef, Vec<i32>, i32)> {
        self.inner.get_upcall_info(slot)
    }
    fn loaded_class_count(&self) -> usize {
        self.inner.loaded_class_count()
    }
    fn gc_collection_count(&self) -> u64 {
        self.inner.gc_collection_count()
    }
    fn gc_address_epoch(&self) -> u64 {
        self.inner.gc_address_epoch()
    }
    fn force_gc(&mut self) {
        self.inner.force_gc()
    }
    fn force_gc_for_vm(&mut self) {
        self.inner.force_gc_for_vm()
    }
}

fn make_class_mirror_um(um: &mut UpdaterMock, name: &str) -> (ObjectRef, ClassId) {
    make_class_mirror(&mut um.inner, name)
}

static ARFU_TARGET_OLD: AtomicUsize = AtomicUsize::new(0);
static ARFU_TARGET_NEW: AtomicUsize = AtomicUsize::new(0);
static ARFU_OP_OLD: AtomicUsize = AtomicUsize::new(0);
static ARFU_OP_NEW: AtomicUsize = AtomicUsize::new(0);
static ARFU_PREV_OLD: AtomicUsize = AtomicUsize::new(0);
static ARFU_PREV_NEW: AtomicUsize = AtomicUsize::new(0);
static ARFU_APPLIED: AtomicUsize = AtomicUsize::new(0);
static ARFU_APPLY_CALLS: AtomicUsize = AtomicUsize::new(0);

fn relocating_arfu_apply(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if (method_name, descriptor) != ("apply", "(Ljava/lang/Object;)Ljava/lang/Object;") {
        return None;
    }
    ARFU_APPLY_CALLS.fetch_add(1, Ordering::SeqCst);
    assert_eq!(
        receiver.as_ptr() as usize,
        ARFU_OP_OLD.load(Ordering::SeqCst)
    );
    assert_eq!(
        args.first().copied(),
        Some(Value::Object(Some(unsafe {
            ObjectRef::from_raw(ARFU_PREV_OLD.load(Ordering::SeqCst) as *mut u8)
        })))
    );
    ctx.remap_native_pin_addr_for_test(
        ARFU_TARGET_OLD.load(Ordering::SeqCst),
        ARFU_TARGET_NEW.load(Ordering::SeqCst),
    );
    ctx.remap_native_pin_addr_for_test(
        ARFU_OP_OLD.load(Ordering::SeqCst),
        ARFU_OP_NEW.load(Ordering::SeqCst),
    );
    ctx.remap_native_pin_addr_for_test(
        ARFU_PREV_OLD.load(Ordering::SeqCst),
        ARFU_PREV_NEW.load(Ordering::SeqCst),
    );
    Some(Ok(Some(Value::Object(Some(unsafe {
        ObjectRef::from_raw(ARFU_APPLIED.load(Ordering::SeqCst) as *mut u8)
    })))))
}

// -----------------------------------------------------------------
// T19.H5: validation-only tests
// -----------------------------------------------------------------

#[test]
fn t19_h5_validate_field_name_accepts_normal_identifiers() {
    assert!(validate_field_name("next").is_ok());
    assert!(validate_field_name("FIELD_42").is_ok());
    assert!(validate_field_name("$inner$0").is_ok());
    assert!(validate_field_name("a").is_ok());
}

#[test]
fn t19_h5_validate_field_name_rejects_dangerous_inputs() {
    assert!(validate_field_name("").is_err());
    assert!(validate_field_name(&"a".repeat(257)).is_err());
    assert!(validate_field_name("../traversal").is_err());
    assert!(validate_field_name("foo/bar").is_err());
    assert!(validate_field_name("foo\\bar").is_err());
    assert!(validate_field_name("foo:bar").is_err());
    assert!(validate_field_name("foo\nbar").is_err());
    assert!(validate_field_name("foo\0bar").is_err());
    assert!(validate_field_name("foo\x7Fbar").is_err());
}

#[test]
fn t19_h5_descriptor_tag_helpers_match_jdk_acceptance() {
    assert_eq!(descriptor_tag_for_int_variant("I"), Some(DESC_TAG_INT));
    assert_eq!(descriptor_tag_for_int_variant("Z"), Some(DESC_TAG_INT));
    assert_eq!(descriptor_tag_for_int_variant("J"), None); // long rejected
    assert_eq!(descriptor_tag_for_int_variant("Ljava/lang/Object;"), None);
    assert_eq!(descriptor_tag_for_long_variant("J"), Some(DESC_TAG_LONG));
    assert_eq!(descriptor_tag_for_long_variant("I"), None);
    assert!(descriptor_is_reference("Ljava/lang/Object;"));
    assert!(descriptor_is_reference("[I"));
    assert!(!descriptor_is_reference("I"));
}

#[test]
fn t19_h5_arfu_update_rereads_pins_after_operator_gc() {
    let mut ctx = mock_ctx();
    let target_old = ctx.fresh_object_ref();
    let target_new = ctx.fresh_object_ref();
    let op_old = ctx.fresh_object_ref();
    let op_new = ctx.fresh_object_ref();
    let prev_old = ctx.fresh_object_ref();
    let prev_new = ctx.fresh_object_ref();
    let applied = ctx.fresh_object_ref();
    let slot = 1;

    ctx.set_field(target_old, slot, Value::Object(Some(prev_old)));
    ctx.set_field(target_new, slot, Value::Object(Some(prev_new)));

    ARFU_TARGET_OLD.store(target_old.as_ptr() as usize, Ordering::SeqCst);
    ARFU_TARGET_NEW.store(target_new.as_ptr() as usize, Ordering::SeqCst);
    ARFU_OP_OLD.store(op_old.as_ptr() as usize, Ordering::SeqCst);
    ARFU_OP_NEW.store(op_new.as_ptr() as usize, Ordering::SeqCst);
    ARFU_PREV_OLD.store(prev_old.as_ptr() as usize, Ordering::SeqCst);
    ARFU_PREV_NEW.store(prev_new.as_ptr() as usize, Ordering::SeqCst);
    ARFU_APPLIED.store(applied.as_ptr() as usize, Ordering::SeqCst);
    ARFU_APPLY_CALLS.store(0, Ordering::SeqCst);
    ctx.set_invoke_virtual_hook(relocating_arfu_apply);

    // `Int(0)`: no vclass recorded, so no `valueCheck` (r21 updaters).
    let result =
        arfu_update_with_operator(&mut ctx, target_old, slot, op_old, true, Value::Int(0));

    assert_eq!(result.unwrap(), Some(Value::Object(Some(applied))));
    assert_eq!(ARFU_APPLY_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(
        ctx.get_field(target_new, slot),
        Value::Object(Some(applied))
    );
    assert_eq!(
        ctx.get_field(target_old, slot),
        Value::Object(Some(prev_old))
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

// -----------------------------------------------------------------
// T19.H5: ARFU.newUpdater happy + error paths
// -----------------------------------------------------------------

#[test]
fn t19_h5_register_natives_lists_every_factory() {
    let mut r = NativeMethodRegistry::new();
    register_atomic_updater_natives(&mut r);
    assert!(r
        .find(
            CLS_REF_FIELD_UPDATER,
            "newUpdater",
            "(Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/String;)Ljava/util/concurrent/atomic/AtomicReferenceFieldUpdater;",
        )
        .is_some());
    assert!(r
        .find(
            CLS_INT_FIELD_UPDATER,
            "newUpdater",
            "(Ljava/lang/Class;Ljava/lang/String;)Ljava/util/concurrent/atomic/AtomicIntegerFieldUpdater;",
        )
        .is_some());
    assert!(r
        .find(
            CLS_LONG_FIELD_UPDATER,
            "newUpdater",
            "(Ljava/lang/Class;Ljava/lang/String;)Ljava/util/concurrent/atomic/AtomicLongFieldUpdater;",
        )
        .is_some());
    // Plus impl-class accessors must be present.
    assert!(r
        .find(
            CLS_REF_FIELD_UPDATER_IMPL,
            "compareAndSet",
            "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z",
        )
        .is_some());
    assert!(r
        .find(
            CLS_INT_FIELD_UPDATER_IMPL,
            "getAndAdd",
            "(Ljava/lang/Object;I)I"
        )
        .is_some());
}

/// `--real-jdk` must register **nothing** from this module.
///
/// Not just `newUpdater`. The base-class accessor rows are the half that
/// fails silently: `getAndIncrement`/`addAndGet`/… are CONCRETE on the
/// abstract `Atomic*FieldUpdater` in the JDK and are not overridden by
/// `Atomic*FieldUpdaterImpl`, so with a real impl receiver CratonVM's
/// superclass-walking virtual dispatch would hand the call to the native,
/// `impl_slot` would read the impl's `cclass` reference out of
/// `FU_SLOT_FIELD_INDEX`, get `None`, and `.unwrap_or(0)` would fetch-add
/// **slot 0** of the target object. A test that only checked `newUpdater`
/// would be green on precisely the tree that does that.
///
/// Asserted as "the whole registry is empty" as well as per triple, so a
/// future registration added inside `register_arfu`/`register_aifu`/
/// `register_alfu` is covered without anyone remembering to extend the
/// list. The compatible arm below is the mutation control: it proves the
/// emptiness is the mode's doing and not a broken registrar.
#[test]
fn t19_h5_jdk_only_registers_nothing_from_this_module() {
    let mut strict = NativeMethodRegistry::new();
    strict.set_compatibility_mode(cratonvm_types::compat::CompatibilityMode::JdkOnly);
    register_atomic_updater_natives(&mut strict);
    assert!(
        strict.is_empty(),
        "--real-jdk must register no *FieldUpdater native at all ({} rows present); \
         the JDK's own Atomic*FieldUpdaterImpl bytecode is what strict mode exists \
         to run",
        strict.len()
    );

    for (cls, name, desc) in [
        (
            CLS_REF_FIELD_UPDATER,
            "newUpdater",
            "(Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/String;)Ljava/util/concurrent/atomic/AtomicReferenceFieldUpdater;",
        ),
        (
            CLS_INT_FIELD_UPDATER,
            "newUpdater",
            "(Ljava/lang/Class;Ljava/lang/String;)Ljava/util/concurrent/atomic/AtomicIntegerFieldUpdater;",
        ),
        (
            CLS_LONG_FIELD_UPDATER,
            "newUpdater",
            "(Ljava/lang/Class;Ljava/lang/String;)Ljava/util/concurrent/atomic/AtomicLongFieldUpdater;",
        ),
        // The silent half — the accessors, which is where a `newUpdater`-only
        // test would be green on a broken tree.
        //
        // These used to be the ABSTRACT BASE classes. They are the impl
        // classes since 2026-08-27: registering an accessor on
        // `AtomicIntegerFieldUpdater` itself shadowed the inherited defaults
        // of any application subclass of that public abstract class, and the
        // rows were dead for this module's own objects anyway (dispatch
        // probes the receiver's own class first). See the note above
        // `register_aifu`'s impl block for the measurement.
        //
        // This list has to name registrations that really exist, because the
        // compatible arm below is a MUTATION CONTROL: if the triple is wrong
        // the strict assertion above passes vacuously. When the base rows
        // were removed, this control is what failed and said so.
        (CLS_REF_FIELD_UPDATER_IMPL, "get", "(Ljava/lang/Object;)Ljava/lang/Object;"),
        (CLS_INT_FIELD_UPDATER_IMPL, "getAndIncrement", "(Ljava/lang/Object;)I"),
        (CLS_INT_FIELD_UPDATER_IMPL, "addAndGet", "(Ljava/lang/Object;I)I"),
        (CLS_LONG_FIELD_UPDATER_IMPL, "incrementAndGet", "(Ljava/lang/Object;)J"),
        (CLS_LONG_FIELD_UPDATER_IMPL, "getAndAdd", "(Ljava/lang/Object;J)J"),
    ] {
        assert!(
            strict.find(cls, name, desc).is_none(),
            "--real-jdk still serves {cls}.{name}{desc}"
        );
        // The control: the same triple IS served in Compatible mode, so a
        // green assertion above cannot come from a typo in the triple.
        let mut compatible = NativeMethodRegistry::new();
        register_atomic_updater_natives(&mut compatible);
        assert!(
            compatible.find(cls, name, desc).is_some(),
            "{cls}.{name}{desc} is not registered in Compatible mode either — the \
             strict assertion above is vacuous"
        );
    }
}

#[test]
fn t19_h5_arfu_new_updater_happy_path_returns_populated_impl() {
    let mut um = UpdaterMock::new();
    let (tmirror, tcid) = make_class_mirror_um(&mut um, "ExtHandler");
    let (vmirror, vcid) = make_class_mirror_um(&mut um, "java/util/logging/Handler");
    um.add_field(
        tcid,
        "next",
        "Ljava/util/logging/Handler;",
        0x0040, /* ACC_VOLATILE */
        7,
        false,
    );
    let name = um.create_string("next");
    let result = native_arfu_new_updater(
        &mut um,
        &[
            Value::Object(Some(tmirror)),
            Value::Object(Some(vmirror)),
            Value::Object(Some(name)),
        ],
    )
    .unwrap()
    .unwrap();
    let impl_obj = match result {
        Value::Object(Some(o)) => o,
        other => panic!("expected ObjectRef, got {other:?}"),
    };
    // tclass / slot / desc-tag / vclass slots populated.
    assert_eq!(
        um.get_field(impl_obj, FU_SLOT_TCLASS_ID),
        Value::Int(tcid.as_u32() as i32),
    );
    assert_eq!(um.get_field(impl_obj, FU_SLOT_FIELD_INDEX), Value::Int(7));
    assert_eq!(
        um.get_field(impl_obj, FU_SLOT_DESC_TAG),
        Value::Int(DESC_TAG_REF)
    );
    assert_eq!(
        um.get_field(impl_obj, FU_SLOT_VCLASS_ID),
        Value::Int(vcid.as_u32() as i32),
    );
}

#[test]
fn t19_h5_arfu_new_updater_missing_field_throws_runtime_exception() {
    let mut um = UpdaterMock::new();
    let (tmirror, _) = make_class_mirror_um(&mut um, "Target");
    let (vmirror, _) = make_class_mirror_um(&mut um, "java/lang/Object");
    let name = um.create_string("does_not_exist");
    let err = native_arfu_new_updater(
        &mut um,
        &[
            Value::Object(Some(tmirror)),
            Value::Object(Some(vmirror)),
            Value::Object(Some(name)),
        ],
    )
    .unwrap_err();
    // A THROWN java.lang.RuntimeException, not an IllegalArgumentException.
    //
    // CHANGED 2026-08-27, and the old assertion was this VM's behaviour
    // rather than the JDK's. All three `newUpdater` factories wrap their
    // reflection in `catch (Exception ex) { throw new RuntimeException(ex); }`,
    // so a missing field arrives as a plain `RuntimeException`.
    // MEASURED (`probes/AtomicUpdaterSweep.java`): HotSpot
    // `java.lang.RuntimeException`, this VM `IllegalArgumentException`.
    //
    // The mock raises it through `new_object_initialized`, so it lands as
    // `ExceptionThrown` rather than an `InternalError` variant -- which is
    // itself the signal that the refusal now goes out as a real Java
    // throwable of a named class instead of a Rust-side enum arm.
    assert!(
        matches!(err, MethodCallFailed::ExceptionThrown(_)),
        "a missing field must be a thrown java.lang.RuntimeException, got {err:?}"
    );
}

#[test]
fn t19_h5_arfu_new_updater_bad_field_name_throws_iae() {
    let mut um = UpdaterMock::new();
    let (tmirror, _) = make_class_mirror_um(&mut um, "Target");
    let (vmirror, _) = make_class_mirror_um(&mut um, "java/lang/Object");
    let name = um.create_string("../traversal");
    let err = native_arfu_new_updater(
        &mut um,
        &[
            Value::Object(Some(tmirror)),
            Value::Object(Some(vmirror)),
            Value::Object(Some(name)),
        ],
    )
    .unwrap_err();
    if field_updater_jdk_rules() {
        // r21 wave 9 review (rev-w68-native): JDK 25 screens no name; it
        // is a missing field, `RuntimeException(NoSuchFieldException)`
        // (`r21_w9_updaters_odd_field_names_are_missing_fields`).
        assert!(
            matches!(err, MethodCallFailed::ExceptionThrown(_)),
            "jdk rules: a missing field, got {err:?}"
        );
        return;
    }
    assert!(matches!(
        err,
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            RuntimeError::IllegalArgumentException { .. }
        ))
    ));
}

#[test]
fn t19_h5_arfu_new_updater_type_mismatch_throws_cce() {
    let mut um = UpdaterMock::new();
    let (tmirror, tcid) = make_class_mirror_um(&mut um, "Target");
    let (vmirror_wrong, _) = make_class_mirror_um(&mut um, "java/lang/Integer");
    // declared as `String` but vclass is `Integer` — type mismatch.
    // ACC_VOLATILE: this test is about the vclass MISMATCH, and a
    // non-volatile fixture would now fail one check earlier for an
    // unrelated reason.
    um.add_field(tcid, "f", "Ljava/lang/String;", 0x0040, 0, false);
    let name = um.create_string("f");
    let err = native_arfu_new_updater(
        &mut um,
        &[
            Value::Object(Some(tmirror)),
            Value::Object(Some(vmirror_wrong)),
            Value::Object(Some(name)),
        ],
    )
    .unwrap_err();
    match err {
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            RuntimeError::ClassCastException { .. },
        )) => {}
        other => panic!("expected ClassCastException, got {other:?}"),
    }
}

#[test]
fn t19_h5_arfu_new_updater_object_declared_accepts_any_vclass() {
    // CHANGED JIT round 21 wave 1 (lane updaters): the "special case" this
    // test pinned is not the JDK's. JDK 25 `AtomicReferenceFieldUpdaterImpl`
    // throws `ClassCastException` when `vclass != field.getType()`, an
    // identity test with no exception for `java.lang.Object`. The old
    // acceptance survives only under `CRATONVM_FIELD_UPDATER_JDK_RULES=0`.
    if field_updater_jdk_rules() {
        return;
    }
    let mut um = UpdaterMock::new();
    let (tmirror, tcid) = make_class_mirror_um(&mut um, "Target");
    let (vmirror, _) = make_class_mirror_um(&mut um, "java/lang/Integer");
    // ACC_VOLATILE — see the sibling test above.
    um.add_field(tcid, "f", "Ljava/lang/Object;", 0x0040, 0, false);
    let name = um.create_string("f");
    let res = native_arfu_new_updater(
        &mut um,
        &[
            Value::Object(Some(tmirror)),
            Value::Object(Some(vmirror)),
            Value::Object(Some(name)),
        ],
    );
    assert!(res.is_ok());
}

/// JIT round 21 wave 1, lane updaters (`CRATONVM_FIELD_UPDATER_JDK_RULES`):
/// the JDK 25 factories' own rules -- `vclass` must BE the field's type
/// (a `java.lang.Object` field with `Integer.class` is a message-less
/// `ClassCastException`), an int updater takes exactly `int` ("Must be
/// integer type" for a `boolean`), and a field a superclass declares is
/// not found (`getDeclaredField`: a thrown `RuntimeException`).
#[test]
fn r21_updaters_new_updater_follows_the_jdk_factory_rules() {
    if !field_updater_jdk_rules() {
        return;
    }
    // vclass identity.
    let mut um = UpdaterMock::new();
    let (tmirror, tcid) = make_class_mirror_um(&mut um, "Target");
    let (vmirror, _) = make_class_mirror_um(&mut um, "java/lang/Integer");
    um.add_field(tcid, "f", "Ljava/lang/Object;", 0x0040, 0, false);
    let name = um.create_string("f");
    let err = native_arfu_new_updater(
        &mut um,
        &[
            Value::Object(Some(tmirror)),
            Value::Object(Some(vmirror)),
            Value::Object(Some(name)),
        ],
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::ClassCastException { ref message }
            )) if message.is_empty()
        ),
        "Object field + Integer.class must be a message-less CCE, got {err:?}"
    );
    // The same field with its own type is accepted.
    let (omirror, _) = make_class_mirror_um(&mut um, "java/lang/Object");
    let name = um.create_string("f");
    assert!(native_arfu_new_updater(
        &mut um,
        &[
            Value::Object(Some(tmirror)),
            Value::Object(Some(omirror)),
            Value::Object(Some(name)),
        ],
    )
    .is_ok());

    // An int updater on a boolean field.
    let mut um = UpdaterMock::new();
    let (tmirror, tcid) = make_class_mirror_um(&mut um, "Flags");
    um.add_field(tcid, "z", "Z", 0x0040, 0, false);
    let name = um.create_string("z");
    let err = native_aifu_new_updater(
        &mut um,
        &[Value::Object(Some(tmirror)), Value::Object(Some(name))],
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::IllegalArgumentException { ref message }
            )) if message == "Must be integer type"
        ),
        "a boolean field must be \"Must be integer type\", got {err:?}"
    );

    // A field the SUPERCLASS declares.
    let mut um = UpdaterMock::new();
    let (_, base) = make_class_mirror_um(&mut um, "p/Base");
    let (sub_mirror, sub) = make_class_mirror_um(&mut um, "p/Sub");
    um.inner.set_superclass(sub, base);
    um.add_field(base, "i", "I", 0x0040, 0, false);
    let name = um.create_string("i");
    let err = native_aifu_new_updater(
        &mut um,
        &[Value::Object(Some(sub_mirror)), Value::Object(Some(name))],
    )
    .unwrap_err();
    assert!(
        matches!(err, MethodCallFailed::ExceptionThrown(_)),
        "an inherited field must be a thrown RuntimeException(NoSuchFieldException), got {err:?}"
    );
}

/// r21 review items 4 (lane updaters): `vclass` against the field type by
/// IDENTITY through the declaring class's loader -- a same-named class of
/// another loader is the message-less CCE, the field's own class passes;
/// a primitive `vclass` on a reference field is the CCE (not "vclass is
/// not a regular class mirror"); the primitive `vclass` that IS a primitive
/// field's type is IAE "Must be reference type".
#[test]
fn r21_updaters_vclass_identity_and_primitive_vclass_answers() {
    if !field_updater_jdk_rules() {
        return;
    }
    let cce = |r: MethodCallResult| {
        matches!(
            r,
            Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::ClassCastException { ref message }
            ))) if message.is_empty()
        )
    };
    let call = |um: &mut UpdaterMock, t: ObjectRef, v: ObjectRef, f: &str| {
        let name = um.create_string(f);
        native_arfu_new_updater(
            um,
            &[
                Value::Object(Some(t)),
                Value::Object(Some(v)),
                Value::Object(Some(name)),
            ],
        )
    };
    // Identity: two classes named `p/V`, the field's type the first.
    let mut um = UpdaterMock::new();
    let (tmirror, tcid) = make_class_mirror_um(&mut um, "p/T");
    let (vmirror, vcid) = make_class_mirror_um(&mut um, "p/V");
    let other = um.inner.declare_second_copy("p/V");
    let other_mirror = um.get_class_mirror(other);
    um.add_field(tcid, "v", "Lp/V;", 0x0040, 0, false);
    // SAFETY: single-threaded test code.
    unsafe { (*um.by_loader.get()).insert("p/V".to_string(), vcid) };
    assert!(call(&mut um, tmirror, vmirror, "v").is_ok(), "the field's own class");
    assert!(cce(call(&mut um, tmirror, other_mirror, "v")), "a same-named class of another loader");
    // A primitive vclass on a reference field.
    let int_mirror = um.primitive_class_mirror("int");
    assert!(cce(call(&mut um, tmirror, int_mirror, "v")), "int.class on a reference field");
    // The primitive vclass that is the primitive field's own type.
    um.add_field(tcid, "i", "I", 0x0040, 1, false);
    let int_mirror = um.primitive_class_mirror("int");
    let err = call(&mut um, tmirror, int_mirror, "i").unwrap_err();
    assert!(
        matches!(
            err,
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::IllegalArgumentException { ref message }
            )) if message == "Must be reference type"
        ),
        "int.class on an int field, got {err:?}"
    );
    // Another primitive vclass on that int field: the CCE.
    let long_mirror = um.primitive_class_mirror("long");
    assert!(cce(call(&mut um, tmirror, long_mirror, "i")), "long.class on an int field");
}

/// r21 wave 7, lane updaters (page `r21-review-w45-vm-findings-20261019.md`
/// item 1): the field type is what the DECLARING class's resolver view
/// answers, not the first same-named class up the loader's parent chain.
/// A child-first / sibling-delegating loader resolved `p/V` to the second
/// copy: that copy passes and the parent-chain copy is the CCE. When the
/// resolver does not know the name yet, the name comparison answers.
#[test]
fn r21_w7_updaters_vclass_identity_follows_the_declaring_class_resolver() {
    if !field_updater_jdk_rules() {
        return;
    }
    let call = |um: &mut UpdaterMock, t: ObjectRef, v: ObjectRef| {
        let name = um.create_string("v");
        native_arfu_new_updater(
            um,
            &[
                Value::Object(Some(t)),
                Value::Object(Some(v)),
                Value::Object(Some(name)),
            ],
        )
    };
    let mut um = UpdaterMock::new();
    let (tmirror, tcid) = make_class_mirror_um(&mut um, "p/T");
    let (parent_mirror, _) = make_class_mirror_um(&mut um, "p/V");
    let sibling = um.inner.declare_second_copy("p/V");
    let sibling_mirror = um.get_class_mirror(sibling);
    um.add_field(tcid, "v", "Lp/V;", 0x0040, 0, false);
    // Not known to the resolver yet: both same-named copies pass by name.
    assert!(call(&mut um, tmirror, sibling_mirror).is_ok(), "unknown: by name");
    assert!(call(&mut um, tmirror, parent_mirror).is_ok(), "unknown: by name");
    // The declaring class resolved `p/V` to the sibling's copy.
    // SAFETY: single-threaded test code.
    unsafe { (*um.by_loader.get()).insert("p/V".to_string(), sibling) };
    assert!(call(&mut um, tmirror, sibling_mirror).is_ok(), "the class the field resolved to");
    let err = call(&mut um, tmirror, parent_mirror).unwrap_err();
    assert!(
        matches!(
            err,
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::ClassCastException { ref message }
            )) if message.is_empty()
        ),
        "the parent chain's same-named class, got {err:?}"
    );
}

/// r21 wave 9 review (rev-w68-native): JDK 25's `getDeclaredField` refuses
/// no name shape; an empty, slashed, dotted or overlong name is just a
/// missing field, `RuntimeException(NoSuchFieldException(name))`, for all
/// three factories -- never the old screen's `IllegalArgumentException`.
#[test]
fn r21_w9_updaters_odd_field_names_are_missing_fields() {
    if !field_updater_jdk_rules() {
        return;
    }
    let long_name = "a".repeat(300);
    for odd in ["", "a/b", "../x", "a:b", long_name.as_str()] {
        for variant in 0..3 {
            let mut um = UpdaterMock::new();
            let (tmirror, _) = make_class_mirror_um(&mut um, "p/T");
            let (vmirror, _) = make_class_mirror_um(&mut um, "p/V");
            let name = um.create_string(odd);
            let err = match variant {
                0 => native_arfu_new_updater(
                    &mut um,
                    &[
                        Value::Object(Some(tmirror)),
                        Value::Object(Some(vmirror)),
                        Value::Object(Some(name)),
                    ],
                ),
                1 => native_aifu_new_updater(
                    &mut um,
                    &[Value::Object(Some(tmirror)), Value::Object(Some(name))],
                ),
                _ => native_alfu_new_updater(
                    &mut um,
                    &[Value::Object(Some(tmirror)), Value::Object(Some(name))],
                ),
            }
            .unwrap_err();
            assert!(
                matches!(err, MethodCallFailed::ExceptionThrown(_))
                    && um.allocated().ends_with(&[
                        "java/lang/NoSuchFieldException".to_string(),
                        "java/lang/RuntimeException".to_string(),
                    ]),
                "name {odd:?} (variant {variant}), got {err:?}"
            );
        }
    }
}

/// r21 wave 7, lane updaters (page `r21-review-w45-vm-findings-20261019.md`
/// item 2): JDK 25's factories run `tclass.getDeclaredField(fieldName)`
/// inside `try { .. } catch (Exception ex) { throw new RuntimeException(ex); }`,
/// and `getDeclaredField` opens with `Objects.requireNonNull(name)`. So a
/// null `fieldName` is `RuntimeException(new NullPointerException())` for
/// all three factories, and it wins over a null `vclass`; a null `vclass`
/// is reached only after the `try` -- a missing field is still
/// `RuntimeException(NoSuchFieldException)` -- and is the bare CCE.
#[test]
fn r21_w7_updaters_null_vclass_and_null_field_name_answers() {
    if !field_updater_jdk_rules() {
        return;
    }
    let null_name_shape = |um: &UpdaterMock, err: &MethodCallFailed| {
        matches!(err, MethodCallFailed::ExceptionThrown(_))
            && um.allocated().ends_with(&[
                "java/lang/NullPointerException".to_string(),
                "java/lang/RuntimeException".to_string(),
            ])
    };
    // ARFU, null fieldName (vclass present, and vclass null too).
    for null_vclass in [false, true] {
        let mut um = UpdaterMock::new();
        let (tmirror, _) = make_class_mirror_um(&mut um, "p/T");
        let (vmirror, _) = make_class_mirror_um(&mut um, "p/V");
        let v = if null_vclass { None } else { Some(vmirror) };
        let err = native_arfu_new_updater(
            &mut um,
            &[Value::Object(Some(tmirror)), Value::Object(v), Value::Object(None)],
        )
        .unwrap_err();
        assert!(null_name_shape(&um, &err), "ARFU null fieldName (null vclass {null_vclass}), got {err:?}");
    }
    // AIFU / ALFU, null fieldName.
    for long in [false, true] {
        let mut um = UpdaterMock::new();
        let (tmirror, _) = make_class_mirror_um(&mut um, "p/T");
        let args = [Value::Object(Some(tmirror)), Value::Object(None)];
        let err = if long {
            native_alfu_new_updater(&mut um, &args)
        } else {
            native_aifu_new_updater(&mut um, &args)
        }
        .unwrap_err();
        assert!(null_name_shape(&um, &err), "null fieldName (long {long}), got {err:?}");
    }
    // ARFU, null vclass on a valid field (reference and primitive): CCE.
    for (field, desc) in [("v", "Lp/V;"), ("i", "I")] {
        let mut um = UpdaterMock::new();
        let (tmirror, tcid) = make_class_mirror_um(&mut um, "p/T");
        um.add_field(tcid, field, desc, 0x0040, 0, false);
        let name = um.create_string(field);
        let err = native_arfu_new_updater(
            &mut um,
            &[Value::Object(Some(tmirror)), Value::Object(None), Value::Object(Some(name))],
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                    RuntimeError::ClassCastException { ref message }
                )) if message.is_empty()
            ),
            "null vclass on {field}:{desc}, got {err:?}"
        );
    }
    // ARFU, null vclass on a missing field: the `try` answers first.
    let mut um = UpdaterMock::new();
    let (tmirror, _) = make_class_mirror_um(&mut um, "p/T");
    let name = um.create_string("nope");
    let err = native_arfu_new_updater(
        &mut um,
        &[Value::Object(Some(tmirror)), Value::Object(None), Value::Object(Some(name))],
    )
    .unwrap_err();
    assert!(
        matches!(err, MethodCallFailed::ExceptionThrown(_))
            && um.allocated().ends_with(&[
                "java/lang/NoSuchFieldException".to_string(),
                "java/lang/RuntimeException".to_string(),
            ]),
        "null vclass on a missing field, got {err:?}"
    );
}

#[test]
fn t19_h5_arfu_new_updater_static_field_throws_iae() {
    let mut um = UpdaterMock::new();
    let (tmirror, tcid) = make_class_mirror_um(&mut um, "Target");
    let (vmirror, _) = make_class_mirror_um(&mut um, "java/lang/Object");
    um.add_field(tcid, "stat", "Ljava/lang/Object;", 0x0008, 0, true);
    let name = um.create_string("stat");
    let err = native_arfu_new_updater(
        &mut um,
        &[
            Value::Object(Some(tmirror)),
            Value::Object(Some(vmirror)),
            Value::Object(Some(name)),
        ],
    )
    .unwrap_err();
    assert!(matches!(
        err,
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            RuntimeError::IllegalArgumentException { .. }
        ))
    ));
}

#[test]
fn t19_h5_arfu_new_updater_final_field_throws_iae() {
    let mut um = UpdaterMock::new();
    let (tmirror, tcid) = make_class_mirror_um(&mut um, "Target");
    let (vmirror, _) = make_class_mirror_um(&mut um, "java/lang/Object");
    um.add_field(tcid, "fin", "Ljava/lang/Object;", 0x0010, 0, false);
    let name = um.create_string("fin");
    let err = native_arfu_new_updater(
        &mut um,
        &[
            Value::Object(Some(tmirror)),
            Value::Object(Some(vmirror)),
            Value::Object(Some(name)),
        ],
    )
    .unwrap_err();
    assert!(matches!(
        err,
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            RuntimeError::IllegalArgumentException { .. }
        ))
    ));
}

// -----------------------------------------------------------------
// T19.H5: AIFU / ALFU happy + error paths
// -----------------------------------------------------------------

#[test]
fn t19_h5_aifu_new_updater_happy_path_int_field() {
    let mut um = UpdaterMock::new();
    let (tmirror, tcid) = make_class_mirror_um(&mut um, "Target");
    um.add_field(tcid, "counter", "I", 0x0040, 3, false);
    let name = um.create_string("counter");
    let res = native_aifu_new_updater(
        &mut um,
        &[Value::Object(Some(tmirror)), Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap();
    let impl_obj = match res {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    assert_eq!(um.get_field(impl_obj, FU_SLOT_FIELD_INDEX), Value::Int(3));
    assert_eq!(
        um.get_field(impl_obj, FU_SLOT_DESC_TAG),
        Value::Int(DESC_TAG_INT)
    );
}

#[test]
fn t19_h5_aifu_new_updater_long_field_rejected() {
    let mut um = UpdaterMock::new();
    let (tmirror, tcid) = make_class_mirror_um(&mut um, "Target");
    // ACC_VOLATILE, and this one MATTERS: the assertion below only checks
    // for `IllegalArgumentException`, and "field must be volatile" is also
    // an IAE. With a non-volatile fixture this test went green for a
    // reason that has nothing to do with the long/int descriptor split it
    // is named for.
    um.add_field(tcid, "v", "J", 0x0040, 0, false);
    let name = um.create_string("v");
    let err = native_aifu_new_updater(
        &mut um,
        &[Value::Object(Some(tmirror)), Value::Object(Some(name))],
    )
    .unwrap_err();
    assert!(matches!(
        err,
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            RuntimeError::IllegalArgumentException { .. }
        ))
    ));
}

#[test]
fn t19_h5_alfu_new_updater_happy_path_long_field() {
    let mut um = UpdaterMock::new();
    let (tmirror, tcid) = make_class_mirror_um(&mut um, "Target");
    um.add_field(tcid, "ts", "J", 0x0040, 5, false);
    let name = um.create_string("ts");
    let res = native_alfu_new_updater(
        &mut um,
        &[Value::Object(Some(tmirror)), Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap();
    let impl_obj = match res {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    assert_eq!(um.get_field(impl_obj, FU_SLOT_FIELD_INDEX), Value::Int(5));
    assert_eq!(
        um.get_field(impl_obj, FU_SLOT_DESC_TAG),
        Value::Int(DESC_TAG_LONG)
    );
}

// -----------------------------------------------------------------
// T19.H5: Accessor round-trips (compareAndSet / getAndSet / get / set)
// -----------------------------------------------------------------

#[test]
fn t19_h5_arfu_compare_and_set_round_trip() {
    let mut um = UpdaterMock::new();
    let (tmirror, tcid) = make_class_mirror_um(&mut um, "Target");
    let (vmirror, _) = make_class_mirror_um(&mut um, "java/lang/Object");
    um.add_field(tcid, "next", "Ljava/lang/Object;", 0x0040, 1, false);
    let name = um.create_string("next");
    let updater = match native_arfu_new_updater(
        &mut um,
        &[
            Value::Object(Some(tmirror)),
            Value::Object(Some(vmirror)),
            Value::Object(Some(name)),
        ],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    // Allocate a fake Target instance with 4 slots (slot 1 is the field).
    let target = um.alloc_object(tcid, 4);
    // Initially Object(None) (default).  CAS from None → some value.
    let some_value_obj = um.create_string("hello");
    let ok = native_arfu_compare_and_set(
        &mut um,
        &[
            Value::Object(Some(updater)),
            Value::Object(Some(target)),
            Value::Int(0), // expected: not Object(None) — see below
            Value::Object(Some(some_value_obj)),
        ],
    )
    .unwrap();
    // Default field for slot 1 in MockNativeContext is Value::Int(0),
    // so the CAS expected=Int(0) succeeds and writes the new value.
    assert_eq!(ok, Some(Value::Int(1)));
    let post = native_arfu_get(
        &mut um,
        &[Value::Object(Some(updater)), Value::Object(Some(target))],
    )
    .unwrap()
    .unwrap();
    match post {
        Value::Object(Some(s)) => {
            assert_eq!(um.read_string(s).as_deref(), Some("hello"));
        }
        other => panic!("expected new string, got {other:?}"),
    }
}

#[test]
fn t19_h5_aifu_get_and_add_round_trip() {
    let mut um = UpdaterMock::new();
    let (tmirror, tcid) = make_class_mirror_um(&mut um, "Counter");
    um.add_field(tcid, "n", "I", 0x0040, 0, false);
    let name = um.create_string("n");
    let updater = match native_aifu_new_updater(
        &mut um,
        &[Value::Object(Some(tmirror)), Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    let target = um.alloc_object(tcid, 1);
    // default slot 0 is Int(0); add 5 — result is current 0, slot now 5.
    let prev = native_aifu_get_and_add(
        &mut um,
        &[
            Value::Object(Some(updater)),
            Value::Object(Some(target)),
            Value::Int(5),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(prev, Value::Int(0));
    let after = native_aifu_get(
        &mut um,
        &[Value::Object(Some(updater)), Value::Object(Some(target))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(after, Value::Int(5));
}

#[test]
fn t19_h5_alfu_set_and_get_round_trip() {
    let mut um = UpdaterMock::new();
    let (tmirror, tcid) = make_class_mirror_um(&mut um, "TimestampHolder");
    um.add_field(tcid, "ts", "J", 0x0040, 0, false);
    let name = um.create_string("ts");
    let updater = match native_alfu_new_updater(
        &mut um,
        &[Value::Object(Some(tmirror)), Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    let target = um.alloc_object(tcid, 1);
    native_alfu_set(
        &mut um,
        &[
            Value::Object(Some(updater)),
            Value::Object(Some(target)),
            Value::Long(1234567890123),
        ],
    )
    .unwrap();
    let got = native_alfu_get(
        &mut um,
        &[Value::Object(Some(updater)), Value::Object(Some(target))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(got, Value::Long(1234567890123));
}

#[test]
fn t19_h5_aifu_compare_and_set_observable_progress() {
    // Bench-style: 4 sequential increments must produce monotonically
    // increasing observed values.  Simulates the multi-thread case
    // without spawning real threads (UpdaterMock is single-threaded).
    let mut um = UpdaterMock::new();
    let (tmirror, tcid) = make_class_mirror_um(&mut um, "Counter");
    um.add_field(tcid, "n", "I", 0x0040, 0, false);
    let name = um.create_string("n");
    let updater = match native_aifu_new_updater(
        &mut um,
        &[Value::Object(Some(tmirror)), Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    let target = um.alloc_object(tcid, 1);
    for expected in 1..=4 {
        let r = native_aifu_increment_and_get(
            &mut um,
            &[Value::Object(Some(updater)), Value::Object(Some(target))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(r, Value::Int(expected));
    }
}

#[test]
fn t19_h5_arfu_get_and_set_replaces_value() {
    let mut um = UpdaterMock::new();
    let (tmirror, tcid) = make_class_mirror_um(&mut um, "Holder");
    let (vmirror, _) = make_class_mirror_um(&mut um, "java/lang/Object");
    um.add_field(tcid, "ref", "Ljava/lang/Object;", 0x0040, 0, false);
    let name = um.create_string("ref");
    let updater = match native_arfu_new_updater(
        &mut um,
        &[
            Value::Object(Some(tmirror)),
            Value::Object(Some(vmirror)),
            Value::Object(Some(name)),
        ],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    let target = um.alloc_object(tcid, 1);
    // Pre-populate slot 0 with a marker ref.
    let first = um.create_string("first");
    um.set_field(target, 0, Value::Object(Some(first)));
    let next = um.create_string("second");
    let prev = native_arfu_get_and_set(
        &mut um,
        &[
            Value::Object(Some(updater)),
            Value::Object(Some(target)),
            Value::Object(Some(next)),
        ],
    )
    .unwrap()
    .unwrap();
    match prev {
        Value::Object(Some(s)) => {
            assert_eq!(um.read_string(s).as_deref(), Some("first"));
        }
        _ => panic!(),
    }
    match um.get_field(target, 0) {
        Value::Object(Some(s)) => {
            assert_eq!(um.read_string(s).as_deref(), Some("second"));
        }
        _ => panic!(),
    }
}

#[test]
fn t19_h5_null_target_returns_cce_not_silent() {
    let mut um = UpdaterMock::new();
    let (tmirror, tcid) = make_class_mirror_um(&mut um, "Target");
    let (vmirror, _) = make_class_mirror_um(&mut um, "java/lang/Object");
    um.add_field(tcid, "f", "Ljava/lang/Object;", 0x0040, 0, false);
    let name = um.create_string("f");
    let updater = match native_arfu_new_updater(
        &mut um,
        &[
            Value::Object(Some(tmirror)),
            Value::Object(Some(vmirror)),
            Value::Object(Some(name)),
        ],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    // Pass null as target — must throw, and specifically a
    // ClassCastException.
    //
    // CHANGED 2026-08-27, from NullPointerException. This test's NAME is
    // still exactly right — the point is that a null target is not silently
    // absorbed — but the TYPE it asserted was this VM's, not the JDK's.
    //
    // MEASURED against HotSpot 25.0.3+9 (`probes/AtomicUpdaterSweep.java`):
    //
    //   u.get(null)   HotSpot ClassCastException   CratonVM (then) NPE
    //
    // The JDK's accessors open with `if (!tclass.isInstance(obj)) throw new
    // ClassCastException()`, and `isInstance` is FALSE for null — so a null
    // target leaves by the same door as a wrong-typed one, and never
    // reaches a dereference. A caller catching CCE around a field-updater
    // access is catching the documented type; NPE escaped it.
    let err = native_arfu_get(
        &mut um,
        &[Value::Object(Some(updater)), Value::Object(None)],
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::ClassCastException { .. }
            ))
        ),
        "null target must be a ClassCastException like the JDK's, got {err:?}"
    );
}

#[test]
fn t19_h5_descriptor_to_internal_name_only_for_pure_class_descriptor() {
    assert_eq!(
        ref_descriptor_to_internal_name("Ljava/lang/Object;"),
        Some("java/lang/Object".to_string())
    );
    assert_eq!(ref_descriptor_to_internal_name("[I"), None);
    assert_eq!(ref_descriptor_to_internal_name("I"), None);
}

// -----------------------------------------------------------------
// getAndIncrement / getAndDecrement / addAndGet return semantics.
//
// These accessors previously used a bounded CAS loop that, on
// contended loop exhaustion, returned a fabricated `0` and applied
// NO update. They now delegate to the unbounded `atomic_fetch_add_*`
// primitive. getAnd* must return the OLD value (and bump the slot);
// addAndGet must return the NEW value. The tests assert both the
// return value and the committed slot value so a regression to the
// bogus-0 fallthrough (which left the slot unchanged) would fail.
// -----------------------------------------------------------------

/// Build an int updater over field `n` (slot 0) of a fresh `Counter`
/// instance and return `(um, updater, target)`.
fn int_updater_over_slot0(start: i32) -> (UpdaterMock, ObjectRef, ObjectRef) {
    let mut um = UpdaterMock::new();
    let (tmirror, tcid) = make_class_mirror_um(&mut um, "Counter");
    um.add_field(tcid, "n", "I", 0x0040, 0, false);
    let name = um.create_string("n");
    let updater = match native_aifu_new_updater(
        &mut um,
        &[Value::Object(Some(tmirror)), Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    let target = um.alloc_object(tcid, 1);
    um.set_field(target, 0, Value::Int(start));
    (um, updater, target)
}

/// Build a long updater over field `v` (slot 0) of a fresh `LongHolder`.
fn long_updater_over_slot0(start: i64) -> (UpdaterMock, ObjectRef, ObjectRef) {
    let mut um = UpdaterMock::new();
    let (tmirror, tcid) = make_class_mirror_um(&mut um, "LongHolder");
    um.add_field(tcid, "v", "J", 0x0040, 0, false);
    let name = um.create_string("v");
    let updater = match native_alfu_new_updater(
        &mut um,
        &[Value::Object(Some(tmirror)), Value::Object(Some(name))],
    )
    .unwrap()
    .unwrap()
    {
        Value::Object(Some(o)) => o,
        _ => panic!(),
    };
    let target = um.alloc_object(tcid, 1);
    um.set_field(target, 0, Value::Long(start));
    (um, updater, target)
}

#[test]
fn t19_h5_aifu_get_and_increment_returns_old_and_bumps_slot() {
    let (mut um, updater, target) = int_updater_over_slot0(41);
    let ret = native_aifu_get_and_increment(
        &mut um,
        &[Value::Object(Some(updater)), Value::Object(Some(target))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ret, Value::Int(41), "getAndIncrement returns the OLD value");
    assert_eq!(um.get_field(target, 0), Value::Int(42), "slot is bumped");
}

#[test]
fn t19_h5_aifu_get_and_decrement_returns_old_and_bumps_slot() {
    let (mut um, updater, target) = int_updater_over_slot0(7);
    let ret = native_aifu_get_and_decrement(
        &mut um,
        &[Value::Object(Some(updater)), Value::Object(Some(target))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ret, Value::Int(7), "getAndDecrement returns the OLD value");
    assert_eq!(
        um.get_field(target, 0),
        Value::Int(6),
        "slot is decremented"
    );
}

#[test]
fn t19_h5_aifu_add_and_get_returns_new_value() {
    let (mut um, updater, target) = int_updater_over_slot0(100);
    let ret = native_aifu_add_and_get(
        &mut um,
        &[
            Value::Object(Some(updater)),
            Value::Object(Some(target)),
            Value::Int(-30),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ret, Value::Int(70), "addAndGet returns the NEW value");
    assert_eq!(
        um.get_field(target, 0),
        Value::Int(70),
        "slot holds new value"
    );
}

#[test]
fn t19_h5_alfu_get_and_increment_returns_old_and_bumps_slot() {
    let (mut um, updater, target) = long_updater_over_slot0(9_000_000_000);
    let ret = native_alfu_get_and_increment(
        &mut um,
        &[Value::Object(Some(updater)), Value::Object(Some(target))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        ret,
        Value::Long(9_000_000_000),
        "getAndIncrement returns the OLD value"
    );
    assert_eq!(um.get_field(target, 0), Value::Long(9_000_000_001));
}

#[test]
fn t19_h5_alfu_get_and_decrement_returns_old_and_bumps_slot() {
    let (mut um, updater, target) = long_updater_over_slot0(1);
    let ret = native_alfu_get_and_decrement(
        &mut um,
        &[Value::Object(Some(updater)), Value::Object(Some(target))],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ret, Value::Long(1), "getAndDecrement returns the OLD value");
    assert_eq!(um.get_field(target, 0), Value::Long(0));
}

#[test]
fn t19_h5_alfu_add_and_get_returns_new_value() {
    let (mut um, updater, target) = long_updater_over_slot0(1_000);
    let ret = native_alfu_add_and_get(
        &mut um,
        &[
            Value::Object(Some(updater)),
            Value::Object(Some(target)),
            Value::Long(250),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(ret, Value::Long(1_250), "addAndGet returns the NEW value");
    assert_eq!(um.get_field(target, 0), Value::Long(1_250));
}

#[test]
fn t19_h5_int_accessors_accumulate_across_calls() {
    // Sequential calls must accumulate (no fabricated-0 reset): the
    // old bounded-loop fallthrough would have lost updates and broken
    // this monotonic progression under contention.
    let (mut um, updater, target) = int_updater_over_slot0(0);
    for i in 0..5 {
        let old = native_aifu_get_and_increment(
            &mut um,
            &[Value::Object(Some(updater)), Value::Object(Some(target))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            old,
            Value::Int(i),
            "each getAndIncrement returns prior count"
        );
    }
    assert_eq!(um.get_field(target, 0), Value::Int(5));
}
