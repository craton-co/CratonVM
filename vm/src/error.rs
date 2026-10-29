// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Re-export from cratonvm-types crate.
pub use cratonvm_types::error::*;

/// A failure HotSpot reports with `vm_exit_during_initialization`: the two
/// lines `Error occurred during initialization of VM` and [`Self::detail`]
/// on **stdout**, nothing on stderr, exit status 1.
///
/// `SharedVm::new` is infallible by signature, so a boot failure of this
/// kind unwinds with this value as the panic payload
/// (`std::panic::panic_any`); the launcher's panic hook recognises it
/// ([`vm_initialization_error_of`]), prints HotSpot's lines and exits 1, and
/// an embedder that catches the unwind can downcast the payload. Interpreter
/// round i2 wave 54, lane L1: a startup agent whose library does not load or
/// whose `Agent_OnLoad` fails (`docs/feature-designs/jdk-only-mode.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmInitializationError {
    /// HotSpot's second line, e.g. `agent library failed Agent_OnLoad: <name>`.
    pub detail: String,
}

impl VmInitializationError {
    /// HotSpot's first line.
    pub const HEADLINE: &'static str = "Error occurred during initialization of VM";

    /// The two lines as HotSpot prints them, each ending in a newline.
    pub fn report(&self) -> String {
        format!("{}\n{}\n", Self::HEADLINE, self.detail)
    }
}

impl std::fmt::Display for VmInitializationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", Self::HEADLINE, self.detail)
    }
}

impl std::error::Error for VmInitializationError {}

/// zki (page `zgc-zt23hunt4-transparent-huge-pages-flag-ignored` item 4): a
/// failure HotSpot reports with `vm_shutdown_during_initialization` and then
/// a failed `JNI_CreateJavaVM` -- the two lines of [`VmInitializationError`]
/// on **stdout**, then the launcher's `Error: Could not create the Java
/// Virtual Machine.` / `Error: A fatal exception has occurred. Program will
/// exit.` on **stderr**, exit status 1. Measured on 25.0.4 for a ZGC initial
/// heap the hugetlbfs pool cannot hold (`Failed to allocate initial Java
/// heap (126M)`). Unwinds `SharedVm::new` as the panic payload, as
/// [`VmInitializationError`] does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmCreationError {
    /// HotSpot's second stdout line.
    pub detail: String,
}

impl VmCreationError {
    /// The launcher's two stderr lines.
    pub const LAUNCHER_LINES: &'static str = "Error: Could not create the Java Virtual Machine.\nError: A fatal exception has occurred. Program will exit.\n";

    /// The two stdout lines, each ending in a newline.
    pub fn report(&self) -> String {
        format!("{}\n{}\n", VmInitializationError::HEADLINE, self.detail)
    }
}

impl std::fmt::Display for VmCreationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", VmInitializationError::HEADLINE, self.detail)
    }
}

impl std::error::Error for VmCreationError {}

/// The [`VmCreationError`] a panic carries, if it is one.
pub fn vm_creation_error_of<'a>(
    info: &'a std::panic::PanicHookInfo<'_>,
) -> Option<&'a VmCreationError> {
    info.payload().downcast_ref::<VmCreationError>()
}

/// The [`VmInitializationError`] a panic carries, if it is one.
pub fn vm_initialization_error_of<'a>(
    info: &'a std::panic::PanicHookInfo<'_>,
) -> Option<&'a VmInitializationError> {
    info.payload().downcast_ref::<VmInitializationError>()
}
