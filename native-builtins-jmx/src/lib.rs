#![cfg_attr(not(test), deny(unsafe_code))]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(clippy::all)]
#![allow(deprecated)]

#[cfg(feature = "management")]
pub mod jmx;
#[cfg(feature = "management")]
pub mod jmx_openmbean;

pub fn install_hooks() {
    #[cfg(feature = "management")]
    cratonvm_native_builtins::hooks::install_jmx_hooks(cratonvm_native_builtins::hooks::JmxHooks {
        alloc_thread_mxbean: jmx::alloc_thread_mxbean,
        alloc_os_mxbean: jmx::alloc_os_mxbean,
        register_mbean_server: jmx::register_mbean_server,
        register_jmx_natives: jmx::register_jmx_natives,
        register_thread_impl: jmx::register_thread_impl,
        register_jmx_openmbean_natives: jmx_openmbean::register_jmx_openmbean_natives,
        build_method_mirror: jmx_openmbean::build_method_mirror,
        parse_method_descriptor_pub: jmx_openmbean::parse_method_descriptor_pub,
        type_descriptor_to_class_mirror_pub: jmx_openmbean::type_descriptor_to_class_mirror_pub,
    });
}
