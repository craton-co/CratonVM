// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.util.ArrayList;
import java.util.List;
import java.util.TreeSet;

/**
 * gci i22/fl: which threads can Java see after the Generational concurrent cycle ran?
 *
 * <p>The d8 gate of {@code CRATONVM_GEN_CONC_SERVICE_THREAD}
 * ({@code gc-defects-round-20260927/triage.md}): the service thread registers with no
 * {@code Thread} object, so {@code Thread.getAllStackTraces()} and
 * {@code ThreadMXBean.getThreadCount()} must read the same with the switch on as off.
 * The probe grows a retained ring past the concurrent start level (run it with
 * {@code CRATONVM_GC_CONC_START_PERCENT=40}), churns young garbage so cycles run, sleeps
 * 200 ms (the service thread's 20 ms cadence runs ten times), then prints the sorted
 * names of every live thread and the two counts.
 *
 * <p>Usage: {@code GciFl22ThreadVisibilityProbe <ringMiB> <churnMiB>}. Stdout is compared
 * with the switch-off twin (HotSpot's own thread names differ and are recorded only).
 */
public class GciFl22ThreadVisibilityProbe {
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        int ringMiB = args.length > 0 ? Integer.parseInt(args[0]) : 120;
        int churnMiB = args.length > 1 ? Integer.parseInt(args[1]) : 1024;
        List<byte[]> ring = new ArrayList<>();
        long sum = 0;
        for (int i = 0; i < ringMiB * 16; i++) {
            byte[] a = new byte[65536];
            a[0] = (byte) i;
            ring.add(a);
            for (int j = 0; j < 4; j++) {
                sink = new byte[16384];
            }
        }
        for (long b = 0; b < (long) churnMiB * 1024L * 1024L; b += 8192) {
            sink = new byte[8192];
        }
        for (int i = 0; i < ring.size(); i++) {
            sum += ring.get(i)[0];
        }
        Thread.sleep(200);
        TreeSet<String> names = new TreeSet<>();
        for (Thread t : Thread.getAllStackTraces().keySet()) {
            names.add(t.getName() + (t.isDaemon() ? " daemon" : ""));
        }
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        System.out.println("ring-sum " + sum);
        System.out.println("all-stack-traces " + names.size());
        System.out.println("mx-thread-count " + mx.getThreadCount());
        System.out.println("mx-daemon-count " + mx.getDaemonThreadCount());
        for (String s : names) {
            System.out.println("thread " + s);
        }
        System.out.println("PROBE-OK");
    }
}
