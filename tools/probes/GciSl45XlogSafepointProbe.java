// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.ManagementFactory;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * gci i45/sl ({@code gci-i44sp-xlog-safepoint-prints-nothing-20261016}): the
 * {@code -Xlog:safepoint} lines against HotSpot 25. Run with
 * {@code -Xlog:safepoint:file=/tmp/sl45_%p.log -Dsl45.log=/tmp/sl45_%p.log}
 * (the {@code %p} keeps each run's file its own; the probe substitutes its
 * pid in the property).
 *
 * <p>Args: {@code <op> <n> [gc|dump]}. {@code gc} (default): {@code n}
 * {@code System.gc()} calls; {@code dump}: one live heap dump through
 * {@code HotSpotDiagnosticMXBean.dumpHeap}. Then the log file is read and only
 * predicates are printed, so stdout is deterministic on both VMs (the figures
 * are timings).
 *
 * <p>stdout:
 * <pre>
 * sl45 lines_present=true     (at least one safepoint line)
 * sl45 op_count_ge_n=true     (at least n lines name the VM operation op)
 * sl45 shape_ok=true          (every line is SafepointTracing::end's shape)
 * sl45 tag_ok=true            (every line carries the safepoint tag decoration)
 * sl45 total_is_sum=true      (Total = Reaching + At safepoint + Leaving safepoint)
 * </pre>
 * stderr (diagnostic): the lines naming op, and the count of each name.
 */
public class GciSl45XlogSafepointProbe {
    // gci i47w/j45: HotSpot 25's shape, read from Temurin 25.0.3's own log
    // (rows vi45-sl S): no "Cleanup" (JDK 21's), then "Leaving safepoint" and
    // "Threads: R runnable, T total". Total = Reaching + At + Leaving.
    private static final Pattern LINE = Pattern.compile(
            "Safepoint \"([A-Za-z0-9_]+)\", Time since last: (\\d+) ns, Reaching safepoint: (\\d+) ns, "
                    + "At safepoint: (\\d+) ns, Leaving safepoint: (\\d+) ns, Total: (\\d+) ns, "
                    + "Threads: (\\d+) runnable, (\\d+) total$");

    public static void main(String[] args) throws Exception {
        String op = args.length > 0 ? args[0] : "SerialGCCollect";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 5;
        String mode = args.length > 2 ? args[2] : "gc";
        if (mode.equals("dump")) {
            Path out = Files.createTempDirectory("sl45").resolve("sl45.hprof");
            ManagementFactory.getPlatformMXBean(com.sun.management.HotSpotDiagnosticMXBean.class)
                    .dumpHeap(out.toString(), true);
            Files.deleteIfExists(out);
            Files.deleteIfExists(out.getParent());
        } else {
            for (int i = 0; i < n; i++) {
                System.gc();
                Thread.sleep(2);
            }
        }
        // Let the last pause's line land (both VMs write it at the pause's end).
        Thread.sleep(200);

        String template = System.getProperty("sl45.log", "/tmp/sl45_%p.log");
        Path log = Path.of(template.replace("%p", Long.toString(ProcessHandle.current().pid())));
        List<String> all = Files.exists(log) ? Files.readAllLines(log) : List.of();
        int lines = 0;
        int named = 0;
        boolean shape = true;
        boolean tag = true;
        boolean sum = true;
        java.util.TreeMap<String, Integer> names = new java.util.TreeMap<>();
        for (String l : all) {
            int at = l.indexOf("Safepoint \"");
            if (at < 0) {
                continue;
            }
            lines++;
            tag &= l.substring(0, at).contains("[safepoint");
            Matcher m = LINE.matcher(l.substring(at));
            if (!m.matches()) {
                shape = false;
                System.err.println("sl45 bad shape: " + l);
                continue;
            }
            names.merge(m.group(1), 1, Integer::sum);
            if (m.group(1).equals(op)) {
                named++;
                System.err.println("sl45 line: " + l);
            }
            long reach = Long.parseLong(m.group(3));
            long atSp = Long.parseLong(m.group(4));
            long leave = Long.parseLong(m.group(5));
            long total = Long.parseLong(m.group(6));
            sum &= total == reach + atSp + leave;
        }
        System.err.println("sl45 log " + log + " names " + names);
        System.out.println("sl45 lines_present=" + (lines > 0));
        System.out.println("sl45 op_count_ge_n=" + (named >= n));
        System.out.println("sl45 shape_ok=" + shape);
        System.out.println("sl45 tag_ok=" + tag);
        System.out.println("sl45 total_is_sum=" + sum);
    }
}
