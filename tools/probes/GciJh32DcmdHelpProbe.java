// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.ManagementFactory;
import javax.management.MBeanServer;
import javax.management.ObjectName;

/**
 * gci i32/jh -- {@code help <command>} pages through the DiagnosticCommand
 * MBean ({@code i31-hp-report.md} review item 1). HotSpot 25's
 * {@code HelpDCmd} prints the name, the description, {@code Impact: ...}, an
 * empty line (JDK 25 has no {@code Permission:} line), then
 * {@code Syntax: <name>} for a parserless command or
 * {@code Syntax : <name> [options] <args>} with the {@code Arguments:} /
 * {@code Options:} blocks ({@code \t<name> : [optional] <description>
 * (<TYPE>, <default>)}). CratonVM printed neither the options nor the types
 * and defaults, and {@code help help} answered "No such command". The MBean
 * is registered on CratonVM only under
 * {@code CRATONVM_JMX_DIAGNOSTIC_COMMAND_MBEAN=1}.
 *
 * <pre>
 *   java     -Xmx64m GciJh32DcmdHelpProbe
 *   CRATONVM_JMX_DIAGNOSTIC_COMMAND_MBEAN=1 cratonvm -Xmx64m GciJh32DcmdHelpProbe
 * </pre>
 *
 * stdout: {@code registered true}, then for each command in the fixed order
 * below a line {@code == <name>} followed by the page verbatim, then
 * {@code done}. stderr: {@code [jh32] <name> <length>} per page.
 */
public class GciJh32DcmdHelpProbe {
    static final String[] NAMES = {
        "GC.class_histogram", "GC.heap_dump", "GC.heap_info", "GC.run",
        "Thread.dump_to_file", "Thread.print", "VM.command_line", "VM.flags",
        "VM.system_properties", "VM.uptime", "VM.version", "help", "No.such_command",
    };

    public static void main(String[] args) throws Exception {
        MBeanServer server = ManagementFactory.getPlatformMBeanServer();
        ObjectName on = new ObjectName("com.sun.management:type=DiagnosticCommand");
        boolean registered = server.isRegistered(on);
        System.out.println("registered " + registered);
        if (!registered) {
            System.out.println("done");
            return;
        }
        for (String name : NAMES) {
            String text;
            try {
                Object out = server.invoke(on, "help", new Object[] {new String[] {name}},
                        new String[] {String[].class.getName()});
                text = String.valueOf(out);
            } catch (Exception e) {
                text = "threw " + e.getClass().getName() + "\n";
            }
            System.err.println("[jh32] " + name + " " + text.length());
            System.out.println("== " + name);
            System.out.print(text);
        }
        System.out.println("done");
    }
}
