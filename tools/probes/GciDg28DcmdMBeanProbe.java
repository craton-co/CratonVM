// gci wave i28, lane dg: the `com.sun.management:type=DiagnosticCommand` MBean
// (docs/known-issues/gc/gci-i14mp-memory-pool-and-gc-mxbean-review-findings-20261013.md,
// item 11b). HotSpot 25 registers it in every platform MBean server
// (`VMManagementImpl.remoteDiagnosticCommandsSupport` = 1) over the four
// `DiagnosticCommandImpl` natives; CratonVM registers it only under the opt-in
// `CRATONVM_JMX_DIAGNOSTIC_COMMAND_MBEAN=1`.
//
//   java     -XX:+UseSerialGC -Xmx64m -cp <classes> GciDg28DcmdMBeanProbe
//   cratonvm -Xmx64m -cp <classes> GciDg28DcmdMBeanProbe        (with the switch)
//
// Expected stdout (HotSpot 25; deterministic):
//
//   registered true
//   op gcRun true
//   ... (one `op <name> true` line per name in WANT)
//   gcRun ''
//   vmVersion nonEmpty true
//   help first GC.run
//   noSuchOp ReflectionException NoSuchMethodException
//   gcRun dcmd.name GC.run
//   gcRun dcmd.permissionClass java.lang.management.ManagementPermission
//   gcRun dcmd.permissionName control
//   gcRun signature 0
//   done
//
// stderr (diagnostics only): `[dg28] ops <n> [<sorted operation names>]`.
import java.lang.management.ManagementFactory;
import java.util.TreeSet;
import javax.management.Descriptor;
import javax.management.MBeanInfo;
import javax.management.MBeanOperationInfo;
import javax.management.MBeanServer;
import javax.management.ObjectName;
import javax.management.ReflectionException;

public class GciDg28DcmdMBeanProbe {
    static final String[] WANT = {
        "gcClassHistogram", "gcHeapDump", "gcHeapInfo", "gcRun", "help", "threadPrint",
        "vmCommandLine", "vmFlags", "vmSystemProperties", "vmUptime", "vmVersion",
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
        TreeSet<String> names = new TreeSet<>();
        for (MBeanOperationInfo op : info.getOperations()) {
            names.add(op.getName());
        }
        System.err.println("[dg28] ops " + names.size() + " " + names);
        for (String want : WANT) {
            System.out.println("op " + want + " " + names.contains(want));
        }
        Object gc = server.invoke(on, "gcRun", null, null);
        System.out.println("gcRun '" + gc + "'");
        Object version = server.invoke(on, "vmVersion", null, null);
        System.out.println("vmVersion nonEmpty "
                + (version instanceof String && !((String) version).isEmpty()));
        Object help = server.invoke(on, "help", new Object[] {new String[] {"GC.run"}},
                new String[] {String[].class.getName()});
        System.out.println("help first " + String.valueOf(help).split("\n")[0]);
        try {
            server.invoke(on, "noSuchOp", null, null);
            System.out.println("noSuchOp returned");
        } catch (ReflectionException e) {
            Throwable cause = e.getCause();
            System.out.println("noSuchOp " + e.getClass().getSimpleName() + " "
                    + (cause == null ? "null" : cause.getClass().getSimpleName()));
        }
        for (MBeanOperationInfo op : info.getOperations()) {
            if (op.getName().equals("gcRun")) {
                Descriptor d = op.getDescriptor();
                System.out.println("gcRun dcmd.name " + d.getFieldValue("dcmd.name"));
                System.out.println("gcRun dcmd.permissionClass " + d.getFieldValue("dcmd.permissionClass"));
                System.out.println("gcRun dcmd.permissionName " + d.getFieldValue("dcmd.permissionName"));
                System.out.println("gcRun signature " + op.getSignature().length);
            }
        }
        System.out.println("done");
    }
}
