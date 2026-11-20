use std::sync::OnceLock;
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, VmError};
use cratonvm_types::{ClassId, ObjectRef, Value};

pub type HookResult<T> = std::result::Result<T, VmError>;

pub struct JfrHooks {
    pub any_recording: fn() -> bool,
    pub throwable_created: fn(&mut dyn NativeContext, ObjectRef),
    pub thread_sleep_begin: fn(&mut dyn NativeContext, i64) -> usize,
    pub thread_sleep_end: fn(&mut dyn NativeContext, usize),
    pub force_gc_as_system_gc: fn(&mut dyn NativeContext),
    pub forget_jfr_thread_keys: fn(&[usize]),
    pub forget_vm_jfr_state: fn(usize),
    pub register_jfr_natives: fn(&mut NativeMethodRegistry),
}

pub struct JmxHooks {
    pub alloc_thread_mxbean: fn(&mut dyn NativeContext) -> std::result::Result<ObjectRef, MethodCallFailed>,
    pub alloc_os_mxbean: fn(&mut dyn NativeContext) -> std::result::Result<ObjectRef, MethodCallFailed>,
    pub register_mbean_server: fn(&mut NativeMethodRegistry),
    pub register_jmx_natives: fn(&mut NativeMethodRegistry),
    pub register_thread_impl: fn(&mut NativeMethodRegistry),
    pub register_jmx_openmbean_natives: fn(&mut NativeMethodRegistry),
    pub build_method_mirror: fn(&mut dyn NativeContext, ObjectRef, &str, &str, u16) -> std::result::Result<ObjectRef, MethodCallFailed>,
    pub parse_method_descriptor_pub: fn(&str) -> (Vec<String>, String),
    pub type_descriptor_to_class_mirror_pub: fn(&mut dyn NativeContext, &str) -> ObjectRef,
}

pub static JFR_HOOKS: OnceLock<JfrHooks> = OnceLock::new();
pub static JMX_HOOKS: OnceLock<JmxHooks> = OnceLock::new();

pub fn install_jfr_hooks(h: JfrHooks) { let _ = JFR_HOOKS.set(h); }
pub fn install_jmx_hooks(h: JmxHooks) { let _ = JMX_HOOKS.set(h); }

#[inline]
pub fn jfr_any_recording() -> bool {
    JFR_HOOKS.get().map_or(false, |h| (h.any_recording)())
}
#[inline]
pub fn jfr_throwable_created(ctx: &mut dyn NativeContext, v: ObjectRef) {
    if let Some(h) = JFR_HOOKS.get() { (h.throwable_created)(ctx, v); }
}
#[inline]
pub fn jfr_thread_sleep_begin(ctx: &mut dyn NativeContext, nanos: i64) -> usize {
    JFR_HOOKS.get().map_or(0, |h| (h.thread_sleep_begin)(ctx, nanos))
}
#[inline]
pub fn jfr_thread_sleep_end(ctx: &mut dyn NativeContext, root: usize) {
    if let Some(h) = JFR_HOOKS.get() { (h.thread_sleep_end)(ctx, root); }
}
#[inline]
pub fn jfr_force_gc_as_system_gc(ctx: &mut dyn NativeContext) {
    if let Some(h) = JFR_HOOKS.get() { (h.force_gc_as_system_gc)(ctx); } else { ctx.force_gc(); }
}
#[inline]
pub fn jfr_forget_jfr_thread_keys(keys: &[usize]) {
    if let Some(h) = JFR_HOOKS.get() { (h.forget_jfr_thread_keys)(keys); }
}
#[inline]
pub fn jfr_forget_vm_jfr_state(vm: usize) {
    if let Some(h) = JFR_HOOKS.get() { (h.forget_vm_jfr_state)(vm); }
}
#[inline]
pub fn jfr_register_jfr_natives(r: &mut NativeMethodRegistry) {
    if let Some(h) = JFR_HOOKS.get() { (h.register_jfr_natives)(r); }
}
