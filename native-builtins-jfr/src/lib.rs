#![cfg_attr(not(test), deny(unsafe_code))]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(clippy::all)]
#![allow(deprecated)]

pub mod jfr;
pub mod jfr_backend;
pub mod jfr_chunk;
pub mod jfr_meta;
pub mod jfr_vm_events;
pub mod jfr_vm_more;

pub fn install_hooks() {
    cratonvm_native_builtins::hooks::install_jfr_hooks(cratonvm_native_builtins::hooks::JfrHooks {
        any_recording: jfr_vm_events::any_recording,
        throwable_created: jfr_vm_events::throwable_created,
        thread_sleep_begin: jfr_vm_events::thread_sleep_begin,
        thread_sleep_end: jfr_vm_events::thread_sleep_end,
        force_gc_as_system_gc: jfr_vm_more::force_gc_as_system_gc,
        forget_jfr_thread_keys: jfr::forget_jfr_thread_keys,
        forget_vm_jfr_state: jfr::forget_vm_jfr_state,
        register_jfr_natives: jfr::register_jfr_natives,
    });
}

mod raw_access;
