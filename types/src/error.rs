// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! VM error types and the two-layer exception model.
//!
//! Defines [`MethodCallFailed`] — the result of a failed Java method call,
//! split into non-catchable internal VM errors and catchable Java exceptions —
//! along with the [`VmError`] hierarchy ([`ClassFileError`], [`LinkageError`],
//! [`RuntimeError`]) and the [`MethodCallResult`] alias used throughout the VM.

use std::fmt;

use thiserror::Error;

use crate::{ObjectRef, Value};

// ---------------------------------------------------------------------------
// MethodCallFailed -- the two-layer exception model
// ---------------------------------------------------------------------------

/// The result of executing a Java method.
///
/// - `Ok(Some(value))` -- method returned a value (non-void)
/// - `Ok(None)` -- method returned void
/// - `Err(MethodCallFailed)` -- method failed (either internal error or Java exception)
pub type MethodCallResult = Result<Option<Value>, MethodCallFailed>;

/// How a method call can fail.
///
/// This is the core of the exception model:
/// - **`InternalError`**: a Rust-level VM bug or fatal error. These are **not**
///   catchable by Java `catch` blocks. They abort execution entirely.
/// - **`ExceptionThrown`**: a Java exception was thrown. The `ObjectRef` points
///   to a heap-allocated `Throwable` object that can be caught by Java exception
///   handlers.
#[derive(Debug)]
pub enum MethodCallFailed {
    /// Internal VM error (not catchable by Java code).
    InternalError(VmError),

    /// A Java exception was thrown (can be caught by exception handlers).
    /// The `ObjectRef` points to the `Throwable` object on the heap.
    ExceptionThrown(ObjectRef),
}

impl fmt::Display for MethodCallFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // `VmError` is already a self-describing error: every variant's
            // `Display` carries its own category prefix ("class file error: ",
            // "linkage error: ", "runtime error: ", "internal error: ").
            // Prepending another "internal error: " here doubled the prefix
            // for `VmError::Internal` (yielding "internal error: internal
            // error: ...") and mislabeled the other variants. Delegate to the
            // inner error's `Display` so the category appears exactly once.
            MethodCallFailed::InternalError(err) => write!(f, "{err}"),
            MethodCallFailed::ExceptionThrown(obj_ref) => {
                write!(f, "exception thrown: ref({:p})", obj_ref.as_ptr())
            }
        }
    }
}

impl From<VmError> for MethodCallFailed {
    fn from(err: VmError) -> Self {
        MethodCallFailed::InternalError(err)
    }
}

impl From<ClassFileError> for MethodCallFailed {
    fn from(err: ClassFileError) -> Self {
        MethodCallFailed::InternalError(VmError::ClassFile(err))
    }
}

impl From<LinkageError> for MethodCallFailed {
    fn from(err: LinkageError) -> Self {
        MethodCallFailed::InternalError(VmError::Linkage(err))
    }
}

impl From<RuntimeError> for MethodCallFailed {
    fn from(err: RuntimeError) -> Self {
        MethodCallFailed::InternalError(VmError::Runtime(err))
    }
}

// ---------------------------------------------------------------------------
// VmError -- the existing error hierarchy
// ---------------------------------------------------------------------------

/// Top-level VM error categories.
#[derive(Debug, Error)]
pub enum VmError {
    /// Error reading or parsing a `.class` file.
    #[error("class file error: {0}")]
    ClassFile(#[from] ClassFileError),

    /// Error during class linking (verification, preparation, resolution).
    #[error("linkage error: {0}")]
    Linkage(#[from] LinkageError),

    /// Runtime error during bytecode execution.
    #[error("runtime error: {0}")]
    Runtime(#[from] RuntimeError),

    /// Internal VM error (bug in the implementation).
    #[error("internal error: {message}")]
    Internal { message: String },
}

/// Errors related to class file loading and parsing.
#[derive(Debug, Error)]
pub enum ClassFileError {
    #[error("class not found: {class_name}")]
    ClassNotFound { class_name: String },

    #[error("I/O error reading class {class_name}: {source}")]
    IoError {
        class_name: String,
        source: std::io::Error,
    },

    #[error("invalid class file {class_name}: {message}")]
    InvalidClassFile { class_name: String, message: String },

    #[error("unsupported class version {major}.{minor} for class {class_name}")]
    UnsupportedVersion {
        class_name: String,
        major: u16,
        minor: u16,
    },
}

/// Errors during class linking (JVM spec Chapter 5).
#[derive(Debug, Error)]
pub enum LinkageError {
    #[error("class format error in {class_name}: {message}")]
    ClassFormatError { class_name: String, message: String },

    #[error("verification error in {class_name}.{method_name}: {message}")]
    VerifyError {
        class_name: String,
        method_name: String,
        message: String,
    },

    #[error("no class def found: {class_name}")]
    NoClassDefFoundError { class_name: String },

    #[error("incompatible class change: {message}")]
    IncompatibleClassChangeError { message: String },

    #[error("no such field: {class_name}.{field_name}")]
    NoSuchFieldError {
        class_name: String,
        field_name: String,
    },

    #[error("no such method: {class_name}.{method_name}{method_descriptor}")]
    NoSuchMethodError {
        class_name: String,
        method_name: String,
        method_descriptor: String,
    },

    #[error("illegal access: {message}")]
    IllegalAccessError { message: String },

    #[error("abstract method error: {class_name}.{method_name}")]
    AbstractMethodError {
        class_name: String,
        method_name: String,
    },

    /// JVMTI `RedefineClasses` / `RetransformClasses` rejected the new
    /// bytecode because it violates JEP 109's structural-equivalence
    /// constraints (class name, superclass, interfaces, field set, or
    /// method declarations changed). Surfaced to JVMTI agents as
    /// `JVMTI_ERROR_UNSUPPORTED_REDEFINITION_*` and to in-process Java
    /// callers as `UnsupportedClassRedefinitionException`.
    #[error("unsupported class redefinition: {class_name}: {message}")]
    UnsupportedClassRedefinitionError { class_name: String, message: String },
}

/// Runtime exceptions during bytecode execution.
#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("NullPointerException{}", format_optional_message(.message))]
    NullPointerException { message: Option<String> },

    #[error("ArrayIndexOutOfBoundsException: index {index}")]
    ArrayIndexOutOfBoundsException { index: i32 },

    #[error("ArithmeticException: {message}")]
    ArithmeticException { message: String },

    #[error("ClassCastException: {message}")]
    ClassCastException { message: String },

    #[error("StackOverflowError")]
    StackOverflowError,

    #[error("OutOfMemoryError: {message}")]
    OutOfMemoryError { message: String },

    #[error("NegativeArraySizeException: {size}")]
    NegativeArraySizeException { size: i32 },

    #[error("ArrayStoreException: {message}")]
    ArrayStoreException { message: String },

    #[error("StringIndexOutOfBoundsException: index {index}")]
    StringIndexOutOfBoundsException { index: i32 },

    #[error("ClassNotFoundException: {class_name}")]
    ClassNotFoundException { class_name: String },

    #[error("UnsatisfiedLinkError: {message}")]
    UnsatisfiedLinkError { message: String },

    #[error("IllegalMonitorStateException: {message}")]
    IllegalMonitorStateException { message: String },

    #[error("NumberFormatException: {message}")]
    NumberFormatException { message: String },

    #[error("InterruptedException")]
    InterruptedException,

    #[error("NoSuchFieldException: {field_name}")]
    NoSuchFieldException { field_name: String },

    #[error("NoSuchMethodException: {message}")]
    NoSuchMethodException { message: String },

    #[error("IllegalAccessException: {message}")]
    IllegalAccessException { message: String },

    #[error("InaccessibleObjectException: {message}")]
    InaccessibleObjectException { message: String },

    #[error("IllegalArgumentException: {message}")]
    IllegalArgumentException { message: String },

    #[error("IOException: {message}")]
    IOException { message: String },

    #[error("EOFException: {message}")]
    EOFException { message: String },

    /// `java.net.UnknownHostException` — a host name could not be resolved or
    /// is malformed. A subclass of IOException; must be thrown as the concrete
    /// type because real code catches it specifically (e.g. Tomcat
    /// `NetMask` catches `UnknownHostException` to convert to
    /// IllegalArgumentException — a bare IOException escapes that catch).
    #[error("UnknownHostException: {message}")]
    UnknownHostException { message: String },

    /// `java.net.SocketTimeoutException` — a blocking socket operation timed
    /// out (e.g. a read exceeded `setSoTimeout`/`setReadTimeout`). A subclass
    /// of `InterruptedIOException`/`IOException`; must be thrown as the
    /// concrete type because real code catches it specifically (e.g. Tomcat's
    /// `TestConnector.testStop` does `catch (SocketTimeoutException)` to treat
    /// a post-stop read timeout as 503 — a bare IOException escapes that catch).
    #[error("SocketTimeoutException: {message}")]
    SocketTimeoutException { message: String },

    /// `java.net.ConnectException` — a connection attempt was actively
    /// refused (or otherwise failed to establish) by the remote host. A
    /// subclass of `SocketException`/`IOException`; must be thrown as the
    /// concrete type because real code catches it specifically (e.g. ES
    /// `RestClientMultipleHostsIntegTests.testNodeSelector` does
    /// `catch (ConnectException e)` around a request to a stopped host — a
    /// bare IOException whose message merely mentions "ConnectException"
    /// escapes that catch and fails the test).
    #[error("ConnectException: {message}")]
    ConnectException { message: String },

    /// `java.net.BindException` — a `bind()` failed, typically because the
    /// requested address/port is already in use. A subclass of
    /// `SocketException`/`IOException`; must be thrown as the concrete type
    /// because real code catches it specifically (e.g. Spring Boot's
    /// `PortInUseException.throwIfPortBindingException` does
    /// `ifCausedBy(ex, BindException.class, ...)` walking the cause chain —
    /// a bare IOException whose message merely mentions "BindException" as a
    /// text prefix is invisible to that `instanceof`-based walk, so
    /// `NettyWebServer.start()` falls back to a generic `WebServerException`
    /// instead of the specific `PortInUseException` tests assert on).
    #[error("BindException: {message}")]
    BindException { message: String },

    #[error("FileNotFoundException: {path}")]
    FileNotFoundException { path: String },

    #[error("NoSuchFileException: {path}")]
    NoSuchFileException { path: String },

    #[error("UnsupportedOperationException: {message}")]
    UnsupportedOperationException { message: String },

    #[error("IllegalStateException: {message}")]
    IllegalStateException { message: String },

    #[error("IllegalThreadStateException: {message}")]
    IllegalThreadStateException { message: String },

    /// Thrown when a method is invoked by an unauthorized caller. Used by the
    /// Panama native-access gate when `--enable-native-access` has not been
    /// granted to the calling module — matches OpenJDK's
    /// `java.lang.IllegalCallerException` semantics.
    #[error("IllegalCallerException: {message}")]
    IllegalCallerException { message: String },

    #[error("ConcurrentModificationException")]
    ConcurrentModificationException,

    #[error("NoSuchElementException: {message}")]
    NoSuchElementException { message: String },

    /// `java.nio.BufferUnderflowException` — a relative `get` was attempted on
    /// a buffer with no elements remaining. Distinct from IllegalStateException
    /// because real code catches it specifically (e.g. Tomcat
    /// `CharsetUtil.isAsciiSuperset`); folding it into IllegalStateException
    /// makes those `catch (BufferUnderflowException)` blocks miss.
    #[error("BufferUnderflowException")]
    BufferUnderflowException,

    /// `java.nio.BufferOverflowException` — a relative `put` was attempted on a
    /// buffer with no space remaining.
    #[error("BufferOverflowException")]
    BufferOverflowException,

    /// `java.nio.ReadOnlyBufferException` — a mutating operation (`put`,
    /// `compact`, `array()`) was attempted on a read-only buffer.
    #[error("ReadOnlyBufferException")]
    ReadOnlyBufferException,

    #[error("InputMismatchException: {message}")]
    InputMismatchException { message: String },

    #[error("SecurityException: {message}")]
    SecurityException { message: String },

    #[error("MatchException: {message}")]
    MatchException { message: String },

    #[error("not implemented: {feature}")]
    NotImplemented { feature: String },
}

fn format_optional_message(message: &Option<String>) -> String {
    match message {
        Some(msg) => format!(": {msg}"),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- VmError Display tests --

    #[test]
    fn vm_error_class_file_display() {
        let err = VmError::ClassFile(ClassFileError::ClassNotFound {
            class_name: "com/example/Foo".into(),
        });
        assert_eq!(
            format!("{err}"),
            "class file error: class not found: com/example/Foo"
        );
    }

    #[test]
    fn vm_error_linkage_display() {
        let err = VmError::Linkage(LinkageError::NoClassDefFoundError {
            class_name: "Bar".into(),
        });
        assert_eq!(format!("{err}"), "linkage error: no class def found: Bar");
    }

    #[test]
    fn vm_error_runtime_display() {
        let err = VmError::Runtime(RuntimeError::StackOverflowError);
        assert_eq!(format!("{err}"), "runtime error: StackOverflowError");
    }

    #[test]
    fn vm_error_internal_display() {
        let err = VmError::Internal {
            message: "something broke".into(),
        };
        assert_eq!(format!("{err}"), "internal error: something broke");
    }

    // -- VmError From conversions --

    #[test]
    fn vm_error_from_class_file_error() {
        let cfe = ClassFileError::ClassNotFound {
            class_name: "X".into(),
        };
        let vm_err: VmError = cfe.into();
        assert!(matches!(vm_err, VmError::ClassFile(_)));
    }

    #[test]
    fn vm_error_from_linkage_error() {
        let le = LinkageError::IllegalAccessError {
            message: "denied".into(),
        };
        let vm_err: VmError = le.into();
        assert!(matches!(vm_err, VmError::Linkage(_)));
    }

    #[test]
    fn vm_error_from_runtime_error() {
        let re = RuntimeError::StackOverflowError;
        let vm_err: VmError = re.into();
        assert!(matches!(vm_err, VmError::Runtime(_)));
    }

    // -- ClassFileError variants --

    #[test]
    fn class_file_error_class_not_found() {
        let err = ClassFileError::ClassNotFound {
            class_name: "java/lang/Object".into(),
        };
        assert_eq!(format!("{err}"), "class not found: java/lang/Object");
    }

    #[test]
    fn class_file_error_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file gone");
        let err = ClassFileError::IoError {
            class_name: "Test".into(),
            source: io_err,
        };
        let display = format!("{err}");
        assert!(display.contains("I/O error reading class Test"));
        assert!(display.contains("file gone"));
    }

    #[test]
    fn class_file_error_invalid_class_file() {
        let err = ClassFileError::InvalidClassFile {
            class_name: "Bad".into(),
            message: "bad magic number".into(),
        };
        assert_eq!(format!("{err}"), "invalid class file Bad: bad magic number");
    }

    #[test]
    fn class_file_error_unsupported_version() {
        let err = ClassFileError::UnsupportedVersion {
            class_name: "Future".into(),
            major: 99,
            minor: 0,
        };
        assert_eq!(
            format!("{err}"),
            "unsupported class version 99.0 for class Future"
        );
    }

    // -- LinkageError variants --

    #[test]
    fn linkage_error_class_format_error() {
        let err = LinkageError::ClassFormatError {
            class_name: "X".into(),
            message: "corrupt".into(),
        };
        assert_eq!(format!("{err}"), "class format error in X: corrupt");
    }

    #[test]
    fn linkage_error_verify_error() {
        let err = LinkageError::VerifyError {
            class_name: "A".into(),
            method_name: "foo".into(),
            message: "bad stack".into(),
        };
        assert_eq!(format!("{err}"), "verification error in A.foo: bad stack");
    }

    #[test]
    fn linkage_error_no_such_field() {
        let err = LinkageError::NoSuchFieldError {
            class_name: "C".into(),
            field_name: "x".into(),
        };
        assert_eq!(format!("{err}"), "no such field: C.x");
    }

    #[test]
    fn linkage_error_no_such_method() {
        let err = LinkageError::NoSuchMethodError {
            class_name: "C".into(),
            method_name: "run".into(),
            method_descriptor: "(I)V".into(),
        };
        assert_eq!(format!("{err}"), "no such method: C.run(I)V");
    }

    #[test]
    fn linkage_error_incompatible_class_change() {
        let err = LinkageError::IncompatibleClassChangeError {
            message: "interface became class".into(),
        };
        assert_eq!(
            format!("{err}"),
            "incompatible class change: interface became class"
        );
    }

    #[test]
    fn linkage_error_illegal_access() {
        let err = LinkageError::IllegalAccessError {
            message: "private".into(),
        };
        assert_eq!(format!("{err}"), "illegal access: private");
    }

    #[test]
    fn linkage_error_abstract_method() {
        let err = LinkageError::AbstractMethodError {
            class_name: "I".into(),
            method_name: "doIt".into(),
        };
        assert_eq!(format!("{err}"), "abstract method error: I.doIt");
    }

    // -- RuntimeError variants --

    #[test]
    fn runtime_error_null_pointer_with_message() {
        let err = RuntimeError::NullPointerException {
            message: Some("field access".into()),
        };
        assert_eq!(format!("{err}"), "NullPointerException: field access");
    }

    #[test]
    fn runtime_error_null_pointer_without_message() {
        let err = RuntimeError::NullPointerException { message: None };
        assert_eq!(format!("{err}"), "NullPointerException");
    }

    #[test]
    fn runtime_error_array_index_out_of_bounds() {
        let err = RuntimeError::ArrayIndexOutOfBoundsException { index: -1 };
        assert_eq!(format!("{err}"), "ArrayIndexOutOfBoundsException: index -1");
    }

    #[test]
    fn runtime_error_arithmetic() {
        let err = RuntimeError::ArithmeticException {
            message: "/ by zero".into(),
        };
        assert_eq!(format!("{err}"), "ArithmeticException: / by zero");
    }

    #[test]
    fn runtime_error_class_cast() {
        let err = RuntimeError::ClassCastException {
            message: "String cannot be cast to Integer".into(),
        };
        assert_eq!(
            format!("{err}"),
            "ClassCastException: String cannot be cast to Integer"
        );
    }

    #[test]
    fn runtime_error_stack_overflow() {
        let err = RuntimeError::StackOverflowError;
        assert_eq!(format!("{err}"), "StackOverflowError");
    }

    #[test]
    fn runtime_error_out_of_memory() {
        let err = RuntimeError::OutOfMemoryError {
            message: "heap full".into(),
        };
        assert_eq!(format!("{err}"), "OutOfMemoryError: heap full");
    }

    #[test]
    fn runtime_error_negative_array_size() {
        let err = RuntimeError::NegativeArraySizeException { size: -5 };
        assert_eq!(format!("{err}"), "NegativeArraySizeException: -5");
    }

    #[test]
    fn runtime_error_array_store() {
        let err = RuntimeError::ArrayStoreException {
            message: "wrong type".into(),
        };
        assert_eq!(format!("{err}"), "ArrayStoreException: wrong type");
    }

    #[test]
    fn runtime_error_string_index_out_of_bounds() {
        let err = RuntimeError::StringIndexOutOfBoundsException { index: 99 };
        assert_eq!(
            format!("{err}"),
            "StringIndexOutOfBoundsException: index 99"
        );
    }

    #[test]
    fn runtime_error_class_not_found() {
        let err = RuntimeError::ClassNotFoundException {
            class_name: "Missing".into(),
        };
        assert_eq!(format!("{err}"), "ClassNotFoundException: Missing");
    }

    #[test]
    fn runtime_error_unsatisfied_link() {
        let err = RuntimeError::UnsatisfiedLinkError {
            message: "native lib".into(),
        };
        assert_eq!(format!("{err}"), "UnsatisfiedLinkError: native lib");
    }

    #[test]
    fn runtime_error_illegal_monitor_state() {
        let err = RuntimeError::IllegalMonitorStateException {
            message: "not owner".into(),
        };
        assert_eq!(format!("{err}"), "IllegalMonitorStateException: not owner");
    }

    #[test]
    fn runtime_error_number_format() {
        let err = RuntimeError::NumberFormatException {
            message: "abc".into(),
        };
        assert_eq!(format!("{err}"), "NumberFormatException: abc");
    }

    #[test]
    fn runtime_error_interrupted() {
        let err = RuntimeError::InterruptedException;
        assert_eq!(format!("{err}"), "InterruptedException");
    }

    #[test]
    fn runtime_error_not_implemented() {
        let err = RuntimeError::NotImplemented {
            feature: "invokedynamic".into(),
        };
        assert_eq!(format!("{err}"), "not implemented: invokedynamic");
    }

    #[test]
    fn runtime_error_concurrent_modification() {
        let err = RuntimeError::ConcurrentModificationException;
        assert_eq!(format!("{err}"), "ConcurrentModificationException");
    }

    #[test]
    fn runtime_error_io_exception() {
        let err = RuntimeError::IOException {
            message: "broken pipe".into(),
        };
        assert_eq!(format!("{err}"), "IOException: broken pipe");
    }

    #[test]
    fn runtime_error_file_not_found() {
        let err = RuntimeError::FileNotFoundException {
            path: "/tmp/missing.txt".into(),
        };
        assert_eq!(format!("{err}"), "FileNotFoundException: /tmp/missing.txt");
    }

    #[test]
    fn runtime_error_unsupported_operation() {
        let err = RuntimeError::UnsupportedOperationException {
            message: "immutable".into(),
        };
        assert_eq!(format!("{err}"), "UnsupportedOperationException: immutable");
    }

    #[test]
    fn runtime_error_illegal_state() {
        let err = RuntimeError::IllegalStateException {
            message: "closed".into(),
        };
        assert_eq!(format!("{err}"), "IllegalStateException: closed");
    }

    #[test]
    fn runtime_error_illegal_caller_display() {
        // The new variant exists and formats consistently with its siblings —
        // bare class name followed by ": <message>", no double prefix.
        let err = RuntimeError::IllegalCallerException {
            message: "Native access is not enabled for this module".into(),
        };
        assert_eq!(
            format!("{err}"),
            "IllegalCallerException: Native access is not enabled for this module"
        );
    }

    #[test]
    fn runtime_error_illegal_caller_is_distinct_from_illegal_state() {
        // Regression guard for task #57: the Panama native-access gate used
        // to fold IllegalCallerException into IllegalStateException because
        // the variant did not exist. The two variants must remain distinct
        // at the Rust level so the exception-mapping table can route them
        // to different Java classes.
        let caller = RuntimeError::IllegalCallerException {
            message: "denied".into(),
        };
        let state = RuntimeError::IllegalStateException {
            message: "denied".into(),
        };
        assert!(matches!(
            caller,
            RuntimeError::IllegalCallerException { .. }
        ));
        assert!(matches!(state, RuntimeError::IllegalStateException { .. }));
        // Display strings must not collide.
        assert_ne!(format!("{caller}"), format!("{state}"));
    }

    #[test]
    fn runtime_error_no_such_element() {
        let err = RuntimeError::NoSuchElementException {
            message: "empty".into(),
        };
        assert_eq!(format!("{err}"), "NoSuchElementException: empty");
    }

    #[test]
    fn runtime_error_input_mismatch() {
        let err = RuntimeError::InputMismatchException {
            message: "expected int".into(),
        };
        assert_eq!(format!("{err}"), "InputMismatchException: expected int");
    }

    #[test]
    fn runtime_error_no_such_field_exception() {
        let err = RuntimeError::NoSuchFieldException {
            field_name: "value".into(),
        };
        assert_eq!(format!("{err}"), "NoSuchFieldException: value");
    }

    #[test]
    fn runtime_error_no_such_method_exception() {
        let err = RuntimeError::NoSuchMethodException {
            message: "run()".into(),
        };
        assert_eq!(format!("{err}"), "NoSuchMethodException: run()");
    }

    #[test]
    fn runtime_error_illegal_access_exception() {
        let err = RuntimeError::IllegalAccessException {
            message: "private method".into(),
        };
        assert_eq!(format!("{err}"), "IllegalAccessException: private method");
    }

    #[test]
    fn runtime_error_illegal_argument_exception() {
        let err = RuntimeError::IllegalArgumentException {
            message: "negative".into(),
        };
        assert_eq!(format!("{err}"), "IllegalArgumentException: negative");
    }

    // -- MethodCallFailed tests --

    #[test]
    fn method_call_failed_internal_error_display() {
        let err = MethodCallFailed::InternalError(VmError::Internal {
            message: "oops".into(),
        });
        // `MethodCallFailed::InternalError` delegates to the inner `VmError`'s
        // `Display`, which already supplies the "internal error: " prefix —
        // so the prefix must appear exactly once, not twice.
        assert_eq!(format!("{err}"), "internal error: oops");
    }

    #[test]
    fn method_call_failed_exception_thrown_display() {
        let fake_ptr = 0xDEAD_BEE0_u64 as *mut u8; // must be 8-byte aligned
        let obj = unsafe { ObjectRef::from_raw(fake_ptr) };
        let err = MethodCallFailed::ExceptionThrown(obj);
        let display = format!("{err}");
        assert!(display.starts_with("exception thrown: ref(0x"));
    }

    #[test]
    fn method_call_failed_from_vm_error() {
        let vm_err = VmError::Internal {
            message: "bug".into(),
        };
        let mcf: MethodCallFailed = vm_err.into();
        assert!(matches!(mcf, MethodCallFailed::InternalError(_)));
    }

    #[test]
    fn method_call_failed_from_class_file_error() {
        let cfe = ClassFileError::ClassNotFound {
            class_name: "X".into(),
        };
        let mcf: MethodCallFailed = cfe.into();
        assert!(matches!(
            mcf,
            MethodCallFailed::InternalError(VmError::ClassFile(_))
        ));
    }

    #[test]
    fn method_call_failed_from_linkage_error() {
        let le = LinkageError::NoClassDefFoundError {
            class_name: "Y".into(),
        };
        let mcf: MethodCallFailed = le.into();
        assert!(matches!(
            mcf,
            MethodCallFailed::InternalError(VmError::Linkage(_))
        ));
    }

    #[test]
    fn method_call_failed_from_runtime_error() {
        let re = RuntimeError::StackOverflowError;
        let mcf: MethodCallFailed = re.into();
        assert!(matches!(
            mcf,
            MethodCallFailed::InternalError(VmError::Runtime(_))
        ));
    }

    // -- MethodCallResult pattern matching --

    #[test]
    fn method_call_result_ok_value() {
        let result: MethodCallResult = Ok(Some(Value::Int(42)));
        assert!(result.is_ok());
        assert_eq!(result.unwrap().unwrap().as_int(), Some(42));
    }

    #[test]
    fn method_call_result_ok_void() {
        let result: MethodCallResult = Ok(None);
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn method_call_result_err_internal() {
        let result: MethodCallResult = Err(MethodCallFailed::InternalError(VmError::Internal {
            message: "fail".into(),
        }));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, MethodCallFailed::InternalError(_)));
    }

    // -- format_optional_message --

    #[test]
    fn format_optional_message_some() {
        let result = format_optional_message(&Some("detail".into()));
        assert_eq!(result, ": detail");
    }

    #[test]
    fn format_optional_message_none() {
        let result = format_optional_message(&None);
        assert_eq!(result, "");
    }
}
