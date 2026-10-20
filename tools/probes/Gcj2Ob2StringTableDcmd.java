// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.ManagementFactory;
import javax.management.MBeanServer;
import javax.management.ObjectName;

/**
 * gcj j2/ob2 -- {@code jcmd VM.stringtable} reads the live String table
 * ({@code docs/known-issues/gc/gcj-j1st-string-table-logging-and-statistics-are-not-hotspots-20261019.md}
 * item 1). Through the DiagnosticCommand MBean (the in-process door to the
 * same command): the {@code Number of entries} figure before and after
 * interning {@code N} fresh Strings the probe keeps reachable, and whether
 * {@code -verbose} lists one of them.
 *
 * <pre>
 *   java     -XX:+UseSerialGC        -Xmx256m Gcj2Ob2StringTableDcmd 2000
 *   cratonvm -XX:+UseGenerationalGC  -Xmx256m Gcj2Ob2StringTableDcmd 2000
 * </pre>
 *
 * stdout (deterministic): {@code registered true}, {@code grew true},
 * {@code verbose true}, {@code done}. stderr: the two entry counts and the
 * statistics text's first line (byte figures are layout-dependent and never
 * on stdout).
 */
public class Gcj2Ob2StringTableDcmd {
    static String[] keep;

    static long entries(String text) {
        for (String line : text.split("\n")) {
            if (line.startsWith("Number of entries")) {
                String v = line.substring(line.indexOf(':') + 1);
                int eq = v.indexOf('=');
                if (eq >= 0) {
                    v = v.substring(0, eq);
                }
                return Long.parseLong(v.trim());
            }
        }
        return -1;
    }

    static String run(MBeanServer server, ObjectName on, String... args) throws Exception {
        Object r = server.invoke(on, "vmStringtable", new Object[] {args},
                new String[] {String[].class.getName()});
        return String.valueOf(r);
    }

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 2000;
        MBeanServer server = ManagementFactory.getPlatformMBeanServer();
        ObjectName on = new ObjectName("com.sun.management:type=DiagnosticCommand");
        boolean registered = server.isRegistered(on);
        System.out.println("registered " + registered);
        if (!registered) {
            System.out.println("done");
            return;
        }
        String before = run(server, on);
        long e0 = entries(before);
        keep = new String[n];
        for (int i = 0; i < n; i++) {
            keep[i] = ("gcj2ob2-" + i + "-" + (i * 31 + 7)).intern();
        }
        String after = run(server, on);
        long e1 = entries(after);
        System.err.println("[gcj2ob2] entries before=" + e0 + " after=" + e1
                + " first=" + after.split("\n")[0]);
        System.out.println("grew " + (e0 >= 0 && e1 - e0 >= n));
        String verbose = run(server, on, "-verbose");
        System.out.println("verbose " + verbose.contains("gcj2ob2-17-" + (17 * 31 + 7)));
        System.out.println("done");
    }
}
