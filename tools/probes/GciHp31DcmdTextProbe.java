// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.ManagementFactory;
import javax.management.Descriptor;
import javax.management.MBeanInfo;
import javax.management.MBeanOperationInfo;
import javax.management.MBeanServer;
import javax.management.ObjectName;

/**
 * gci i31/hp -- the DiagnosticCommand MBean's command texts
 * ({@code i30-ob-report.md} review item 1): each operation's descriptor fields
 * {@code dcmd.description} and {@code dcmd.vmImpact} for the commands both VMs
 * register. HotSpot 25 answers {@code GC.run} with {@code Call
 * java.lang.System.gc().} / {@code Medium: Depends on Java heap size and
 * content.}; CratonVM answered {@code Trigger garbage collection} /
 * {@code Medium}. The MBean is registered on CratonVM only under
 * {@code CRATONVM_JMX_DIAGNOSTIC_COMMAND_MBEAN=1}.
 *
 * <pre>
 *   java     -Xmx64m GciHp31DcmdTextProbe
 *   CRATONVM_JMX_DIAGNOSTIC_COMMAND_MBEAN=1 cratonvm -Xmx64m GciHp31DcmdTextProbe
 * </pre>
 *
 * stdout: {@code registered true}, then one line per command in the fixed
 * order below, {@code <name> | <description> | <impact>} (a command the VM
 * does not list prints {@code <name> | absent}), then {@code done}.
 * stderr: {@code [hp31] operations <n>}.
 */
public class GciHp31DcmdTextProbe {
    static final String[] NAMES = {
        "GC.class_histogram", "GC.heap_dump", "GC.heap_info", "GC.run",
        "JFR.dump", "JFR.start", "JFR.stop",
        "Thread.dump_to_file", "Thread.print", "VM.command_line", "VM.flags",
        "VM.system_properties", "VM.uptime", "VM.version", "help",
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
        MBeanInfo info = server.getMBeanInfo(on);
        MBeanOperationInfo[] ops = info.getOperations();
        System.err.println("[hp31] operations " + ops.length);
        for (String name : NAMES) {
            String line = name + " | absent";
            for (MBeanOperationInfo op : ops) {
                Descriptor d = op.getDescriptor();
                Object n = d.getFieldValue("dcmd.name");
                if (name.equals(n)) {
                    line = name + " | " + d.getFieldValue("dcmd.description")
                            + " | " + d.getFieldValue("dcmd.vmImpact");
                    break;
                }
            }
            System.out.println(line);
        }
        System.out.println("done");
    }
}
