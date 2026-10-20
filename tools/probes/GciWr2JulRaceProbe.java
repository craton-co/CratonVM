// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.Reference;
import java.util.concurrent.CyclicBarrier;
import java.util.logging.LogManager;
import java.util.logging.Logger;

/**
 * gci i2/wr: the concurrent first use of {@code java.util.logging} that the
 * launcher's first-thread-start pre-warm defends
 * ({@code docs/internal/fixed-suite-bugs/tomcat/38-*}; page
 * {@code docs/internal/gc/gch-h1ak-first-thread-start-initialises-java-util-logging-RETIRED-UNFIXED-20261018.md}).
 *
 * {@code threads} threads meet at a barrier and then each makes the process's
 * first {@code Logger.getLogger} call at the same moment. Mode {@code default}
 * uses the JDK's {@code LogManager}; mode {@code parents} first selects (through
 * {@code java.util.logging.manager}, set in {@code main} as Tomcat's
 * {@code LoggingBaseTest} does) a manager whose synchronized {@code addLogger}
 * creates each dotted parent with {@code Logger.getLogger(parent)} first, the
 * shape of {@code org.apache.juli.ClassLoaderLogManager} that the Tomcat
 * WebSocket suite hung in. Run with {@code CRATONVM_JUL_FIRST_START_PREWARM=0}
 * the race is open (no pre-warm); on the default arm the pre-warm runs at the
 * first {@code start()} and the row is a control.
 *
 * stdout is deterministic: one line per thread in index order, then a summary.
 * A thread still running after the join budget prints {@code HUNG} and the
 * process exits 3 (the threads are daemons so the exit is not blocked).
 */
public class GciWr2JulRaceProbe {
    /** Public with a public no-arg constructor: {@code LogManager}'s
     *  initialisation instantiates it reflectively. */
    public static class ParentsFirstManager extends LogManager {
        public ParentsFirstManager() {
            super();
        }

        @Override
        public synchronized boolean addLogger(Logger logger) {
            String name = logger.getName();
            int dot = name.lastIndexOf('.');
            Logger parent = null;
            if (dot > 0) {
                // Create the parent first, as ClassLoaderLogManager does.
                parent = Logger.getLogger(name.substring(0, dot));
            }
            boolean added = super.addLogger(logger);
            // Keep the parent strongly reachable until the child is linked to it.
            Reference.reachabilityFence(parent);
            return added;
        }
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "default";
        int threads = args.length > 1 ? Integer.parseInt(args[1]) : 8;
        long joinMs = args.length > 2 ? Long.parseLong(args[2]) : 60000L;
        if (mode.equals("parents")) {
            System.setProperty("java.util.logging.manager", ParentsFirstManager.class.getName());
        } else if (!mode.equals("default")) {
            System.out.println("usage: GciWr2JulRaceProbe default|parents [threads] [joinMs]");
            System.exit(2);
        }
        final CyclicBarrier barrier = new CyclicBarrier(threads);
        final String[] got = new String[threads];
        final String[] parent = new String[threads];
        final Throwable[] threw = new Throwable[threads];
        Thread[] ts = new Thread[threads];
        for (int i = 0; i < threads; i++) {
            final int k = i;
            ts[i] = new Thread(new Runnable() {
                @Override
                public void run() {
                    try {
                        barrier.await();
                        Logger l = Logger.getLogger("gci.wr2.race.t" + k + ".leaf");
                        got[k] = l.getName();
                        Logger p = l.getParent();
                        parent[k] = p == null ? "null" : p.getName();
                    } catch (Throwable t) {
                        threw[k] = t;
                    }
                }
            }, "jul-race-" + i);
            ts[i].setDaemon(true);
        }
        for (Thread t : ts) {
            t.start();
        }
        long deadline = System.currentTimeMillis() + joinMs;
        int hung = 0;
        for (Thread t : ts) {
            long left = deadline - System.currentTimeMillis();
            if (left > 0) {
                t.join(left);
            }
            if (t.isAlive()) {
                hung++;
            }
        }
        int ok = 0;
        for (int i = 0; i < threads; i++) {
            if (ts[i].isAlive()) {
                System.out.println("t" + i + " HUNG");
            } else if (threw[i] != null) {
                System.out.println("t" + i + " threw " + threw[i].getClass().getName());
            } else {
                boolean good = ("gci.wr2.race.t" + i + ".leaf").equals(got[i]);
                if (good) {
                    ok++;
                }
                System.out.println("t" + i + " logger=" + got[i] + " parent=" + parent[i]
                        + (good ? "" : " BAD"));
            }
        }
        System.out.println("jul-race: mode=" + mode + " threads=" + threads + " ok=" + ok
                + " hung=" + hung + " manager="
                + (hung == 0 ? LogManager.getLogManager().getClass().getSimpleName() : "?"));
        System.out.flush();
        if (hung > 0) {
            Runtime.getRuntime().halt(3);
        }
    }
}
