// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Registrations RETIRED as contract §1.4 shadows: a native standing in front
//! of concrete JDK bytecode, one subsystem at a time.
//!
//! # The rule
//!
//! §1.4 says a native that shadows real bytecode should yield to it under
//! `--jdk-only`. The obvious disposition for the whole shadow population is
//! `SyntheticStub`, and that was implemented as a dispatch-time dial
//! (`CRATONVM_ENFORCE_NATIVE_SHADOW`) and MEASURED: arming it whole-VM takes
//! the strict corpus from **32 passed / 17 failed to 3 / 46**. The failures are
//! not dispatch faults — under `--jdk-only` the surviving bridges ARE the
//! object model for large parts of `java.base`, so yielding them hands real
//! code objects it cannot service.
//!
//! So the order is fixed and is not this file's choice: a class's state has to
//! become real before its shadow can be retired. What this file holds is the
//! subsystems where that has been **measured true**, triple by triple.
//!
//! # `java/util/logging` — retired 2026-08-11
//!
//! Measured on Azure linux, JDK 25.0.4+7, one binary, one workload, only the
//! dial differing:
//!
//! ```text
//!   SUITE=jdk-only CRATONVM_ARGS=--jdk-only bash regression-suite/run.sh
//!     baseline                                       23 passed / 4 failed
//!     CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/logging/  23 passed / 4 failed
//!   failing set unchanged: RJdkHandles RJdkReflect RJdkForkJoin RJdkJmx
//! ```
//!
//! Verdict-neutral is the acceptance criterion, not green: those four fail on
//! `dev` for reasons that have nothing to do with logging, and a change that
//! left them failing for a NEW reason would be a regression this comparison
//! catches.
//!
//! The 84 triples below are every LIVE (`owns_slot`) `Bridge` registration on a
//! `java/util/logging/` receiver whose image target carries a `Code` attribute,
//! declared or inherited — i.e. exactly the rows the dial would yield. They are
//! re-tagged [`NativeKind::SyntheticStub`](crate::registry::NativeKind::SyntheticStub)
//! at registration, so `--jdk-only` refuses them and the real class runs, and
//! `--real-jdk` is unchanged (a `SyntheticStub` registers and dispatches
//! normally in `Compatible` mode).
//!
//! **One `java/util/logging/` bridge is deliberately NOT here.**
//! `java/util/logging/Logger.log` — the census resolves its triple NOWHERE in
//! the hierarchy, so there is no bytecode for it to yield to and refusing it
//! would replace a shadow with an `UnsatisfiedLinkError`. That is the shape the
//! 2026-08-10 wave hit when four of 43 re-tagged receivers had to be held back,
//! and it is why this list is per-TRIPLE rather than per-class-prefix: a prefix
//! rule cannot see it.
//!
//! # Why this is applied centrally
//!
//! Same reason as [`crate::no_image_receiver`]: the property is a MEASUREMENT
//! against an image, which no registration site can know, and the sites do not
//! reliably name themselves anyway — `registered_by` is a `#[track_caller]`
//! record, so a shared `with_category` helper attributes a `LogRecord` triple
//! to `native-io/src/nio_native.rs`. Four different files register these 84.
//!
//! A registration re-tagged here reports `kind_stated`: the kind WAS
//! adjudicated, by measurement rather than by an author.
//!
//! # What would invalidate this
//!
//! A `java/util/logging/` triple that stops carrying `Code` in a supported
//! image, or a strict-corpus run whose failing SET differs from the four above
//! with these retired. `regression-suite/bridge-ratchet.sh` scores the census
//! these rows leave, and `scripts/jdk-only-kind-map.py` freezes each row's kind,
//! so a silent drift in either direction fails a gate rather than a workload.

/// Every `(class, method, descriptor)` retired as a §1.4 shadow.
///
/// **Sorted, and binary-searched.** An out-of-order entry is not a style
/// question: it makes the predicate answer `false` for a row that is in the
/// table, which reads as "not retired" and is invisible. The test below
/// asserts the ordering.
static RETIRED_SHADOW_TRIPLES: &[(&str, &str, &str)] = &[
    ("java/util/logging/FileHandler", "<init>", "()V"),
    ("java/util/logging/FileHandler", "<init>", "(Ljava/lang/String;)V"),
    ("java/util/logging/FileHandler", "close", "()V"),
    ("java/util/logging/FileHandler", "flush", "()V"),
    ("java/util/logging/FileHandler", "publish", "(Ljava/util/logging/LogRecord;)V"),
    ("java/util/logging/Handler", "<init>", "()V"),
    ("java/util/logging/Handler", "getFormatter", "()Ljava/util/logging/Formatter;"),
    ("java/util/logging/Handler", "getLevel", "()Ljava/util/logging/Level;"),
    ("java/util/logging/Handler", "isLoggable", "(Ljava/util/logging/LogRecord;)Z"),
    ("java/util/logging/Handler", "setFormatter", "(Ljava/util/logging/Formatter;)V"),
    ("java/util/logging/Handler", "setLevel", "(Ljava/util/logging/Level;)V"),
    ("java/util/logging/Level", "<clinit>", "()V"),
    ("java/util/logging/Level", "<init>", "(Ljava/lang/String;I)V"),
    ("java/util/logging/Level", "<init>", "(Ljava/lang/String;ILjava/lang/String;)V"),
    ("java/util/logging/Level", "findLevel", "(Ljava/lang/String;)Ljava/util/logging/Level;"),
    ("java/util/logging/Level", "getName", "()Ljava/lang/String;"),
    ("java/util/logging/Level", "intValue", "()I"),
    ("java/util/logging/Level", "parse", "(Ljava/lang/String;)Ljava/util/logging/Level;"),
    ("java/util/logging/Level", "toString", "()Ljava/lang/String;"),
    ("java/util/logging/LogManager", "<init>", "()V"),
    ("java/util/logging/LogManager", "addConfigurationListener", "(Ljava/lang/Runnable;)Ljava/util/logging/LogManager;"),
    ("java/util/logging/LogManager", "addLogger", "(Ljava/util/logging/Logger;)Z"),
    ("java/util/logging/LogManager", "checkAccess", "()V"),
    ("java/util/logging/LogManager", "getLogManager", "()Ljava/util/logging/LogManager;"),
    ("java/util/logging/LogManager", "getLogger", "(Ljava/lang/String;)Ljava/util/logging/Logger;"),
    ("java/util/logging/LogManager", "getLoggerNames", "()Ljava/util/Enumeration;"),
    ("java/util/logging/LogManager", "getProperty", "(Ljava/lang/String;)Ljava/lang/String;"),
    ("java/util/logging/LogManager", "readConfiguration", "()V"),
    ("java/util/logging/LogManager", "readConfiguration", "(Ljava/io/InputStream;)V"),
    ("java/util/logging/LogManager", "removeConfigurationListener", "(Ljava/lang/Runnable;)V"),
    ("java/util/logging/LogManager", "reset", "()V"),
    ("java/util/logging/LogManager", "updateConfiguration", "(Ljava/io/InputStream;Ljava/util/function/Function;)V"),
    ("java/util/logging/LogManager", "updateConfiguration", "(Ljava/util/function/Function;)V"),
    ("java/util/logging/LogRecord", "<init>", "(Ljava/util/logging/Level;Ljava/lang/String;)V"),
    ("java/util/logging/LogRecord", "getLevel", "()Ljava/util/logging/Level;"),
    ("java/util/logging/LogRecord", "getMessage", "()Ljava/lang/String;"),
    ("java/util/logging/LogRecord", "getSequenceNumber", "()J"),
    ("java/util/logging/Logger", "addHandler", "(Ljava/util/logging/Handler;)V"),
    ("java/util/logging/Logger", "config", "(Ljava/lang/String;)V"),
    ("java/util/logging/Logger", "config", "(Ljava/util/function/Supplier;)V"),
    ("java/util/logging/Logger", "entering", "(Ljava/lang/String;Ljava/lang/String;)V"),
    ("java/util/logging/Logger", "exiting", "(Ljava/lang/String;Ljava/lang/String;)V"),
    ("java/util/logging/Logger", "fine", "(Ljava/lang/String;)V"),
    ("java/util/logging/Logger", "fine", "(Ljava/util/function/Supplier;)V"),
    ("java/util/logging/Logger", "finer", "(Ljava/lang/String;)V"),
    ("java/util/logging/Logger", "finer", "(Ljava/util/function/Supplier;)V"),
    ("java/util/logging/Logger", "finest", "(Ljava/lang/String;)V"),
    ("java/util/logging/Logger", "finest", "(Ljava/util/function/Supplier;)V"),
    ("java/util/logging/Logger", "getFilter", "()Ljava/util/logging/Filter;"),
    ("java/util/logging/Logger", "getHandlers", "()[Ljava/util/logging/Handler;"),
    ("java/util/logging/Logger", "getLevel", "()Ljava/util/logging/Level;"),
    ("java/util/logging/Logger", "getLogger", "(Ljava/lang/String;)Ljava/util/logging/Logger;"),
    ("java/util/logging/Logger", "getLogger", "(Ljava/lang/String;Ljava/lang/String;)Ljava/util/logging/Logger;"),
    ("java/util/logging/Logger", "getName", "()Ljava/lang/String;"),
    ("java/util/logging/Logger", "getParent", "()Ljava/util/logging/Logger;"),
    ("java/util/logging/Logger", "getResourceBundle", "()Ljava/util/ResourceBundle;"),
    ("java/util/logging/Logger", "getResourceBundleName", "()Ljava/lang/String;"),
    ("java/util/logging/Logger", "getUseParentHandlers", "()Z"),
    ("java/util/logging/Logger", "info", "(Ljava/lang/String;)V"),
    ("java/util/logging/Logger", "info", "(Ljava/util/function/Supplier;)V"),
    ("java/util/logging/Logger", "isLoggable", "(Ljava/util/logging/Level;)Z"),
    ("java/util/logging/Logger", "log", "(Ljava/util/logging/Level;Ljava/lang/String;)V"),
    ("java/util/logging/Logger", "log", "(Ljava/util/logging/Level;Ljava/lang/String;Ljava/lang/Object;)V"),
    ("java/util/logging/Logger", "log", "(Ljava/util/logging/Level;Ljava/lang/String;Ljava/lang/Throwable;)V"),
    ("java/util/logging/Logger", "log", "(Ljava/util/logging/Level;Ljava/lang/String;[Ljava/lang/Object;)V"),
    ("java/util/logging/Logger", "log", "(Ljava/util/logging/Level;Ljava/lang/Throwable;Ljava/util/function/Supplier;)V"),
    ("java/util/logging/Logger", "log", "(Ljava/util/logging/Level;Ljava/util/function/Supplier;)V"),
    ("java/util/logging/Logger", "log", "(Ljava/util/logging/LogRecord;)V"),
    ("java/util/logging/Logger", "logp", "(Ljava/util/logging/Level;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V"),
    ("java/util/logging/Logger", "logp", "(Ljava/util/logging/Level;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/Throwable;)V"),
    ("java/util/logging/Logger", "removeHandler", "(Ljava/util/logging/Handler;)V"),
    ("java/util/logging/Logger", "setFilter", "(Ljava/util/logging/Filter;)V"),
    ("java/util/logging/Logger", "setLevel", "(Ljava/util/logging/Level;)V"),
    ("java/util/logging/Logger", "setParent", "(Ljava/util/logging/Logger;)V"),
    ("java/util/logging/Logger", "setUseParentHandlers", "(Z)V"),
    ("java/util/logging/Logger", "severe", "(Ljava/lang/String;)V"),
    ("java/util/logging/Logger", "severe", "(Ljava/util/function/Supplier;)V"),
    ("java/util/logging/Logger", "throwing", "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/Throwable;)V"),
    ("java/util/logging/Logger", "warning", "(Ljava/lang/String;)V"),
    ("java/util/logging/Logger", "warning", "(Ljava/util/function/Supplier;)V"),
    ("java/util/logging/LoggingPermission", "<init>", "()V"),
    ("java/util/logging/LoggingPermission", "<init>", "(Ljava/lang/String;)V"),
    ("java/util/logging/LoggingPermission", "<init>", "(Ljava/lang/String;Ljava/lang/String;)V"),
    ("java/util/logging/LoggingPermission", "getName", "()Ljava/lang/String;"),
];

/// Is this exact triple a retired §1.4 shadow?
///
/// The class-name prefix test is a cheap discriminator: every entry is under
/// `java/util/logging/`, and almost no registration is, so the common case
/// costs one prefix compare and nothing else.
pub fn triple_is_retired_shadow(class_name: &str, method_name: &str, descriptor: &str) -> bool {
    if !class_name.starts_with("java/util/logging/") {
        return false;
    }
    RETIRED_SHADOW_TRIPLES
        .binary_search(&(class_name, method_name, descriptor))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table is binary-searched, so ordering is correctness.
    #[test]
    fn the_table_is_sorted_and_unique() {
        for w in RETIRED_SHADOW_TRIPLES.windows(2) {
            assert!(w[0] < w[1], "out of order or duplicated: {:?} then {:?}", w[0], w[1]);
        }
    }

    /// Every entry must be findable through the public predicate — the prefix
    /// discriminator and the table must not disagree about what is in scope.
    #[test]
    fn every_entry_is_reachable_through_the_predicate() {
        for (c, m, d) in RETIRED_SHADOW_TRIPLES {
            assert!(triple_is_retired_shadow(c, m, d), "unreachable entry: {c}.{m}{d}");
        }
    }

    /// The retirement is per-TRIPLE. `Logger.log` shares the class with entries
    /// that ARE retired, and it must not be swept in: the census resolves it
    /// nowhere in the hierarchy, so refusing it would replace a shadow with an
    /// `UnsatisfiedLinkError`.
    #[test]
    fn logger_log_is_held_back() {
        assert!(triple_is_retired_shadow(
            "java/util/logging/Logger", "fine", "(Ljava/lang/String;)V"));
        assert!(!triple_is_retired_shadow(
            "java/util/logging/Logger",
            "log",
            "(Ljava/util/logging/Level;Ljava/lang/String;)V"
        ));
    }

    /// Nothing outside the retired subsystem is touched.
    #[test]
    fn other_subsystems_are_untouched() {
        assert!(!triple_is_retired_shadow("java/lang/String", "length", "()I"));
        assert!(!triple_is_retired_shadow("javax/management/MBeanServer", "getDomains",
                                          "()[Ljava/lang/String;"));
        // A logging class that is not in the table answers false too.
        assert!(!triple_is_retired_shadow("java/util/logging/Logger", "notARealMethod", "()V"));
    }

    /// A vacuity floor. An empty table would make every test above pass and
    /// retire nothing — the measurement recorded 84 triples.
    #[test]
    fn the_table_is_not_empty() {
        assert!(
            RETIRED_SHADOW_TRIPLES.len() >= 80,
            "expected the measured java.util.logging population (84), got {}",
            RETIRED_SHADOW_TRIPLES.len()
        );
    }
}
