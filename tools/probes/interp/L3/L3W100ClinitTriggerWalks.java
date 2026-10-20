import java.lang.StackWalker.Option;
import java.lang.StackWalker.StackFrame;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.util.*;

/**
 * Interpreter round i5 wave 100, lane L3 (suspected, not fixed): what a
 * StackWalker sees from inside a static initializer, by the thing that ran
 * the initializer. HotSpot runs <clinit> as an ordinary Java frame on top of
 * the frame that triggered it (an interpreted `new`/`invokestatic`, a native
 * such as Unsafe.ensureClassInitialized0 or Class.forName0, or a method-handle
 * LambdaForm). CratonVM initializes classes from several Rust doors
 * (reflective accessors, MH linkage, VarHandle field bases) whose frames may
 * be missing or extra.
 *
 * Each row prints the frames between the initializer and main (exclusive),
 * class::method, with hidden-class addresses folded, for a default walker,
 * SHOW_REFLECT_FRAMES and SHOW_HIDDEN_FRAMES, plus getCallerClass() from the
 * initializer. Frames are printed only up to main, so the launcher does not
 * matter.
 *
 * Run: java -cp . L3W100ClinitTriggerWalks
 * Expected: whatever HotSpot 25 prints (diff the two outputs). The `new` and
 * `static` rows are an empty list on HotSpot with every walker; the getCaller
 * rows name the class whose frame is directly below <clinit>.
 */
public class L3W100ClinitTriggerWalks {
    static final List<String> LOG = new ArrayList<>();

    static String fold(String s) {
        return s.replaceAll("([/.])0x[0-9a-f]+", "$1ADDR").replaceAll("\\$\\$Lambda[/$]?[0-9a-zA-Z]*", "\\$\\$Lambda");
    }

    static List<String> walk(StackWalker w) {
        return w.walk(s -> {
            List<String> out = new ArrayList<>();
            boolean inInit = false;
            for (StackFrame f : (Iterable<StackFrame>) s::iterator) {
                String id = f.getClassName() + "::" + f.getMethodName();
                if (f.getClassName().equals(L3W100ClinitTriggerWalks.class.getName()) && f.getMethodName().equals("main")) break;
                if (!inInit) {
                    if (f.getMethodName().equals("<clinit>")) inInit = true;
                    continue;
                }
                if (f.getClassName().equals(L3W100ClinitTriggerWalks.class.getName()) && f.getMethodName().startsWith("trigger")) {
                    out.add("trigger");
                    break;
                }
                out.add(fold(id));
            }
            return out;
        });
    }

    static void record(String label) {
        LOG.add(label + " default: " + walk(StackWalker.getInstance()));
        LOG.add(label + " reflect: " + walk(StackWalker.getInstance(Option.SHOW_REFLECT_FRAMES)));
        LOG.add(label + " hidden: " + walk(StackWalker.getInstance(Option.SHOW_HIDDEN_FRAMES)));
        Class<?> caller;
        try {
            caller = StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE).getCallerClass();
            LOG.add(label + " caller: " + fold(caller.getName()));
        } catch (Throwable t) {
            LOG.add(label + " caller: " + t.getClass().getName());
        }
    }

    static class ByNew { static { record("new"); } }
    static class ByStatic { static { record("static"); } static void m() {} }
    static class ByInvoke { static { record("Method.invoke"); } public static void m() {} }
    static class ByCtor { static { record("Constructor.newInstance"); } public ByCtor() {} }
    static class ByForName { static { record("Class.forName"); } }
    static class ByMh { static { record("MethodHandle.invokeExact"); } static void m() {} }
    static class ByEnsure { static { record("Lookup.ensureInitialized"); } }
    static class ByVh { static int x; static { record("VarHandle.get"); } }
    static class ByLambda { static { record("lambda"); } static void m() {} }
    static class ByGetField { static int x = 1; static { record("Field.getInt"); } }

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
    static void triggerLambda() { Runnable r = ByLambda::m; r.run(); }
    static void triggerGetField() throws Exception { ByGetField.class.getDeclaredField("x").getInt(null); }

    public static void main(String[] args) throws Throwable {
        triggerNew();
        triggerStatic();
        triggerInvoke();
        triggerCtor();
        triggerForName();
        triggerMh();
        triggerEnsure();
        triggerVh();
        triggerLambda();
        triggerGetField();
        for (String s : LOG) System.out.println(s);
    }
}
