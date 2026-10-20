// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.ManagementFactory;
import javax.management.MBeanServer;
import javax.management.ObjectName;

/**
 * gci i30/ob -- {@code help -all} through the DiagnosticCommand MBean
 * ({@code i28-dg-report.md} review item 2). HotSpot 25's {@code HelpDCmd}
 * with {@code -all} prints, for every command in name order, the name, a tab
 * and the description, and an empty line. CratonVM read {@code -all} as a
 * command name ({@code Help unavailable : '-all' : No such command}). The MBean
 * is registered here only under {@code CRATONVM_JMX_DIAGNOSTIC_COMMAND_MBEAN=1}.
 * The two VMs list different commands, so only the SHAPE is printed.
 *
 * <pre>
 *   java     -Xmx64m GciOb30DcmdHelpAllProbe
 *   CRATONVM_JMX_DIAGNOSTIC_COMMAND_MBEAN=1 cratonvm -Xmx64m GciOb30DcmdHelpAllProbe
 * </pre>
 *
 * Expected stdout (HotSpot 25, deterministic):
 * <pre>
 *   registered true
 *   shape name-tab-blank true
 *   sorted true
 *   has GC.run true
 *   has help true
 *   no-such-command false
 *   done
 * </pre>
 * stderr: {@code [ob30] entries <n>} and the first entry.
 */
public class GciOb30DcmdHelpAllProbe {
    public static void main(String[] args) throws Exception {
        MBeanServer server = ManagementFactory.getPlatformMBeanServer();
        ObjectName on = new ObjectName("com.sun.management:type=DiagnosticCommand");
        boolean registered = server.isRegistered(on);
        System.out.println("registered " + registered);
        if (!registered) {
            System.out.println("done");
            return;
        }
        Object out = server.invoke(on, "help", new Object[] {new String[] {"-all"}},
                new String[] {String[].class.getName()});
        String text = String.valueOf(out);
        String[] lines = text.split("\n", -1);
        boolean shape = lines.length >= 4;
        int entries = 0;
        String previous = null;
        boolean sorted = true;
        boolean hasGcRun = false;
        boolean hasHelp = false;
        int i = 0;
        while (shape && i + 2 < lines.length) {
            String name = lines[i];
            String desc = lines[i + 1];
            String blank = lines[i + 2];
            if (name.isEmpty() || name.startsWith("\t") || !desc.startsWith("\t") || !blank.isEmpty()) {
                shape = false;
                break;
            }
            if (previous != null && previous.compareTo(name) > 0) {
                sorted = false;
            }
            previous = name;
            hasGcRun |= name.equals("GC.run");
            hasHelp |= name.equals("help");
            entries++;
            i += 3;
        }
        // What remains after the last entry: the final newline's empty string.
        if (shape && !(i == lines.length - 1 && lines[i].isEmpty())) {
            shape = false;
        }
        System.err.println("[ob30] entries " + entries + " first " + (lines.length > 0 ? lines[0] : ""));
        System.out.println("shape name-tab-blank " + shape);
        System.out.println("sorted " + sorted);
        System.out.println("has GC.run " + hasGcRun);
        System.out.println("has help " + hasHelp);
        System.out.println("no-such-command " + text.contains("No such command"));
        System.out.println("done");
    }
}
