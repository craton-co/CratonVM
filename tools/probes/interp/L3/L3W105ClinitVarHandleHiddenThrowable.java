import java.lang.StackWalker.Option;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.util.*;

/**
 * Interpreter round i5 wave 105, lane L3 (residue of page
 * i103-L3-clinit-trigger-throwable-traces-lack-served-door-frames): under
 * -XX:+ShowHiddenFrames a throwable keeps hidden frames, so a throwable built
 * inside a <clinit> that a static VarHandle's first access triggers lists the
 * hidden VarHandleGuards.guard_* frame between the trigger and the
 * VarHandleInts$FieldStaticReadOnly.get .. Unsafe.ensureClassInitialized0
 * chain, exactly as a SHOW_HIDDEN_FRAMES walk does. CratonVM listed the chain
 * without the guard in the throwable (the walk had it since wave 102).
 *
 * Run (both modes, JIT and --nojit; the flag is the point of the probe):
 *   java -XX:+UnlockDiagnosticVMOptions -XX:+ShowHiddenFrames -cp . L3W105ClinitVarHandleHiddenThrowable
 * Expected HotSpot 25: the "throwable" and "hidden walk" rows are equal and
 * name a java.lang.invoke.VarHandleGuards::guard_* frame right below "trigger"
 * (outermost of the JDK frames), and the last row is "equal: true". Without
 * the flag the "throwable" row has no VarHandleGuards frame and "equal" is
 * false on both VMs.
 */
public class L3W105ClinitVarHandleHiddenThrowable {
    static final List<String> LOG = new ArrayList<>();

    static List<String> between(StackTraceElement[] st) {
        List<String> out = new ArrayList<>();
        boolean inInit = false;
        for (StackTraceElement e : st) {
            if (!inInit) {
                if (e.getMethodName().equals("<clinit>")) inInit = true;
                continue;
            }
            if (e.getClassName().equals(L3W105ClinitVarHandleHiddenThrowable.class.getName())) {
                if (e.getMethodName().equals("trigger")) {
                    out.add("trigger");
                    break;
                }
                if (e.getMethodName().equals("main")) break;
            }
            out.add(e.getClassName().replaceAll("([/.])0x[0-9a-f]+", "$1ADDR") + "::" + e.getMethodName());
        }
        return out;
    }

    static class ByVh {
        static int x;
        static {
            List<String> thrown = between(new Throwable().getStackTrace());
            List<String> walked = between(StackWalker.getInstance(Option.SHOW_HIDDEN_FRAMES).walk(s ->
                    s.map(StackWalker.StackFrame::toStackTraceElement).toArray(StackTraceElement[]::new)));
            LOG.add("throwable: " + thrown);
            LOG.add("hidden walk: " + walked);
            LOG.add("equal: " + thrown.equals(walked));
        }
    }

    static void trigger() throws Exception {
        VarHandle vh = MethodHandles.lookup().findStaticVarHandle(ByVh.class, "x", int.class);
        int v = (int) vh.get();
    }

    public static void main(String[] args) throws Exception {
        trigger();
        for (String s : LOG) System.out.println(s);
    }
}
