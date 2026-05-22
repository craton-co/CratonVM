//! BlueJ educational-IDE boot-test shims.
//!
//! **HISTORY**: Previously this module short-circuited BlueJ's launcher
//! entry classes (`Installer`, `bluej/Main`, `bluej/BlueJ`,
//! `bluej/Boot`, `bluej/launcher/Launcher`) with fake no-op
//! `main([Ljava/lang/String;)V`, and additionally registered a large
//! cluster of synthetic AWT/Swing fake-outs:
//!
//!   * fake no-op `<clinit>`s on dozens of `java/awt/*`, `javax/swing/*`,
//!     and `sun/awt/*` classes;
//!   * faked static accessors returning canned `null` / no-op values
//!     (`AWTKeyStroke.getCachedStroke`, `AppContext.getAppContext`,
//!     `Toolkit.getDefaultToolkit`, `UIManager.getDefaults` / `get` /
//!     `put` / `getLookAndFeel` / `getLAFState`, `SwingUtilities.
//!     appContextGet` / `appContextPut`, `EventQueue.invokeLater`,
//!     `EventDispatchThread.run`, `UIManager.maybeInitialize` etc.).
//!
//! All of these made the JVM exit rc=0 without running BlueJ's real
//! bytecode, and worse, the AWT/Swing intercepts targeted core JDK
//! classes shared by every Swing workload.
//!
//! **CURRENT STATE (real-bytecode audit)**: every short-circuit and
//! fake-out registration has been REMOVED per the "no synthetic stubs"
//! policy. Real BlueJ bytecode now runs. This file is kept so the call
//! site in `lib.rs::register_essential_natives` continues to compile.

use cratonvm_native_api::NativeMethodRegistry;

/// Audit cleanup: no longer registers any natives. Previously short-
/// circuited BlueJ's `main` entry points and faked the AWT/Swing
/// graphics subsystem with canned `<clinit>` / accessor stubs.
pub fn register_bluej_stubs(_registry: &mut NativeMethodRegistry) {
    // Intentionally empty. Real BlueJ Installer bytecode runs.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_bluej_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_bluej_stubs(&mut r);
    }
}
