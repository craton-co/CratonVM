import java.lang.StackWalker.Option;
import java.lang.StackWalker.StackFrame;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;
import java.util.*;
import java.util.function.*;

/**
 * Interpreter round i5 wave 104, lane L3 (hunt): StackWalker walks under each
 * option set (default, SHOW_REFLECT_FRAMES, SHOW_HIDDEN_FRAMES) taken inside a
 * target reached by a method handle (invokeExact, invoke with a conversion,
 * invokeWithArguments, a bound handle, a constructor handle, a handle to a
 * reflective Method.invoke), by a lambda and a method reference, by
 * Method.invoke of a static and of an interface default method, and inside
 * a <clinit> run by each of those. Then the throwable trace of the same
 * places, which equals HotSpot's default walk (no hidden frames, reflection
 * frames listed).
 *
 * Frames are class::method from the walk's innermost frame down to the probe's
 * row method (exclusive); hidden-class addresses and lambda indices folded.
 * The SHOW_HIDDEN rows list LambdaForm frames on HotSpot whose names depend on
 * the JDK's spun forms: compare them to HotSpot 25 on the same JDK build.
 *
 * Run: java -cp . L3W104WalkOptionsThroughHandles, both modes, JIT and --nojit.
 * Expected: HotSpot 25's output.
 */
public class L3W104WalkOptionsThroughHandles {
    static final StackWalker DEFAULT = StackWalker.getInstance();
    static final StackWalker REFLECT = StackWalker.getInstance(Option.SHOW_REFLECT_FRAMES);
    static final StackWalker HIDDEN = StackWalker.getInstance(Option.SHOW_HIDDEN_FRAMES);
    static final List<String> LOG = new ArrayList<>();
    static String current = "?";

    static String fold(String s) {
        return s.replaceAll("([/.])0x[0-9a-f]+", "$1ADDR")
                .replaceAll("\\$\\$Lambda[/$]?[0-9a-zA-Z]*", "\\$\\$Lambda")
                .replaceAll("lambda\\$(\\w+)\\$\\d+", "lambda\\$$1\\$N")
                .replaceAll("LambdaForm\\$(\\w+)/ADDR", "LambdaForm\\$$1");
    }

    static List<String> walk(StackWalker w) {
        return w.walk(s -> {
            List<String> out = new ArrayList<>();
            for (StackFrame f : (Iterable<StackFrame>) s::iterator) {
                if (f.getClassName().equals(L3W104WalkOptionsThroughHandles.class.getName())
                        && f.getMethodName().startsWith("row")) break;
                out.add(fold(f.getClassName() + "::" + f.getMethodName()));
            }
            return out;
        });
    }

    static List<String> trace() {
        List<String> out = new ArrayList<>();
        for (StackTraceElement e : new Throwable().getStackTrace()) {
            if (e.getClassName().equals(L3W104WalkOptionsThroughHandles.class.getName())
                    && e.getMethodName().startsWith("row")) break;
            out.add(fold(e.getClassName() + "::" + e.getMethodName()));
        }
        return out;
    }

    /** The probe point: every capture of the current row. */
    public static void target() {
        LOG.add(current + " default: " + walk(DEFAULT));
        LOG.add(current + " reflect: " + walk(REFLECT));
        LOG.add(current + " hidden: " + walk(HIDDEN));
        LOG.add(current + " throwable: " + trace());
    }

    public static int targetInt(int x) { target(); return x + 1; }
    public void targetVirtual() { target(); }
    public L3W104WalkOptionsThroughHandles() { }
    public L3W104WalkOptionsThroughHandles(int marker) { target(); }

    public interface Iface {
        default void dflt() { target(); }
    }
    static class Impl implements Iface { }

    static class InitByHandle { static { target(); } static void m() { } }
    static class InitByLambda { static { target(); } static void m() { } }
    static class InitByInvoke { static { target(); } public static void m() { } }

    static void rowExact() throws Throwable {
        MethodHandle mh = MethodHandles.lookup().findStatic(L3W104WalkOptionsThroughHandles.class, "target",
                MethodType.methodType(void.class));
        mh.invokeExact();
    }
    static void rowInvokeConverting() throws Throwable {
        MethodHandle mh = MethodHandles.lookup().findStatic(L3W104WalkOptionsThroughHandles.class, "targetInt",
                MethodType.methodType(int.class, int.class));
        Object r = mh.invoke(Integer.valueOf(4));
    }
    static void rowWithArguments() throws Throwable {
        MethodHandle mh = MethodHandles.lookup().findStatic(L3W104WalkOptionsThroughHandles.class, "targetInt",
                MethodType.methodType(int.class, int.class));
        mh.invokeWithArguments(5);
    }
    static void rowBound() throws Throwable {
        MethodHandle mh = MethodHandles.lookup().findVirtual(L3W104WalkOptionsThroughHandles.class, "targetVirtual",
                MethodType.methodType(void.class)).bindTo(new L3W104WalkOptionsThroughHandles());
        mh.invokeExact();
    }
    static void rowConstructor() throws Throwable {
        MethodHandle mh = MethodHandles.lookup().findConstructor(L3W104WalkOptionsThroughHandles.class,
                MethodType.methodType(void.class, int.class));
        Object o = mh.invoke(1);
    }
    static void rowHandleToInvoke() throws Throwable {
        Method m = L3W104WalkOptionsThroughHandles.class.getMethod("target");
        MethodHandle mh = MethodHandles.lookup().findVirtual(Method.class, "invoke",
                MethodType.methodType(Object.class, Object.class, Object[].class));
        Object r = mh.invoke(m, null, new Object[0]);
    }
    static void rowLambda() {
        Runnable r = () -> target();
        r.run();
    }
    static void rowMethodRef() {
        Runnable r = L3W104WalkOptionsThroughHandles::target;
        r.run();
    }
    static void rowInvokeStatic() throws Exception {
        L3W104WalkOptionsThroughHandles.class.getMethod("target").invoke(null);
    }
    static void rowInvokeDefault() throws Exception {
        Iface.class.getMethod("dflt").invoke(new Impl());
    }
    static void rowInitByHandle() throws Throwable {
        MethodHandle mh = MethodHandles.lookup().findStatic(InitByHandle.class, "m", MethodType.methodType(void.class));
        mh.invokeExact();
    }
    static void rowInitByLambda() {
        Runnable r = InitByLambda::m;
        r.run();
    }
    static void rowInitByInvoke() throws Exception {
        InitByInvoke.class.getMethod("m").invoke(null);
    }

    interface Row { void run() throws Throwable; }

    static void run(String name, Row r) {
        current = name;
        try {
            r.run();
        } catch (Throwable t) {
            LOG.add(name + " failed: " + t);
        }
    }

    public static void main(String[] args) {
        run("invokeExact", L3W104WalkOptionsThroughHandles::rowExact);
        run("invoke-converting", L3W104WalkOptionsThroughHandles::rowInvokeConverting);
        run("invokeWithArguments", L3W104WalkOptionsThroughHandles::rowWithArguments);
        run("bound", L3W104WalkOptionsThroughHandles::rowBound);
        run("constructor", L3W104WalkOptionsThroughHandles::rowConstructor);
        run("handle-to-Method.invoke", L3W104WalkOptionsThroughHandles::rowHandleToInvoke);
        run("lambda", L3W104WalkOptionsThroughHandles::rowLambda);
        run("method-ref", L3W104WalkOptionsThroughHandles::rowMethodRef);
        run("Method.invoke-static", L3W104WalkOptionsThroughHandles::rowInvokeStatic);
        run("Method.invoke-default", L3W104WalkOptionsThroughHandles::rowInvokeDefault);
        run("clinit-by-handle", L3W104WalkOptionsThroughHandles::rowInitByHandle);
        run("clinit-by-method-ref", L3W104WalkOptionsThroughHandles::rowInitByLambda);
        run("clinit-by-Method.invoke", L3W104WalkOptionsThroughHandles::rowInitByInvoke);
        for (String s : LOG) System.out.println(s);
    }
}
