import java.lang.StackWalker.Option;
import java.lang.StackWalker.StackFrame;
import java.util.*;
import java.util.function.*;

/**
 * Interpreter round i5 wave 102, lane L3: a SHOW_HIDDEN_FRAMES walk from a
 * static initializer that a lambda proxy's forwarding call triggered. HotSpot
 * runs the proxy's SAM method (hidden) and its `invokestatic` / `new` of the
 * implementation's class runs the `<clinit>` on top of it, so the walk lists
 * `$$Lambda::<sam>` between the `<clinit>` and the call site. CratonVM's
 * lambda dispatch pushes no proxy frame; the stand-in rule
 * (`stackwalker::lambda_site_initializes`) rebuilds it.
 *
 * Rows: a static method reference (Runnable), a constructor reference
 * (Supplier, the proxy's `new` triggers the init), a static method reference
 * through a Function, and a lambda body (the init runs inside the host's
 * `lambda$` method, below the proxy frame).
 *
 * Each row prints the frames between the `<clinit>` and the `trigger*` frame
 * (the trigger itself printed as `trigger`), class::method, hidden-class
 * addresses folded. Run: java -cp . L3W102ClinitLambdaShapes, both modes,
 * JIT and --nojit. Expected: whatever HotSpot 25 prints.
 */
public class L3W102ClinitLambdaShapes {
    static final List<String> LOG = new ArrayList<>();

    static String fold(String s) {
        return s.replaceAll("([/.])0x[0-9a-f]+", "$1ADDR")
                .replaceAll("\\$\\$Lambda[/$]?[0-9a-zA-Z]*", "\\$\\$Lambda");
    }

    static List<String> walk(StackWalker w) {
        return w.walk(s -> {
            List<String> out = new ArrayList<>();
            boolean inInit = false;
            for (StackFrame f : (Iterable<StackFrame>) s::iterator) {
                if (!inInit) {
                    if (f.getMethodName().equals("<clinit>")) inInit = true;
                    continue;
                }
                if (f.getClassName().equals(L3W102ClinitLambdaShapes.class.getName())
                        && f.getMethodName().startsWith("trigger")) {
                    out.add("trigger");
                    break;
                }
                out.add(fold(f.getClassName() + "::" + f.getMethodName()));
            }
            return out;
        });
    }

    static void record(String label) {
        LOG.add(label + " default: " + walk(StackWalker.getInstance()));
        LOG.add(label + " hidden: " + walk(StackWalker.getInstance(Option.SHOW_HIDDEN_FRAMES)));
    }

    static class ByStaticRef { static { record("static-ref"); } static void m() {} }
    static class ByCtorRef { static { record("ctor-ref"); } ByCtorRef() {} }
    static class ByFunction { static { record("function-ref"); } static Integer twice(Integer x) { return x * 2; } }
    static class ByLambdaBody { static { record("lambda-body"); } static void m() {} }

    static void triggerStaticRef() { Runnable r = ByStaticRef::m; r.run(); }
    static void triggerCtorRef() { Supplier<ByCtorRef> s = ByCtorRef::new; s.get(); }
    static void triggerFunction() { Function<Integer, Integer> f = ByFunction::twice; f.apply(3); }
    static void triggerLambdaBody() { Runnable r = () -> ByLambdaBody.m(); r.run(); }

    public static void main(String[] args) {
        triggerStaticRef();
        triggerCtorRef();
        triggerFunction();
        triggerLambdaBody();
        for (String s : LOG) System.out.println(s);
    }
}
