// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Block 2B probe: does `LogManager.getLogManager()` honour
// `-Djava.util.logging.manager=<className>`?
//
// Driven by `vm/tests/block_2b_logmanager_factory.rs`, which runs this class
// twice against the cratonvm CLI: once with no property (the manager must be
// the stock `java.util.logging.LogManager`, and `MyLm-init` must NOT appear)
// and once with `-Djava.util.logging.manager=LmSubclass$MyLm` (the nested
// subclass ctor must run and `getClass().getName()` must name it).
//
// Nothing printed here is a literal expectation: the `class=` line is whatever
// `getLogManager().getClass().getName()` actually returns, `MyLm-init` is
// printed only by the subclass constructor, and `OK` is reached only after the
// checks below pass. A VM that ignores the property prints
// `class=java.util.logging.LogManager` in the second run and dies on the
// mismatch check before reaching `OK`.

import java.util.logging.LogManager;
import java.util.logging.Logger;

public class LmSubclass {

    /** The manager the property names. Its ctor is the only source of `MyLm-init`. */
    public static class MyLm extends LogManager {
        static volatile int inits = 0;

        public MyLm() {
            super();
            inits++;
            System.out.println("MyLm-init");
        }
    }

    private static final String STOCK = "java.util.logging.LogManager";
    private static final String PROP = "java.util.logging.manager";

    public static void main(String[] args) throws Exception {
        // What the VM was asked for. `null` = the default case.
        String requested = System.getProperty(PROP);

        LogManager lm = LogManager.getLogManager();
        if (lm == null) {
            throw new AssertionError("LogManager.getLogManager() returned null");
        }

        // The one observation this probe exists to make.
        String actual = lm.getClass().getName();
        System.out.println("class=" + actual);

        // The manager is a process-wide singleton: a second call must not build
        // a second one (which would also print a second `MyLm-init`).
        if (LogManager.getLogManager() != lm) {
            throw new AssertionError("getLogManager() is not a singleton: two distinct instances");
        }

        String expected = (requested == null) ? STOCK : requested;
        if (!expected.equals(actual)) {
            throw new AssertionError(
                    PROP + "=" + requested + " but getLogManager() is a " + actual
                            + " (expected " + expected + ")");
        }

        if (requested == null) {
            // The subclass must not have been loaded at all in the default case.
            if (MyLm.inits != 0) {
                throw new AssertionError("MyLm ctor ran " + MyLm.inits
                        + " time(s) with no " + PROP + " set");
            }
        } else {
            if (!(lm instanceof MyLm)) {
                throw new AssertionError("manager is not an instance of MyLm: " + actual);
            }
            if (MyLm.inits != 1) {
                throw new AssertionError("MyLm ctor ran " + MyLm.inits + " time(s), expected 1");
            }
        }

        // The instance above must be the one actually servicing java.util.logging,
        // not merely a constructed-and-discarded object: a logger demanded through
        // the public API has to be findable through this manager.
        String name = "lm.subclass.probe";
        Logger logger = Logger.getLogger(name);
        if (logger == null) {
            throw new AssertionError("Logger.getLogger(" + name + ") returned null");
        }
        Logger viaManager = lm.getLogger(name);
        if (viaManager != logger) {
            throw new AssertionError("manager " + actual + " does not own the logger " + name
                    + " (getLogger returned " + viaManager + ")");
        }

        System.out.println("OK");
    }
}
