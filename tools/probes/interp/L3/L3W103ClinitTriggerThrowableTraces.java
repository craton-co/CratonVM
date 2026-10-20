import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.util.*;

/**
 * Interpreter round i5 wave 103, lane L3 (suspected; page
 * i103-L3-clinit-trigger-throwable-traces-lack-served-door-frames): the
 * companion of L3W100ClinitTriggerWalks for the OTHER two stack captures.
 * Inside a static initializer, HotSpot's `new Throwable().getStackTrace()`
 * and `Thread.currentThread().getStackTrace()` list the same JDK frames a
 * StackWalker does between the <clinit> and the trigger (Field.getInt ..
 * MethodHandleAccessorFactory.newFieldAccessor .. Unsafe.ensureClassInitialized0;
 * VarHandle's LazyInitializingVarHandle chain; Class.forName0; ...). CratonVM
 * inserts a served door's class-initialization chain into StackWalker walks
 * only (vm_exec.rs insert_served_init_chain_walk_entries); a throwable built
 * inside the <clinit> is not the mark's own raise, so the throwable capture
 * leaves the mark and lists nothing (--jdk-only Field doors, static
 * VarHandles, both modes).
 *
 * Each row prints class::method of the frames strictly between the
 * initializer and the trigger method, hidden-class addresses folded.
 *
 * Run: java -cp . L3W103ClinitTriggerThrowableTraces (both modes, JIT and
 * --nojit). Expected: whatever HotSpot 25 prints; the "new"/"static" rows are
 * empty lists there.
 */
public class L3W103ClinitTriggerThrowableTraces {
    static final List<String> LOG = new ArrayList<>();

    static String fold(String s) {
        return s.replaceAll("([/.])0x[0-9a-f]+", "$1ADDR")
                .replaceAll("\\$\\$Lambda[/$]?[0-9a-zA-Z]*", "\\$\\$Lambda");
    }

    static List<String> between(StackTraceElement[] st) {
        List<String> out = new ArrayList<>();
        boolean inInit = false;
        for (StackTraceElement e : st) {
            if (!inInit) {
                if (e.getMethodName().equals("<clinit>")) inInit = true;
                continue;
            }
            if (e.getClassName().equals(L3W103ClinitTriggerThrowableTraces.class.getName())) {
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
        LOG.add(label + " throwable: " + between(new Throwable().getStackTrace()));
        LOG.add(label + " thread: " + between(Thread.currentThread().getStackTrace()));
        // The StackWalker answer, for comparison (L3W100's default row; here
        // also for the override accessor's chain, `Field.setInt`).
        LOG.add(label + " walker: " + between(StackWalker.getInstance().walk(s ->
                s.map(StackWalker.StackFrame::toStackTraceElement).toArray(StackTraceElement[]::new))));
    }

    static class ByNew { static { record("new"); } }
    static class ByStatic { static { record("static"); } static void m() {} }
    static class ByInvoke { static { record("Method.invoke"); } public static void m() {} }
    static class ByCtor { static { record("Constructor.newInstance"); } public ByCtor() {} }
    static class ByForName { static { record("Class.forName"); } }
    static class ByMh { static { record("MethodHandle.invokeExact"); } static void m() {} }
    static class ByEnsure { static { record("Lookup.ensureInitialized"); } }
    static class ByVh { static int x; static { record("VarHandle.get"); } }
    static class ByGetField { static int x = 1; static { record("Field.getInt"); } }
    static class BySetField { static int x = 1; static { record("Field.setInt"); } }

    static void triggerNew() { new ByNew(); }
    static void triggerStatic() { ByStatic.m(); }
    static void triggerInvoke() throws Exception { ByInvoke.class.getMethod("m").invoke(null); }
    static void triggerCtor() throws Exception { ByCtor.class.getConstructor().newInstance(); }
    static void triggerForName() throws Exception { Class.forName(ByForName.class.getName()); }
    static void triggerMh() throws Throwable {
        MethodHandle mh = MethodHandles.lookup().findStatic(ByMh.class, "m", MethodType.methodType(void.class));
        mh.invokeExact();
    }
    static void triggerEnsure() throws Exception { MethodHandles.lookup().ensureInitialized(ByEnsure.class); }
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
        triggerNew();
        triggerStatic();
        triggerInvoke();
        triggerCtor();
        triggerForName();
        triggerMh();
        triggerEnsure();
        triggerVh();
        triggerGetField();
        triggerSetField();
        for (String s : LOG) System.out.println(s);
    }
}
