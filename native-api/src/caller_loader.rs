// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The loader a native's by-name class lookups resolve through.
//!
//! A native that stands in for a method of an APPLICATION library class
//! (`org/h2/...`, `reactor/...`: an intrinsic or a bridge) resolves the classes
//! it names the way JNI `FindClass` does: through the loader of the Java class
//! whose frame is running it. Under Spring Boot's `@ClassPathOverrides` /
//! `@ClassPathExclusions` that frame is a `ModifiedClassPathClassLoader` fork's,
//! which holds its OWN copy of every class: H2's `ValueBigint.get` intrinsic built
//! the application loader's `ValueBigint` into the fork's `Value[]`.
//!
//! The JDK's own natives (`Class.forName`, `ClassLoader`, the self-attach agent
//! start, reflection) are NOT stand-ins for a method of the caller: they are
//! given a loader, or are VM services, and resolve through the built-in chain
//! exactly as before. So the rule is decided by what the native is registered
//! FOR: [`note_registration`] records each callback, and the VM publishes the
//! callback it is running ([`enter_native`]); only a callback registered for
//! non-JDK classes alone is an "application native" ([`current_native_is_application`]).
//!
//! A VM service that nevertheless runs inside such a native wraps its body in
//! [`suppress`].
//!
//! Per-thread dispatch state (like [`crate::poly_call_site`]); the callback set is
//! a table of function addresses, the same for every VM.

use std::cell::Cell;
use std::sync::{OnceLock, RwLock};

use rustc_hash::FxHashSet;

const REG_CACHE_SIZE: usize = 64;
#[allow(clippy::declare_interior_mutable_const)]
const REG_CACHE_EMPTY: Cell<(usize, bool)> = Cell::new((0, false));

thread_local! {
    static SUPPRESSED: Cell<u32> = const { Cell::new(0) };
    static CURRENT_NATIVE: Cell<usize> = const { Cell::new(0) };
    // One `Cell` per way: the whole-array `Cell` this replaced copied all
    // 64 entries (1 KiB) out and back on every registration (`p5-natreg`).
    static REG_CACHE: [Cell<(usize, bool)>; REG_CACHE_SIZE] = const { [REG_CACHE_EMPTY; REG_CACHE_SIZE] };
}

struct Sets {
    application: FxHashSet<usize>,
    jdk: FxHashSet<usize>,
}

fn sets() -> &'static RwLock<Sets> {
    static SETS: OnceLock<RwLock<Sets>> = OnceLock::new();
    SETS.get_or_init(|| {
        RwLock::new(Sets {
            application: FxHashSet::default(),
            jdk: FxHashSet::default(),
        })
    })
}

/// Whether `class_name` (internal form) belongs to the JDK / the VM itself.
fn is_jdk_class(class_name: &str) -> bool {
    class_name.starts_with("java/")
        || class_name.starts_with("javax/")
        || class_name.starts_with("jdk/")
        || class_name.starts_with("sun/")
        || class_name.starts_with("com/sun/")
        || class_name.starts_with("cratonvm/")
        || class_name.starts_with('[')
}

/// Record that `callback` is registered for `class_name`.
pub fn note_registration(class_name: &str, callback: usize) {
    if callback == 0 {
        return;
    }
    let jdk = is_jdk_class(class_name);
    let slot = (callback ^ (callback >> 6)) & (REG_CACHE_SIZE - 1);
    if REG_CACHE.with(|c| c[slot].get() == (callback, jdk)) {
        return;
    }
    {
        let guard = sets().read().unwrap_or_else(|e| e.into_inner());
        let known = if jdk {
            guard.jdk.contains(&callback)
        } else {
            guard.application.contains(&callback)
        };
        if known {
            REG_CACHE.with(|c| c[slot].set((callback, jdk)));
            return;
        }
    }
    let mut guard = sets().write().unwrap_or_else(|e| e.into_inner());
    if jdk {
        guard.jdk.insert(callback);
    } else {
        guard.application.insert(callback);
    }
    REG_CACHE.with(|c| c[slot].set((callback, jdk)));
}

/// The native the current thread is inside, restored when dropped (natives nest:
/// a native runs Java that runs another native).
pub struct NativeFrame(usize);

impl Drop for NativeFrame {
    fn drop(&mut self) {
        CURRENT_NATIVE.with(|c| c.set(self.0));
    }
}

/// Publish `callback` as the native this thread is running.
#[inline]
pub fn enter_native(callback: usize) -> NativeFrame {
    NativeFrame(CURRENT_NATIVE.with(|c| c.replace(callback)))
}

/// Whether the native this thread is running is registered for application
/// classes only (a stand-in for a method of an application library class).
pub fn current_native_is_application() -> bool {
    let cb = CURRENT_NATIVE.with(|c| c.get());
    if cb == 0 {
        return false;
    }
    let guard = sets().read().unwrap_or_else(|e| e.into_inner());
    guard.application.contains(&cb) && !guard.jdk.contains(&cb)
}

/// Run `f` with the calling-class loader switched off for this thread's
/// by-name lookups (nested calls included).
pub fn suppress<R>(f: impl FnOnce() -> R) -> R {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            SUPPRESSED.with(|c| c.set(c.get().saturating_sub(1)));
        }
    }
    SUPPRESSED.with(|c| c.set(c.get() + 1));
    let _guard = Guard;
    f()
}

/// Whether [`suppress`] is active on this thread.
pub fn suppressed() -> bool {
    SUPPRESSED.with(|c| c.get() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[inline(never)]
    fn jdk_native(x: usize) -> usize {
        x.wrapping_mul(31).wrapping_add(7)
    }
    #[inline(never)]
    fn app_native(x: usize) -> usize {
        x.wrapping_mul(17).wrapping_add(3).rotate_left(5)
    }

    #[test]
    fn suppress_nests_and_restores() {
        assert!(!suppressed());
        suppress(|| {
            assert!(suppressed());
            suppress(|| assert!(suppressed()));
            assert!(suppressed());
        });
        assert!(!suppressed());
    }

    #[test]
    fn only_an_application_only_callback_is_an_application_native() {
        let app = app_native as usize;
        let jdk = jdk_native as usize;
        note_registration("org/h2/value/ValueBigint", app);
        note_registration("java/lang/Class", jdk);
        {
            let _f = enter_native(app);
            assert!(current_native_is_application());
        }
        {
            let _f = enter_native(jdk);
            assert!(!current_native_is_application());
        }
        // Registered for both: conservatively a JDK native.
        note_registration("org/h2/value/ValueBigint", jdk);
        {
            let _f = enter_native(jdk);
            assert!(!current_native_is_application());
        }
        assert!(!current_native_is_application());
    }
}
