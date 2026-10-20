import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.util.*;

/**
 * Interpreter round i5 wave 104, lane L3 (page
 * i103-L3-clinit-trigger-throwable-traces-lack-served-door-frames, residue
 * (b)): the VM's own listing of the CURRENT thread
 * (vm_exec.rs thread_stack_trace_served), which a real-JDK boot reaches
 * through Thread.getAllStackTraces (Thread.dumpThreads) and ThreadMXBean
 * (ThreadInfo stacks), did not list a served door's class-init chain below a
 * <clinit> the door runs (throwables and StackWalker walks do since waves
 * 100-103). HotSpot's dumps list the JDK frames the <clinit> runs on, hidden
 * frames included (a thread dump shows hidden frames).
 *
 * Rows: "<trigger> <listing>: [frames strictly between the <clinit> and the
 * trigger]". The "new" / "static" rows are controls (empty lists).
 *
 * Run: java -cp . L3W104ClinitTriggerListings, both modes, JIT and --nojit.
 * Expected: HotSpot 25's output.
 */
public class L3W104ClinitTriggerListings {
    static final List<String> LOG = new ArrayList<>();

    static String fold(String s) {
        return s.replaceAll("([/.])0x[0-9a-f]+", "$1ADDR")
                .replaceAll("\\$\\$Lambda[/$]?[0-9a-zA-Z]*", "\\$\\$Lambda");
    }

    static List<String> between(StackTraceElement[] st) {
        List<String> out = new ArrayList<>();
        if (st == null) {
            out.add("<null>");
            return out;
        }
        boolean inInit = false;
        for (StackTraceElement e : st) {
            if (!inInit) {
                if (e.getMethodName().equals("<clinit>")) inInit = true;
                continue;
            }
            if (e.getClassName().equals(L3W104ClinitTriggerListings.class.getName())) {
                if (e.getMethodName().startsWith("trigger")) {
                    out.add("trigger");
                    break;
                }
                if (e.getMethodName().equals("main")) break;
            }
            out.add(fold(e.getClassName() + "::" + e.getMethodName()));
        }
        return out;
    }

    static void record(String label) {
        Thread me = Thread.currentThread();
        LOG.add(label + " getAllStackTraces: " + between(Thread.getAllStackTraces().get(me)));
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        ThreadInfo[] infos = mx.getThreadInfo(new long[] {me.threadId()}, false, false);
        LOG.add(label + " ThreadInfo: " + between(infos[0] == null ? null : infos[0].getStackTrace()));
        ThreadInfo info = mx.getThreadInfo(me.threadId(), Integer.MAX_VALUE);
        LOG.add(label + " ThreadInfo(depth): " + between(info == null ? null : info.getStackTrace()));
        LOG.add(label + " throwable: " + between(new Throwable().getStackTrace()));
    }

    static class ByNew { static { record("new"); } }
    static class ByStatic { static { record("static"); } static void m() {} }
    static class ByVh { static int x; static { record("VarHandle.get"); } }
    static class ByGetField { static int x = 1; static { record("Field.getInt"); } }
    static class BySetField { static int x = 1; static { record("Field.setInt"); } }

    static void triggerNew() { new ByNew(); }
    static void triggerStatic() { ByStatic.m(); }
    static void triggerVh() throws Exception {
        VarHandle vh = MethodHandles.lookup().findStaticVarHandle(ByVh.class, "x", int.class);
        int v = (int) vh.get();
    }
    static void triggerGetField() throws Exception { ByGetField.class.getDeclaredField("x").getInt(null); }
    static void triggerSetField() throws Exception {
        java.lang.reflect.Field f = BySetField.class.getDeclaredField("x");
        f.setAccessible(true);
        f.setInt(null, 2);
    }

    public static void main(String[] args) throws Throwable {
        // Warm the management classes outside any initializer.
        ManagementFactory.getThreadMXBean().getThreadInfo(Thread.currentThread().threadId());
        Thread.getAllStackTraces();
        triggerNew();
        triggerStatic();
        triggerVh();
        triggerGetField();
        triggerSetField();
        for (String s : LOG) System.out.println(s);
    }
}
