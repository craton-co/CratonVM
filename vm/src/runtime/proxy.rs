// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Layout constants of the VM's synthetic proxy super `java/lang/reflect/Proxy$Instance`
//! (the fallback shape `Proxy.newProxyInstance` degrades to when a generated `$ProxyN`
//! cannot be defined) and a descriptor parameter counter the proxy doors share.
//!
//! This module no longer decides what a proxy is. The predicates are
//! `cratonvm_native_builtins::reflect_annotations::proxy_class_is_generated` (`isProxyClass`,
//! `getInvocationHandler`), `proxy_name_has_generated_shape` (name screens) and
//! `runtime::interpreter::typecheck::class_is_generated_proxy_or_shim` (cast / `aastore` /
//! dispatch). Generated proxies carry real bodies (`classloading::proxy_gen`) that call
//! `Proxy$Dispatch.invokeProxy`, in both tiers.
//!
//! | slot | content                                  |
//! |------|------------------------------------------|
//! |  0   | `InvocationHandler` reference (Object)   |
//! |  1   | `Class[]` interfaces array (Object)      |
//! |  2   | reserved (Int)                           |
//!
//! Round 13 wave 8 (lane proxy5) removed the WP2.5 stop-gap half of this module
//! (a shim-name predicate, an Object-method classifier and two census counters),
//! none of which had a production caller; the counters read 0 in every census.

/// The synthetic proxy super / fallback proxy class (see the module doc).
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

/// Count the parameters in a method descriptor like
/// `(ILjava/lang/String;[I)V`. Returns the JVM-spec parameter count
/// (long/double count as 1, matching `Method.getParameterCount()` and
/// matching the JDK behavior for proxy dispatch).
///
/// Mirrors `vm_exec.rs::proxy_count_params` — duplicated here so that
/// the new `proxy.rs` module is self-contained for unit tests.
///
/// The parameter list ends at the first `)` OUTSIDE a class name: a JVM class
/// name may contain `)` (JVMS 4.2.1 forbids only `.`, `;`, `[` and `/`), so
/// `(LA)B;I)V` has two parameters, and cutting at the first `)` in the string
/// counted one (round 13 wave 9, lane proxy6). A descriptor with no closing
/// `)` counts 0, as before.
pub fn count_descriptor_params(descriptor: &str) -> usize {
    let Some(start) = descriptor.find('(') else {
        return 0;
    };
    let mut count = 0usize;
    let mut chars = descriptor[start + 1..].chars();
    while let Some(ch) = chars.next() {
        match ch {
            ')' => return count,
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
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_class_name_constant() {
        assert_eq!(PROXY_INSTANCE_CLASS, "java/lang/reflect/Proxy$Instance");
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
    fn count_descriptor_params_class_name_containing_a_paren() {
        assert_eq!(count_descriptor_params("(LA)B;I)V"), 2);
        assert_eq!(count_descriptor_params("([LA)B;)LC)D;"), 1);
    }
}
