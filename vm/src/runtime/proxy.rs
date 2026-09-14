// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP2.5 — Dynamic `java.lang.reflect.Proxy.newProxyInstance` support.
//!
//! ## Strategy (B+: enhanced synthetic Proxy$Instance)
//!
//! WP2.3 (`Unsafe.defineClass` / `Lookup.defineClass`) is in-flight in
//! parallel — Strategy A (real bytecode generation per JDK
//! `ProxyGenerator`) would block on it. Instead, we extend the existing
//! synthetic `java/lang/reflect/Proxy$Instance` class with proper
//! interface tracking so that:
//!
//!   * `proxy.getClass().getInterfaces()` returns the original interfaces
//!     array passed to `newProxyInstance`.
//!   * `Proxy.isProxyClass(proxy.getClass())` returns `true`.
//!   * Multi-interface proxies dispatch every interface method through
//!     the registered `InvocationHandler`.
//!   * Default-method invocations are routed through the handler (the
//!     handler may explicitly invoke the default body via
//!     `InvocationHandler.invokeDefault` — that JDK-internal call lands
//!     here too).
//!
//! ## Field layout of `java/lang/reflect/Proxy$Instance`
//!
//! | slot | content                                                 |
//! |------|---------------------------------------------------------|
//! |  0   | `InvocationHandler` reference (Object)                  |
//! |  1   | `Class[]` interfaces array (Object)                     |
//! |  2   | identity-hashcode override (Int) — reserved             |
//!
//! When the handler is invoked we synthesize a `java.lang.reflect.Method`
//! object with `name`, `descriptor`, `parameter_count` and pass it as
//! the second argument to `InvocationHandler.invoke(Object, Method,
//! Object[])`. The first argument is the proxy itself; primitives in the
//! arguments are auto-boxed.
//!
//! When WP2.3 lands, this module can flip to Strategy A by emitting a
//! real class file and registering it with the class manager — the
//! callers (the `invokevirtual` interceptor in `interpreter.rs`, the
//! `proxy_invoke_handler_shared` in `vm_exec.rs`, and the natives in
//! `lib.rs::register_reflect_proxy_natives`) all sit on the
//! `Proxy$Instance` synthetic class name and will continue to work as a
//! stop-gap.

/// The synthetic class name we use for every proxy instance.
///
/// All proxies, regardless of their interface set, currently land on
/// this single class — see the module doc-comment for the rationale.
/// A future Strategy-A implementation would emit a unique class per
/// interface set (e.g. `java/lang/reflect/$Proxy0`) and register them
/// with the class manager.
pub const PROXY_INSTANCE_CLASS: &str = "java/lang/reflect/Proxy$Instance";

/// Slot index of the `InvocationHandler` reference on a proxy.
pub const PROXY_FIELD_HANDLER: usize = 0;

/// Slot index of the `Class[]` interfaces array on a proxy.
pub const PROXY_FIELD_INTERFACES: usize = 1;

/// Slot index of an optional identity-hashcode override (Int) on a
/// proxy. Reserved for a later WP that wants to honour
/// `InvocationHandler` returning a custom hashcode for `Object.hashCode`
/// dispatch — currently unused.
pub const PROXY_FIELD_IDENTITY_HASH: usize = 2;

/// Total number of slots in a `Proxy$Instance` heap object.
pub const PROXY_INSTANCE_FIELD_COUNT: usize = 3;

/// Returns `true` iff the given class name names a synthetic proxy
/// class. Used by `java.lang.reflect.Proxy.isProxyClass(Class)` and by
/// the cast-compatibility check that lets a proxy satisfy any interface
/// downcast.
#[inline]
pub fn is_proxy_class_name(name: &str) -> bool {
    name == PROXY_INSTANCE_CLASS
}

/// Count the parameters in a method descriptor like
/// `(ILjava/lang/String;[I)V`. Returns the JVM-spec parameter count
/// (long/double count as 1, matching `Method.getParameterCount()` and
/// matching the JDK behavior for proxy dispatch).
///
/// Mirrors `vm_exec.rs::proxy_count_params` — duplicated here so that
/// the new `proxy.rs` module is self-contained for unit tests.
pub fn count_descriptor_params(descriptor: &str) -> usize {
    let inner = match descriptor.find('(') {
        Some(start) => match descriptor.find(')') {
            Some(end) if end > start => &descriptor[start + 1..end],
            _ => return 0,
        },
        None => return 0,
    };

    let mut count = 0usize;
    let mut chars = inner.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            'B' | 'C' | 'D' | 'F' | 'I' | 'J' | 'S' | 'Z' => count += 1,
            'L' => {
                count += 1;
                for c in chars.by_ref() {
                    if c == ';' {
                        break;
                    }
                }
            }
            '[' => {
                // array prefix — element type follows; the next iteration
                // counts it.
            }
            _ => {
                // Tolerant — unknown descriptors just don't increment.
            }
        }
    }
    count
}

/// Classification of a method name that needs special handling on a
/// proxy. The dispatch routes for `Object.hashCode/equals/toString` are
/// usually delegated to the handler, but the JDK `Proxy` machinery
/// short-circuits a few cases — we mirror that policy here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyObjectMethod {
    /// `getClass()Ljava/lang/Class;` — short-circuited; the handler is
    /// not consulted.
    GetClass,
    /// `hashCode()I` — by spec, dispatched through the handler. Some
    /// callers (e.g. JDK internal collections) expect identity-based
    /// hashing as the default — we still go through the handler so
    /// userland code can override.
    HashCode,
    /// `equals(Ljava/lang/Object;)Z` — dispatched through the handler.
    Equals,
    /// `toString()Ljava/lang/String;` — dispatched through the handler.
    ToString,
    /// Any other method — interface method, dispatched through handler.
    InterfaceMethod,
}

impl ProxyObjectMethod {
    /// Classify the (name, descriptor) pair.
    pub fn classify(name: &str, descriptor: &str) -> Self {
        match (name, descriptor) {
            ("getClass", "()Ljava/lang/Class;") => Self::GetClass,
            ("hashCode", "()I") => Self::HashCode,
            ("equals", "(Ljava/lang/Object;)Z") => Self::Equals,
            ("toString", "()Ljava/lang/String;") => Self::ToString,
            _ => Self::InterfaceMethod,
        }
    }

    /// Returns true iff the dispatch should bypass the handler entirely.
    /// Currently only `getClass` short-circuits.
    pub fn should_bypass_handler(self) -> bool {
        matches!(self, Self::GetClass)
    }
}

/// Diagnostics counters incremented from the dispatch path. The
/// dispatcher in `interpreter.rs` already runs in a hot loop; we keep
/// these as plain `AtomicUsize`s so test code can read them without
/// taking a lock.
pub mod stats {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static PROXY_INSTANCES_CREATED: AtomicUsize = AtomicUsize::new(0);
    static PROXY_DISPATCHES: AtomicUsize = AtomicUsize::new(0);

    /// Bump the "proxy instances created" counter. Called from the
    /// `Proxy.newProxyInstance` native.
    #[inline]
    pub fn inc_instances_created() {
        PROXY_INSTANCES_CREATED.fetch_add(1, Ordering::Relaxed);
    }

    /// Bump the "proxy dispatches handled" counter. Called from the
    /// `proxy_invoke_handler` path.
    #[inline]
    pub fn inc_dispatches() {
        PROXY_DISPATCHES.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot of the instances-created counter (for tests).
    pub fn instances_created() -> usize {
        PROXY_INSTANCES_CREATED.load(Ordering::Relaxed)
    }

    /// Snapshot of the dispatches counter (for tests).
    pub fn dispatches() -> usize {
        PROXY_DISPATCHES.load(Ordering::Relaxed)
    }
}

// NOTE: the global last-proxy-interfaces tracker that powers the
// `proxy.getClass().getInterfaces()` round-trip lives in
// `native-builtins/src/lib.rs::PROXY_LAST_INTERFACES_BITS`. We can't
// host it here because `native-builtins` does not depend on
// `cratonvm-vm` (the dependency goes the other way), so the cache must
// live where both the writer (`native_proxy_new_instance`) and the
// reader (`native_class_get_interfaces`) can reach it.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_class_name_constant() {
        assert_eq!(PROXY_INSTANCE_CLASS, "java/lang/reflect/Proxy$Instance");
    }

    #[test]
    fn is_proxy_class_name_recognizes_synthetic_class() {
        assert!(is_proxy_class_name("java/lang/reflect/Proxy$Instance"));
        assert!(!is_proxy_class_name("java/lang/Object"));
        assert!(!is_proxy_class_name(""));
        assert!(!is_proxy_class_name("Proxy$Instance"));
    }

    #[test]
    fn field_layout_is_consistent() {
        // Slot indices must be unique and < FIELD_COUNT.
        assert!(PROXY_FIELD_HANDLER < PROXY_INSTANCE_FIELD_COUNT);
        assert!(PROXY_FIELD_INTERFACES < PROXY_INSTANCE_FIELD_COUNT);
        assert!(PROXY_FIELD_IDENTITY_HASH < PROXY_INSTANCE_FIELD_COUNT);
        assert_ne!(PROXY_FIELD_HANDLER, PROXY_FIELD_INTERFACES);
        assert_ne!(PROXY_FIELD_HANDLER, PROXY_FIELD_IDENTITY_HASH);
        assert_ne!(PROXY_FIELD_INTERFACES, PROXY_FIELD_IDENTITY_HASH);
    }

    #[test]
    fn count_descriptor_params_voids() {
        assert_eq!(count_descriptor_params("()V"), 0);
        assert_eq!(count_descriptor_params("()I"), 0);
        assert_eq!(count_descriptor_params("()Ljava/lang/String;"), 0);
    }

    #[test]
    fn count_descriptor_params_primitives() {
        assert_eq!(count_descriptor_params("(I)V"), 1);
        assert_eq!(count_descriptor_params("(IJ)V"), 2);
        assert_eq!(count_descriptor_params("(IJDFZBCS)V"), 8);
    }

    #[test]
    fn count_descriptor_params_objects_and_arrays() {
        assert_eq!(count_descriptor_params("(Ljava/lang/String;)V"), 1);
        assert_eq!(
            count_descriptor_params("(Ljava/lang/String;Ljava/lang/Object;)V"),
            2
        );
        assert_eq!(count_descriptor_params("([I)V"), 1);
        assert_eq!(count_descriptor_params("([[I)V"), 1);
        assert_eq!(count_descriptor_params("([Ljava/lang/String;)V"), 1);
    }

    #[test]
    fn count_descriptor_params_mixed() {
        assert_eq!(count_descriptor_params("(ILjava/lang/String;[J)V"), 3);
    }

    #[test]
    fn count_descriptor_params_malformed() {
        assert_eq!(count_descriptor_params(""), 0);
        assert_eq!(count_descriptor_params("()"), 0);
        // missing closing paren — we return 0 rather than panic
        assert_eq!(count_descriptor_params("(I"), 0);
    }

    #[test]
    fn classify_object_methods() {
        assert_eq!(
            ProxyObjectMethod::classify("getClass", "()Ljava/lang/Class;"),
            ProxyObjectMethod::GetClass
        );
        assert_eq!(
            ProxyObjectMethod::classify("hashCode", "()I"),
            ProxyObjectMethod::HashCode
        );
        assert_eq!(
            ProxyObjectMethod::classify("equals", "(Ljava/lang/Object;)Z"),
            ProxyObjectMethod::Equals
        );
        assert_eq!(
            ProxyObjectMethod::classify("toString", "()Ljava/lang/String;"),
            ProxyObjectMethod::ToString
        );
    }

    #[test]
    fn classify_interface_method() {
        assert_eq!(
            ProxyObjectMethod::classify("hello", "(Ljava/lang/String;)Ljava/lang/String;"),
            ProxyObjectMethod::InterfaceMethod
        );
    }

    #[test]
    fn classify_disambiguates_by_descriptor() {
        // hashCode with a non-spec descriptor is treated as an interface
        // method (so hash-collision-free dispatch).
        assert_eq!(
            ProxyObjectMethod::classify("hashCode", "(I)I"),
            ProxyObjectMethod::InterfaceMethod
        );
    }

    #[test]
    fn should_bypass_handler() {
        assert!(ProxyObjectMethod::GetClass.should_bypass_handler());
        assert!(!ProxyObjectMethod::HashCode.should_bypass_handler());
        assert!(!ProxyObjectMethod::Equals.should_bypass_handler());
        assert!(!ProxyObjectMethod::ToString.should_bypass_handler());
        assert!(!ProxyObjectMethod::InterfaceMethod.should_bypass_handler());
    }

    #[test]
    fn stats_counters_are_monotonic() {
        let before = stats::instances_created();
        stats::inc_instances_created();
        let after = stats::instances_created();
        assert_eq!(after, before + 1);

        let before_d = stats::dispatches();
        stats::inc_dispatches();
        let after_d = stats::dispatches();
        assert_eq!(after_d, before_d + 1);
    }
}
