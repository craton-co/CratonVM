// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.logging.LogManager;
import java.util.logging.Logger;

/**
 * gci i1/wr: does the first application {@code Thread.start()} initialise
 * {@code java.util.logging}? HotSpot does not: {@code LogManager.<clinit>}
 * runs on the first use of the logging API, and only then. Page
 * {@code docs/internal/gc/gch-h1ak-first-thread-start-initialises-java-util-logging-RETIRED-UNFIXED-20261018.md}.
 *
 * The probe names its own {@code LogManager} subclass through
 * {@code java.util.logging.manager} (set in {@code main}, before any use, as
 * Tomcat's {@code LoggingBaseTest} does) and prints whether that manager has
 * been constructed at three points: before a thread start, after one, and
 * after the first real use. HotSpot 25 prints {@code false}, {@code false},
 * {@code true}. stdout is deterministic; nothing goes to stderr.
 */
public class GciWr1JulInitProbe {
    static volatile boolean constructed;

    /** Public with a public no-arg constructor: {@code LogManager}'s
     *  initialisation instantiates it reflectively. */
    public static class Mgr extends LogManager {
        public Mgr() {
            super();
            constructed = true;
        }
    }

    public static void main(String[] args) throws Exception {
        System.setProperty("java.util.logging.manager", Mgr.class.getName());
        System.out.println("before start: constructed=" + constructed);
        Thread t = new Thread(new Runnable() {
            @Override
            public void run() {
                // nothing: the start itself is the question
            }
        });
        t.start();
        t.join();
        System.out.println("after start: constructed=" + constructed);
        Logger logger = Logger.getLogger("gci.wr1.probe");
        System.out.println("after use: constructed=" + constructed
                + " manager=" + LogManager.getLogManager().getClass().getSimpleName()
                + " logger=" + logger.getName());
    }
}
