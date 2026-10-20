import java.lang.StackWalker.Option;
import java.lang.StackWalker.StackFrame;
import java.lang.reflect.Method;
import java.util.*;
import java.util.function.*;
import java.util.stream.*;

/**
 * Interpreter round i4 wave 98, lane L3: StackWalker options and
 * caller-sensitive answers as HotSpot 25 gives them. Frames are printed as
 * class::method, hidden-class address suffixes folded, and only the frames
 * up to (and including) main, so the launcher's frames do not matter.
 *
 * Rows: default / RETAIN_CLASS_REFERENCE / SHOW_REFLECT_FRAMES /
 * SHOW_HIDDEN_FRAMES walks through a reflective call and a lambda;
 * getDeclaringClass without RETAIN (UnsupportedOperationException message);
 * getCallerClass from main (IllegalCallerException), through reflection, and
 * through a lambda; StackFrame toString / getDescriptor / getMethodType /
 * getByteCodeIndex >= 0 / isNativeMethod; Class.forName caller-sensitivity
 * through Method.invoke.
 */
public class L3W98StackWalkerOptions {
    static String fold(String s) {
        return s.replaceAll("([/.])0x[0-9a-f]+", "$1ADDR").replaceAll("\\$\\$Lambda\\$\\d+", "\\$\\$Lambda");
    }

    static List<String> walk(StackWalker w) {
        return w.walk(s -> {
            List<String> out = new ArrayList<>();
            for (StackFrame f : (Iterable<StackFrame>) s::iterator) {
                out.add(fold(f.getClassName() + "::" + f.getMethodName()));
                if (f.getClassName().equals("L3W98StackWalkerOptions") && f.getMethodName().equals("main")) break;
            }
            return out;
        });
    }

    public static List<String> viaReflect(StackWalker w) {
        return walk(w);
    }

    public static Class<?> callerOf() {
        return StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE).getCallerClass();
    }

    public static Class<?> forNameHere(String n) throws Exception {
        return Class.forName(n);
    }

    static void row(String label, Object v) {
        System.out.println(label + ": " + v);
    }

    public static void main(String[] args) throws Exception {
        Method vr = L3W98StackWalkerOptions.class.getMethod("viaReflect", StackWalker.class);
        Map<String, StackWalker> walkers = new LinkedHashMap<>();
        walkers.put("default", StackWalker.getInstance());
        walkers.put("retain", StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE));
        walkers.put("reflect", StackWalker.getInstance(Option.SHOW_REFLECT_FRAMES));
        walkers.put("hidden", StackWalker.getInstance(Set.of(Option.SHOW_HIDDEN_FRAMES, Option.RETAIN_CLASS_REFERENCE)));
        for (Map.Entry<String, StackWalker> e : walkers.entrySet()) {
            row(e.getKey() + " direct", walk(e.getValue()));
            row(e.getKey() + " reflect", vr.invoke(null, e.getValue()));
            Function<StackWalker, List<String>> fn = L3W98StackWalkerOptions::walk;
            row(e.getKey() + " mref", fn.apply(e.getValue()));
            Supplier<List<String>> lam = () -> walk(e.getValue());
            row(e.getKey() + " lambda", lam.get());
        }

        StackFrame top = StackWalker.getInstance().walk(s -> s.findFirst()).get();
        try {
            top.getDeclaringClass();
            row("declaringClass w/o retain", "no throw");
        } catch (UnsupportedOperationException ex) {
            row("declaringClass w/o retain", ex.getClass().getName() + ": " + ex.getMessage());
        }
        StackFrame rtop = StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE).walk(s -> s.findFirst()).get();
        row("retain declaringClass", rtop.getDeclaringClass().getName());
        row("frame descriptor", rtop.getDescriptor() + " methodType=" + rtop.getMethodType()
                + " bci>=0=" + (rtop.getByteCodeIndex() >= 0) + " native=" + rtop.isNativeMethod()
                + " file=" + rtop.getFileName() + " line>0=" + (rtop.getLineNumber() > 0));
        row("frame toString", rtop.toString().replaceAll(":\\d+\\)", ":N)"));
        row("ste", rtop.toStackTraceElement().toString().replaceAll(":\\d+\\)", ":N)"));
        try {
            top.getMethodType();
            row("methodType w/o retain", "no throw");
        } catch (UnsupportedOperationException ex) {
            row("methodType w/o retain", ex.getClass().getName() + ": " + ex.getMessage());
        }

        try {
            StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE).getCallerClass();
            row("getCallerClass from main", "no throw");
        } catch (IllegalCallerException ex) {
            row("getCallerClass from main", ex.getClass().getName() + ": " + ex.getMessage());
        }
        try {
            StackWalker.getInstance().getCallerClass();
            row("getCallerClass w/o retain", "no throw");
        } catch (UnsupportedOperationException ex) {
            row("getCallerClass w/o retain", ex.getClass().getName() + ": " + ex.getMessage());
        }
        row("callerOf direct", callerOf().getName());
        row("callerOf reflect", ((Class<?>) L3W98StackWalkerOptions.class.getMethod("callerOf").invoke(null)).getName());
        Supplier<Class<?>> sup = L3W98StackWalkerOptions::callerOf;
        row("callerOf mref", fold(sup.get().getName()));
        Supplier<Class<?>> lam = () -> callerOf();
        row("callerOf lambda", fold(lam.get().getName()));
        row("callerOf stream", Stream.of(1).map(i -> callerOf().getName()).map(L3W98StackWalkerOptions::fold).findFirst().get());

        Method forName = Class.class.getMethod("forName", String.class);
        row("forName reflect", ((Class<?>) forName.invoke(null, "L3W98StackWalkerOptions")).getClassLoader() == ClassLoader.getSystemClassLoader());
        row("forName helper reflect", ((Class<?>) L3W98StackWalkerOptions.class.getMethod("forNameHere", String.class)
                .invoke(null, "L3W98StackWalkerOptions")).getName());
        try {
            forName.invoke(null, "no.such.L3W98");
        } catch (java.lang.reflect.InvocationTargetException ex) {
            row("forName missing", ex.getCause().getClass().getName() + ": " + ex.getCause().getMessage());
        }
        Method gcc = StackWalker.class.getMethod("getCallerClass");
        try {
            gcc.invoke(StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE));
            row("getCallerClass via reflection", "no throw");
        } catch (java.lang.reflect.InvocationTargetException ex) {
            row("getCallerClass via reflection", ex.getCause().getClass().getName() + ": " + ex.getCause().getMessage());
        }
        row("depth limited", StackWalker.getInstance(Set.of(), 2).walk(s -> s.map(StackFrame::getMethodName).limit(3).collect(Collectors.toList())));
        try {
            StackWalker.getInstance(Set.of(), 0);
        } catch (IllegalArgumentException ex) {
            row("estimateDepth 0", ex.getClass().getName() + ": " + ex.getMessage());
        }
        row("thread getStackTrace top", fold(Thread.currentThread().getStackTrace()[0].getMethodName())
                + "/" + fold(Thread.currentThread().getStackTrace()[1].getMethodName()));
    }
}
