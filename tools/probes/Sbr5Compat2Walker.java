import java.util.*;
import java.util.stream.*;

/**
 * SBR round 5, lane compat2: StackWalker / Signal / Runtime against HotSpot. The walker is the JDK's
 * own object, a frame is a StackFrameInfo, Signal messages come from the JDK's constructor, Runtime
 * is the real singleton. Every line must be byte-identical under HotSpot, --jdk-only, --compatible.
 */
public class Sbr5Compat2Walker {
    static String norm(Object v) {
        return String.valueOf(v).replaceAll("@[0-9a-f]+", "@X").replaceAll("in java.library.path(: ?.*)?$", "in java.library.path");
    }

    static void row(String name, Object v) {
        System.out.println(name + " |" + norm(v) + "|");
    }

    interface T {
        Object run() throws Throwable;
    }

    static void t(String name, T r) {
        try {
            row(name, r.run());
        } catch (Throwable e) {
            row(name, "THREW " + e.getClass().getName() + ": " + norm(e.getMessage()));
        }
    }

    static Object deep(int n, T t) throws Throwable {
        return n == 0 ? t.run() : deep(n - 1, t);
    }

    public static void main(String[] args) throws Throwable {
        StackWalker w = StackWalker.getInstance();
        StackWalker r = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE);
        t("w.class", () -> w.getClass().getName());
        t("w.fields", () -> Arrays.stream(StackWalker.class.getDeclaredFields())
                .map(f -> java.lang.reflect.Modifier.toString(f.getModifiers()) + " " + f.getType().getName() + " " + f.getName())
                .sorted().collect(Collectors.toList()));
        t("frame.class", () -> w.walk(s -> s.findFirst()).get().getClass().getName());
        t("frame.class.retain", () -> r.walk(s -> s.findFirst()).get().getClass().getName());
        t("frame.toString.method", () -> w.walk(s -> s.findFirst()).get().getMethodName());
        t("frame.toStackTraceElement", () -> w.walk(s -> s.findFirst()).get().toStackTraceElement().getMethodName());
        t("frame.getDeclaringClass", () -> r.walk(s -> s.findFirst()).get().getDeclaringClass().getName());
        t("frame.getDeclaringClass.noretain", () -> w.walk(s -> s.findFirst()).get().getDeclaringClass());
        t("frame.byteCodeIndex>=0", () -> w.walk(s -> s.findFirst()).get().getByteCodeIndex() >= 0);
        t("frame.descriptor", () -> w.walk(s -> s.findFirst()).get().getDescriptor());
        t("frame.methodType.retain", () -> r.walk(s -> s.findFirst()).get().getMethodType().toString());
        t("frame.methodType.noretain", () -> w.walk(s -> s.findFirst()).get().getMethodType());
        t("frame.isNative", () -> w.walk(s -> s.findFirst()).get().isNativeMethod());
        t("walk.depth", () -> (long) deep(7, () -> w.walk(s -> s.filter(f -> f.getMethodName().equals("deep")).count())));
        t("walk.collect.names", () -> (Object) deep(3, () -> w.walk(s -> s.limit(6).map(StackWalker.StackFrame::getMethodName)
                .collect(Collectors.toList()))));
        t("walk.escaped.stream", () -> {
            Stream<StackWalker.StackFrame> leaked = w.walk(s -> s);
            return leaked.count();
        });
        t("walk.null", () -> w.walk(null));
        t("forEach.count", () -> {
            int[] n = {0};
            w.forEach(f -> n[0]++);
            return n[0] >= 2;
        });
        t("getCallerClass.noretain", () -> w.getCallerClass());
        t("getCallerClass.retain.main", () -> r.getCallerClass());
        t("getInstance.set.empty", () -> StackWalker.getInstance(Set.of()).getClass().getName());
        t("getInstance.set.null", () -> StackWalker.getInstance((Set<StackWalker.Option>) null));
        t("getInstance.opt.null", () -> StackWalker.getInstance((StackWalker.Option) null));
        t("getInstance.depth0", () -> StackWalker.getInstance(Set.of(), 0));
        t("getInstance.depth.neg", () -> StackWalker.getInstance(Set.of(), -1));
        t("getInstance.show", () -> StackWalker.getInstance(StackWalker.Option.SHOW_HIDDEN_FRAMES).walk(s -> s.count() >= 1));

        // Signal
        t("sig.INT", () -> new sun.misc.Signal("INT").getName() + " " + new sun.misc.Signal("INT").getNumber());
        t("sig.TERM.number", () -> new sun.misc.Signal("TERM").getNumber());
        t("sig.toString", () -> new sun.misc.Signal("TERM").toString());
        t("sig.hashCode.eq", () -> new sun.misc.Signal("TERM").hashCode() == new sun.misc.Signal("TERM").hashCode());
        t("sig.equals", () -> new sun.misc.Signal("TERM").equals(new sun.misc.Signal("TERM")));
        t("sig.notequals", () -> new sun.misc.Signal("TERM").equals(new sun.misc.Signal("INT")));
        t("sig.SIGINT", () -> new sun.misc.Signal("SIGINT"));
        t("sig.NOPE", () -> new sun.misc.Signal("NOPE"));
        t("sig.null", () -> new sun.misc.Signal(null));
        t("sig.empty", () -> new sun.misc.Signal(""));
        t("sig.handle.usr1", () -> {
            sun.misc.Signal s = new sun.misc.Signal("USR1");
            var old = sun.misc.Signal.handle(s, sig -> System.out.println("handled " + sig.getName()));
            Object ret = old == sun.misc.SignalHandler.SIG_DFL ? "SIG_DFL" : String.valueOf(old.getClass().getName());
            sun.misc.Signal.raise(s);
            Thread.sleep(300);
            sun.misc.Signal.handle(s, sun.misc.SignalHandler.SIG_DFL);
            return ret;
        });
        t("sig.handle.segv", () -> sun.misc.Signal.handle(new sun.misc.Signal("SEGV"), sig -> { }));
        t("sig.handle.null", () -> sun.misc.Signal.handle(new sun.misc.Signal("USR1"), null));
        t("sig.raise.null", () -> { sun.misc.Signal.raise(null); return "ok"; });
        t("sig.class.fields", () -> Arrays.stream(sun.misc.Signal.class.getDeclaredFields())
                .map(f -> f.getType().getSimpleName() + " " + f.getName()).sorted().collect(Collectors.toList()));
        t("jsig.class.fields", () -> {
            Class<?> c = Class.forName("jdk.internal.misc.Signal");
            return Arrays.stream(c.getDeclaredFields()).map(f -> f.getType().getSimpleName() + " " + f.getName())
                    .sorted().collect(Collectors.toList());
        });

        // Runtime
        t("rt.same", () -> Runtime.getRuntime() == Runtime.getRuntime());
        t("rt.class", () -> Runtime.getRuntime().getClass().getName());
        t("rt.version.same", () -> Runtime.version() == Runtime.version());
        t("rt.version.class", () -> Runtime.version().getClass().getName());
        t("rt.version.feature", () -> Runtime.version().feature() == Integer.parseInt(System.getProperty("java.specification.version")));
        t("rt.version.str", () -> Runtime.version().toString().equals(System.getProperty("java.runtime.version")));
        t("rt.version.parse", () -> Runtime.Version.parse("21.0.1-ea+12-LTS").toString());
        t("rt.version.parse.bad", () -> Runtime.Version.parse("x"));
        t("rt.version.compare", () -> Runtime.Version.parse("17.0.2").compareTo(Runtime.Version.parse("17.0.10")));
        t("rt.version.optional", () -> Runtime.Version.parse("21.0.1+12-LTS").optional());
        t("rt.hook.add.remove", () -> {
            Thread th = new Thread(() -> { });
            Runtime.getRuntime().addShutdownHook(th);
            boolean a = Runtime.getRuntime().removeShutdownHook(th);
            boolean b = Runtime.getRuntime().removeShutdownHook(th);
            return a + " " + b;
        });
        t("rt.hook.dup", () -> {
            Thread th = new Thread(() -> { });
            Runtime.getRuntime().addShutdownHook(th);
            try {
                Runtime.getRuntime().addShutdownHook(th);
                return "accepted";
            } finally {
                Runtime.getRuntime().removeShutdownHook(th);
            }
        });
        t("rt.hook.started", () -> {
            Thread th = new Thread(() -> { });
            th.start();
            th.join();
            Runtime.getRuntime().addShutdownHook(th);
            return "accepted";
        });
        t("rt.hook.null", () -> { Runtime.getRuntime().addShutdownHook(null); return "ok"; });
        t("rt.loadLibrary.missing", () -> { Runtime.getRuntime().loadLibrary("no_such_lib_sbr5"); return "ok"; });
        t("rt.load.relative", () -> { Runtime.getRuntime().load("relative/libx.so"); return "ok"; });
        t("rt.load.missing", () -> { Runtime.getRuntime().load("/no/such/libx.so"); return "ok"; });
        t("rt.loadLibrary.null", () -> { Runtime.getRuntime().loadLibrary(null); return "ok"; });
        t("rt.loadLibrary.slash", () -> { Runtime.getRuntime().loadLibrary("a/b"); return "ok"; });
        t("system.loadLibrary.null", () -> { System.loadLibrary(null); return "ok"; });
        t("system.load.relative", () -> { System.load("relative/libx.so"); return "ok"; });
        t("system.load.missing", () -> { System.load("/no/such/libx.so"); return "ok"; });
        t("system.loadLibrary.missing", () -> { System.loadLibrary("no_such_lib_sbr5"); return "ok"; });
        t("rt.availableProcessors>0", () -> Runtime.getRuntime().availableProcessors() > 0);
        t("rt.maxMemory>0", () -> Runtime.getRuntime().maxMemory() > 0);
        Runtime.getRuntime().addShutdownHook(new Thread(() -> System.out.println("hook ran")));
    }
}
