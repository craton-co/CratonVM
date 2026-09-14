// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use std::sync::Arc;

use crate::attribute::{Attribute, LazyAttribute};
use crate::class_access_flags::MethodAccessFlags;

/// A method in a Java class file (JVM spec 4.6).
///
/// `name` and `descriptor` are stored as shared `Arc<str>` so repeated
/// references to the same identifier across different classes share a
/// single backing allocation. The reader populates them via
/// [`cratonvm_types::intern_arc`] at parse time, so constructing a
/// `ClassFileMethod` from the class-file hot path is a single refcount
/// bump plus a hash-table lookup.
#[derive(Debug, Clone)]
pub struct ClassFileMethod {
    pub access_flags: MethodAccessFlags,
    pub name: Arc<str>,
    pub descriptor: Arc<str>,
    pub attributes: Vec<LazyAttribute>,
}

impl ClassFileMethod {
    pub fn is_static(&self) -> bool {
        self.access_flags.contains(MethodAccessFlags::STATIC)
    }

    pub fn is_native(&self) -> bool {
        self.access_flags.contains(MethodAccessFlags::NATIVE)
    }

    pub fn is_abstract(&self) -> bool {
        self.access_flags.contains(MethodAccessFlags::ABSTRACT)
    }

    pub fn is_synchronized(&self) -> bool {
        self.access_flags.contains(MethodAccessFlags::SYNCHRONIZED)
    }

    pub fn is_bridge(&self) -> bool {
        self.access_flags.contains(MethodAccessFlags::BRIDGE)
    }

    /// Returns the Code attribute, if present and already decoded.
    /// Lazy attributes must be force-decoded by the caller first.
    pub fn code(&self) -> Option<&crate::attribute::CodeAttribute> {
        self.attributes.iter().find_map(|a| match a.as_decoded() {
            Some(Attribute::Code(code)) => Some(code),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_method(flags: MethodAccessFlags) -> ClassFileMethod {
        ClassFileMethod {
            access_flags: flags,
            name: Arc::from("testMethod"),
            descriptor: Arc::from("()V"),
            attributes: Vec::new(),
        }
    }

    #[test]
    fn method_name_and_descriptor() {
        let m = ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC,
            name: Arc::from("main"),
            descriptor: Arc::from("([Ljava/lang/String;)V"),
            attributes: Vec::new(),
        };
        assert_eq!(&*m.name, "main");
        assert_eq!(&*m.descriptor, "([Ljava/lang/String;)V");
    }

    #[test]
    fn is_static_true() {
        let m = make_method(MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC);
        assert!(m.is_static());
    }

    #[test]
    fn is_static_false() {
        let m = make_method(MethodAccessFlags::PUBLIC);
        assert!(!m.is_static());
    }

    #[test]
    fn is_native_true() {
        let m = make_method(MethodAccessFlags::NATIVE);
        assert!(m.is_native());
    }

    #[test]
    fn is_native_false() {
        let m = make_method(MethodAccessFlags::PUBLIC);
        assert!(!m.is_native());
    }

    #[test]
    fn is_abstract_true() {
        let m = make_method(MethodAccessFlags::ABSTRACT);
        assert!(m.is_abstract());
    }

    #[test]
    fn is_abstract_false() {
        let m = make_method(MethodAccessFlags::PUBLIC);
        assert!(!m.is_abstract());
    }

    #[test]
    fn is_synchronized_true() {
        let m = make_method(MethodAccessFlags::SYNCHRONIZED);
        assert!(m.is_synchronized());
    }

    #[test]
    fn is_synchronized_false() {
        let m = make_method(MethodAccessFlags::PUBLIC);
        assert!(!m.is_synchronized());
    }

    #[test]
    fn combined_flags() {
        let m = make_method(
            MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC | MethodAccessFlags::SYNCHRONIZED,
        );
        assert!(m.is_static());
        assert!(m.is_synchronized());
        assert!(!m.is_native());
        assert!(!m.is_abstract());
    }

    #[test]
    fn code_returns_none_when_absent() {
        let m = make_method(MethodAccessFlags::PUBLIC);
        assert!(m.code().is_none());
    }

    #[test]
    fn code_returns_some_when_present() {
        let code_attr = crate::attribute::CodeAttribute {
            max_stack: 2,
            max_locals: 1,
            code: crate::byte_view::ByteView::from_vec(vec![0xb1]), // return
            exception_table: Vec::new(),
            attributes: Vec::new(),
        };
        let m = ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC,
            name: Arc::from("foo"),
            descriptor: Arc::from("()V"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::Code(code_attr))],
        };
        let code = m.code().expect("should have Code attribute");
        assert_eq!(code.max_stack, 2);
        assert_eq!(code.max_locals, 1);
        assert_eq!(code.code, vec![0xb1]);
    }
}
