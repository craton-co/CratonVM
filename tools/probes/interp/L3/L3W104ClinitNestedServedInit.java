import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.lang.reflect.Field;
import java.util.*;

/**
 * Interpreter round i5 wave 104, lane L3 (page
 * i103-L3-clinit-trigger-throwable-traces-lack-served-door-frames, residue
 * (a)): inside a static initializer a served door runs (a Field value door
 * under --jdk-only, a static VarHandle's holder in both modes), a NESTED
 * served initialization, or a served raise caught inside the <clinit>,
 * replaced the thread's one class-init chain mark and withdrew it, so every
 * capture and walk made later in the same <clinit> listed the <clinit>
 * directly on the trigger (HotSpot keeps listing the door's JDK frames: the
 * <clinit> runs on top of them). Wave 104 keeps the marks as a stack
 * (ServedNativeRaise::outer).
 *
 * Rows: "<door> <when> <capture>: [frames from the innermost <clinit> down to
 * the trigger]". "inner" rows are captured inside the nested <clinit> and
 * list the inner chain, the outer <clinit> and the outer chain. "raise" rows
 * are the caught exception's own trace below the <clinit>.
 *
 * Run: java -cp . L3W104ClinitNestedServedInit, both modes, JIT and --nojit;
 * CRATONVM_THROWABLE_SERVED_INIT_CHAIN_FRAMES=0 gives the old throwable rows.
 * Expected: HotSpot 25's output.
 */
public class L3W104ClinitNestedServedInit {
    static final List<String> LOG = new ArrayList<>();

    static String fold(String s) {
        return s.replaceAll("([/.])0x[0-9a-f]+", "$1ADDR")
                .replaceAll("\\$\\$Lambda[/$]?[0-9a-zA-Z]*", "\\$\\$Lambda");
    }

    static List<String> below(StackTraceElement[] st) {
        List<String> out = new ArrayList<>();
        boolean inInit = false;
        for (StackTraceElement e : st) {
            if (!inInit) {
                if (e.getMethodName().equals("<clinit>")) {
                    inInit = true;
                    out.add(simple(e.getClassName()) + "::<clinit>");
                }
                continue;
            }
            if (e.getClassName().equals(L3W104ClinitNestedServedInit.class.getName())) {
                if (e.getMethodName().startsWith("trigger")) {
                    out.add("trigger");
                    break;
                }
                if (e.getMethodName().equals("main")) break;
            }
            String cn = e.getClassName();
            out.add(cn.startsWith(L3W104ClinitNestedServedInit.class.getName())
                    ? simple(cn) + "::" + e.getMethodName()
                    : fold(cn + "::" + e.getMethodName()));
        }
        return out;
    }

    static String simple(String cn) {
        int d = cn.lastIndexOf('$');
        return d < 0 ? cn : cn.substring(d + 1);
    }

    static void record(String label) {
        LOG.add(label + " throwable: " + below(new Throwable().getStackTrace()));
        LOG.add(label + " walker: " + below(StackWalker.getInstance().walk(s ->
                s.map(StackWalker.StackFrame::toStackTraceElement).toArray(StackTraceElement[]::new))));
    }

    // --- Field.getInt doors (served Intrinsics under --jdk-only) ---
    static class FInner { static int x = 1; static { record("Field inner"); } }
    static class FOuter {
        static int x = 1;
        int inst;
        static {
            record("Field before");
            try {
                FInner.class.getDeclaredField("x").getInt(null);
            } catch (Exception e) {
                LOG.add("Field nested failed " + e);
            }
            record("Field after-nested");
            try {
                FOuter.class.getDeclaredField("inst").getInt(null);
                LOG.add("Field raise: none");
            } catch (NullPointerException e) {
                LOG.add("Field raise: " + below(e.getStackTrace()));
            } catch (Exception e) {
                LOG.add("Field raise other " + e.getClass().getName());
            }
            record("Field after-raise");
        }
    }

    // --- static VarHandle holders (served in both modes) ---
    static class VInner { static int x; static { record("VarHandle inner"); } }
    static class VOuter {
        static int x;
        static {
            record("VarHandle before");
            try {
                VarHandle vh = MethodHandles.lookup().findStaticVarHandle(VInner.class, "x", int.class);
                int v = (int) vh.get();
            } catch (Exception e) {
                LOG.add("VarHandle nested failed " + e);
            }
            record("VarHandle after-nested");
            try {
                // A static VarHandle's served raise caught in the <clinit>:
                // a set through a read-only (final) field's handle.
                VarHandle ro = MethodHandles.lookup().findStaticVarHandle(VRo.class, "K", int.class);
                ro.set(3);
                LOG.add("VarHandle raise: none");
            } catch (UnsupportedOperationException e) {
                LOG.add("VarHandle raise: " + below(e.getStackTrace()));
            } catch (Exception e) {
                LOG.add("VarHandle raise other " + e.getClass().getName());
            }
            record("VarHandle after-raise");
        }
    }
    static class VRo { static final int K = Integer.parseInt("7"); }

    static void triggerField() throws Exception { FOuter.class.getDeclaredField("x").getInt(null); }
    static void triggerVarHandle() throws Exception {
        VarHandle vh = MethodHandles.lookup().findStaticVarHandle(VOuter.class, "x", int.class);
        int v = (int) vh.get();
    }

    public static void main(String[] args) throws Throwable {
        triggerField();
        triggerVarHandle();
        for (String s : LOG) System.out.println(s);
    }
}
