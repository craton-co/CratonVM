// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.ManagementFactory;
import javax.management.MBeanServer;
import javax.management.ObjectName;

/**
 * gcj j2/ob2 -- capture probe: the raw text of one DiagnosticCommand MBean
 * operation ({@code vmFlags}, {@code vmCommandLine}, {@code vmUptime},
 * {@code vmSymboltable}, {@code vmStringtable}, ...) with its arguments, on
 * stdout between {@code ---begin} and {@code ---end}. For RECORD rows only:
 * the text carries pids, paths, times and sizes, so no row compares it raw
 * ({@code docs/known-issues/gc/gcj-j2ob2-launcher-and-serviceability-review-residuals-20261019.md}).
 *
 * <pre>
 *   java -XX:+UseSerialGC -Xmx256m Gcj2Ob2DcmdText vmFlags
 *   java -XX:+UseSerialGC -Xmx256m Gcj2Ob2DcmdText vmStringtable -verbose
 * </pre>
 */
public class Gcj2Ob2DcmdText {
    public static void main(String[] args) throws Exception {
        String op = args.length > 0 ? args[0] : "vmFlags";
        String[] rest = new String[Math.max(0, args.length - 1)];
        System.arraycopy(args, Math.min(1, args.length), rest, 0, rest.length);
        MBeanServer server = ManagementFactory.getPlatformMBeanServer();
        ObjectName on = new ObjectName("com.sun.management:type=DiagnosticCommand");
        if (!server.isRegistered(on)) {
            System.out.println("registered false");
            return;
        }
        Object r;
        try {
            r = server.invoke(on, op, new Object[] {rest},
                    new String[] {String[].class.getName()});
        } catch (javax.management.ReflectionException e) {
            // A command without arguments is an operation without parameters.
            r = server.invoke(on, op, new Object[0], new String[0]);
        }
        System.out.println("---begin " + op);
        System.out.print(r);
        System.out.println();
        System.out.println("---end " + op);
    }
}
