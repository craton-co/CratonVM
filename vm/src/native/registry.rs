// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// Re-exported from cratonvm-native-api. All crate::native::registry::* paths continue to work.
pub use cratonvm_native_api::{
    AnnotationData, AnnotationElementValue, FieldMetadata, MethodMetadata, NativeCallback,
    NativeClassAccess, NativeContext, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeMethodRegistry, NativeSystemAccess, NativeThreadAccess,
    NativeThreadBlocker, StackTraceEntry, TypeArgAnnotations,
};
