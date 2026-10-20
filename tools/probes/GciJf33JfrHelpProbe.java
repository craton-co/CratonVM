// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.ManagementFactory;
import javax.management.MBeanServer;
import javax.management.ObjectName;

/**
 * gci i33/jf -- page {@code gci-i32jh-jfr-dcmd-help-is-not-the-jdks-printhelp-20261015}.
 * HotSpot 25's {@code HelpDCmd} prints a command's name, description,
 * {@code Impact: ...} and an empty line, then the command's
 * {@code print_help}; for a {@code JFR.*} command that is the JDK's own
 * {@code jdk.jfr.internal.dcmd.DCmd*.getHelp()} text, which CratonVM printed
 * as a generic one-row {@code Syntax : JFR.x  [<options>]} page. The page's
 * second item: {@code GC.finalizer_info} and {@code GC.run_finalization} were
 * not registered at all. Read through the DiagnosticCommand MBean (registered
 * on CratonVM only under {@code CRATONVM_JMX_DIAGNOSTIC_COMMAND_MBEAN=1}).
 *
 * <pre>
 *   java     -Xmx64m GciJf33JfrHelpProbe
 *   CRATONVM_JMX_DIAGNOSTIC_COMMAND_MBEAN=1 cratonvm -Xmx64m GciJf33JfrHelpProbe
 * </pre>
 *
 * stdout: {@code registered true}; {@code listed <name> <true|false>} for the
 * two GC commands (from the {@code help} listing); for each name below a line
 * {@code == <name>} followed by its {@code help} page verbatim; {@code done}.
 * stderr: {@code [jf33] <name> <length>} per page.
 */
public class GciJf33JfrHelpProbe {
    static final String[] PAGES = {
        "JFR.start", "JFR.dump", "JFR.stop", "GC.finalizer_info", "GC.run_finalization",
    };

    static String help(MBeanServer server, ObjectName on, String[] args) {
        try {
            Object out = server.invoke(on, "help", new Object[] {args},
                    new String[] {String[].class.getName()});
            return String.valueOf(out);
        } catch (Exception e) {
            return "threw " + e.getClass().getName() + "\n";
        }
    }

    public static void main(String[] args) throws Exception {
        MBeanServer server = ManagementFactory.getPlatformMBeanServer();
        ObjectName on = new ObjectName("com.sun.management:type=DiagnosticCommand");
        boolean registered = server.isRegistered(on);
        System.out.println("registered " + registered);
        if (!registered) {
            System.out.println("done");
            return;
        }
        String listing = help(server, on, new String[0]);
        for (String name : new String[] {"GC.finalizer_info", "GC.run_finalization"}) {
            boolean listed = false;
            for (String line : listing.split("\n")) {
                if (line.equals(name)) {
                    listed = true;
                }
            }
            System.out.println("listed " + name + " " + listed);
        }
        for (String name : PAGES) {
            String text = help(server, on, new String[] {name});
            System.err.println("[jf33] " + name + " " + text.length());
            System.out.println("== " + name);
            System.out.print(text);
        }
        System.out.println("done");
    }
}
