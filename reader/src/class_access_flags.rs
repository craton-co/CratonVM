// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use bitflags::bitflags;

bitflags! {
    /// Access flags for a class or interface (JVM spec 4.1, Table 4.1-A).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ClassAccessFlags: u16 {
        const PUBLIC     = 0x0001;
        const FINAL      = 0x0010;
        const SUPER      = 0x0020;
        const INTERFACE  = 0x0200;
        const ABSTRACT   = 0x0400;
        const SYNTHETIC  = 0x1000;
        const ANNOTATION = 0x2000;
        const ENUM       = 0x4000;
        const MODULE     = 0x8000;
    }
}

bitflags! {
    /// Access flags for a field (JVM spec 4.5, Table 4.5-A).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FieldAccessFlags: u16 {
        const PUBLIC    = 0x0001;
        const PRIVATE   = 0x0002;
        const PROTECTED = 0x0004;
        const STATIC    = 0x0008;
        const FINAL     = 0x0010;
        const VOLATILE  = 0x0040;
        const TRANSIENT = 0x0080;
        const SYNTHETIC = 0x1000;
        const ENUM      = 0x4000;
    }
}

bitflags! {
    /// Access flags for a method (JVM spec 4.6, Table 4.6-A).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct MethodAccessFlags: u16 {
        const PUBLIC       = 0x0001;
        const PRIVATE      = 0x0002;
        const PROTECTED    = 0x0004;
        const STATIC       = 0x0008;
        const FINAL        = 0x0010;
        const SYNCHRONIZED = 0x0020;
        const BRIDGE       = 0x0040;
        const VARARGS      = 0x0080;
        const NATIVE       = 0x0100;
        const ABSTRACT     = 0x0400;
        const STRICT       = 0x0800;
        const SYNTHETIC    = 0x1000;
    }
}
