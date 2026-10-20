// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.EnumSet;
import java.util.List;
import java.util.Set;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import java.util.function.Supplier;
import java.util.stream.Collectors;

/**
 * SBR wave 5, lane {@code fjp2}: the rows wave 4 FJP held --
 * {@code StackWalker.walk}/{@code forEach}/{@code getCallerClass} (the JDK's
 * {@code StackStreamFactory} path), {@code Runtime.load0}/{@code loadLibrary0}
 * (the real {@code ClassLoader.loadLibrary} over the loaders' real
 * {@code NativeLibraries}), {@code jdk.internal.misc.Signal.raise} and
 * {@code sun.misc.Signal} (a carrier holding a real
 * {@code jdk.internal.misc.Signal}), and the stream
 * {@code MemorySegment.elements} hands out.
 *
 * <p>Every row is evaluated ONCE and printed (the first-run value), then
 * re-evaluated inside a hot loop and compared with that first value;
 * {@code drift} counts mismatches, so cold == warm is asserted. Diff the whole
 * output against HotSpot 25. Nothing printed depends on paths, identity hashes
 * or scheduling order.
 *
 * <pre>
 *   java --enable-native-access=ALL-UNNAMED SbrW5Fjp2 [iters]      (HotSpot oracle)
 *   cratonvm --enable-native-access=ALL-UNNAMED -cp . SbrW5Fjp2 [iters]
 * </pre>
 */
public class SbrW5Fjp2 {
    static final List<String> LABELS = new ArrayList<>();
    static final List<Supplier<String>> ROWS = new ArrayList<>();

    interface X {
        Object get() throws Throwable;
    }

    static void rowx(String label, X x) {
        LABELS.add(label);
        ROWS.add(() -> {
            try {
                return String.valueOf(x.get());
            } catch (Throwable t) {
                return "!" + t.getClass().getName() + ": " + t.getMessage();
            }
        });
    }

    static String safe(Supplier<String> s) {
        try {
            return s.get();
        } catch (Throwable t) {
            return "!" + t.getClass().getName() + ": " + t.getMessage();
        }
    }

    /** Throwable -> class name plus whether its message names {@code what} (paths never printed). */
    static String err(Throwable t, String what) {
        String m = String.valueOf(t.getMessage());
        return t.getClass().getName() + " names=" + m.contains(what);
    }

    // ---------------------------------------------------------------- StackWalker fixtures

    static String frames(java.util.stream.Stream<StackWalker.StackFrame> s) {
        return s.filter(f -> f.getClassName().startsWith("SbrW5Fjp2"))
                .limit(3)
                .map(f -> f.getMethodName())
                .collect(Collectors.joining(","));
    }

    static String deep(int n) {
        if (n == 0) {
            return StackWalker.getInstance().walk(s -> s.filter(f -> f.getMethodName().equals("deep"))
                    .count()) + "";
        }
        return deep(n - 1);
    }

    static final StackWalker RETAIN = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE);

    static Class<?> callerOf() {
        return RETAIN.getCallerClass();
    }

    static final class Helper {
        static Class<?> who() {
            return RETAIN.getCallerClass();
        }

        public static String reflective() {
            return RETAIN.getCallerClass().getName();
        }

        public static String walkReflect() {
            return StackWalker.getInstance(StackWalker.Option.SHOW_REFLECT_FRAMES)
                    .walk(s -> s.limit(3).map(f -> f.getClassName() + "." + f.getMethodName())
                            .collect(Collectors.joining(",")));
        }

        public static String walkNoReflect() {
            return StackWalker.getInstance()
                    .walk(s -> s.limit(2).map(f -> f.getClassName() + "." + f.getMethodName())
                            .collect(Collectors.joining(",")));
        }
    }

    static String nested() {
        return StackWalker.getInstance().walk(s -> s.limit(1).map(f -> f.getMethodName() + "/"
                + StackWalker.getInstance().walk(t -> t.limit(2).map(StackWalker.StackFrame::getMethodName)
                        .collect(Collectors.joining(","))))
                .collect(Collectors.joining()));
    }

    static long depthOf(int n) {
        if (n == 0) {
            return StackWalker.getInstance().walk(s -> s.count());
        }
        return depthOf(n - 1);
    }

    // ---------------------------------------------------------------- main

    public static void main(String[] args) throws Exception {
        // ------------------------------------------------ StackWalker
        rowx("sw.walk filter/map", () -> StackWalker.getInstance().walk(SbrW5Fjp2::frames));
        rowx("sw.walk deep", () -> deep(5));
        rowx("sw.walk deep 40", () -> deep(40));
        rowx("sw.walk depth delta", () -> depthOf(30) - depthOf(0));
        rowx("sw.walk count>=3", () -> StackWalker.getInstance().walk(s -> s.count()) >= 3);
        rowx("sw.walk first frame", () -> StackWalker.getInstance().walk(s -> s.findFirst())
                .map(f -> f.getClassName() + "." + f.getMethodName()).orElse("none"));
        rowx("sw.walk skip/limit", () -> StackWalker.getInstance()
                .walk(s -> s.skip(1).limit(1).map(StackWalker.StackFrame::getMethodName).collect(Collectors.toList())));
        rowx("sw.walk nested", SbrW5Fjp2::nested);
        rowx("sw.frame class", () -> StackWalker.getInstance().walk(s -> s.findFirst()).get().getClass().getName());
        rowx("sw.frame toStackTraceElement", () -> StackWalker.getInstance().walk(s -> s.findFirst()).get()
                .toStackTraceElement().getMethodName());
        rowx("sw.frame toString prefix", () -> {
            String t = StackWalker.getInstance().walk(s -> s.findFirst()).get().toString();
            return t.substring(0, t.indexOf('('));
        });
        rowx("sw.frame file/line>0", () -> {
            var f = StackWalker.getInstance().walk(s -> s.findFirst()).get();
            return f.getFileName() + " " + (f.getLineNumber() > 0);
        });
        rowx("sw.frame bci>=0 native", () -> {
            var f = StackWalker.getInstance().walk(s -> s.findFirst()).get();
            return (f.getByteCodeIndex() >= 0) + " " + f.isNativeMethod();
        });
        rowx("sw.frame getDeclaringClass no-retain", () -> StackWalker.getInstance().walk(s -> s.findFirst()).get()
                .getDeclaringClass());
        rowx("sw.frame getDeclaringClass retain", () -> RETAIN.walk(s -> s.findFirst()).get()
                .getDeclaringClass().getName());
        rowx("sw.frame getMethodType", () -> RETAIN.walk(s -> s.skip(1).findFirst()).get().getMethodType());
        rowx("sw.frame descriptor", () -> RETAIN.walk(s -> s.skip(1).findFirst()).get().getDescriptor());
        rowx("sw.getInstance(Set)", () -> StackWalker.getInstance(EnumSet.of(StackWalker.Option.RETAIN_CLASS_REFERENCE,
                StackWalker.Option.SHOW_REFLECT_FRAMES)).walk(s -> s.findFirst()).get().getDeclaringClass().getSimpleName());
        rowx("sw.getInstance(Set,int)", () -> StackWalker.getInstance(Set.of(), 4).walk(s -> s.limit(1).count()));
        rowx("sw.walk via reflection", () -> Helper.class.getMethod("walkNoReflect").invoke(null));
        rowx("sw.walk via reflection SHOW_REFLECT", () -> {
            String s = (String) Helper.class.getMethod("walkReflect").invoke(null);
            return s.substring(0, s.indexOf(','));
        });
        rowx("sw.getCallerClass", () -> callerOf().getName());
        rowx("sw.getCallerClass nested class", () -> Helper.who().getName());
        rowx("sw.getCallerClass via reflection", () -> {
            Method m = Helper.class.getMethod("reflective");
            return m.invoke(null);
        });
        rowx("sw.getCallerClass lambda", () -> ((Supplier<Class<?>>) () -> RETAIN.getCallerClass()).get().getName());
        rowx("sw.getCallerClass no-retain", () -> StackWalker.getInstance().getCallerClass());
        rowx("sw.getCallerClass from main-frame thread", () -> {
            AtomicReference<String> r = new AtomicReference<>();
            Thread t = new Thread(() -> r.set(safe(() -> callerOf().getName())));
            t.start();
            t.join();
            return r.get();
        });
        rowx("sw.forEach", () -> {
            List<String> names = new ArrayList<>();
            StackWalker.getInstance().forEach(f -> {
                if (f.getClassName().startsWith("SbrW5Fjp2") && names.size() < 2) names.add(f.getMethodName());
            });
            return names;
        });
        rowx("sw.forEach null NPE", () -> {
            StackWalker.getInstance().forEach(null);
            return "no-throw";
        });
        rowx("sw.walk null fn NPE", () -> StackWalker.getInstance().walk(null));
        rowx("sw.walk fn throws", () -> StackWalker.getInstance().walk(s -> {
            throw new IllegalStateException("boom");
        }));
        rowx("sw.stream escapes", () -> {
            java.util.stream.Stream<StackWalker.StackFrame> leaked = StackWalker.getInstance().walk(s -> s);
            try {
                return leaked.count();
            } catch (IllegalStateException e) {
                return "ISE " + e.getMessage();
            }
        });
        rowx("sw.walk other thread", () -> {
            AtomicReference<String> r = new AtomicReference<>();
            Thread t = new Thread(() -> r.set(safe(() -> StackWalker.getInstance()
                    .walk(s -> s.map(StackWalker.StackFrame::getMethodName).collect(Collectors.joining(","))))));
            t.start();
            t.join();
            return r.get();
        });

        // ------------------------------------------------ Runtime load / loadLibrary
        rowx("rt.loadLibrary missing", () -> {
            String n = "cratonvm_fjp2_no_such_lib";
            try {
                Runtime.getRuntime().loadLibrary(n);
                return "loaded";
            } catch (UnsatisfiedLinkError e) {
                return err(e, n);
            }
        });
        rowx("rt.load missing abs", () -> {
            String p = new java.io.File("cratonvm-fjp2-no-such.so").getAbsolutePath();
            try {
                Runtime.getRuntime().load(p);
                return "loaded";
            } catch (UnsatisfiedLinkError e) {
                return err(e, p) + " prefix=" + String.valueOf(e.getMessage()).startsWith("Can't load library");
            }
        });
        rowx("rt.load relative", () -> {
            try {
                Runtime.getRuntime().load("relative.so");
                return "loaded";
            } catch (UnsatisfiedLinkError e) {
                return e.getClass().getName() + " " + e.getMessage();
            }
        });
        rowx("rt.loadLibrary separator", () -> {
            try {
                Runtime.getRuntime().loadLibrary("a/b");
                return "loaded";
            } catch (UnsatisfiedLinkError e) {
                return e.getClass().getName() + " " + e.getMessage();
            }
        });
        rowx("rt.loadLibrary null NPE", () -> {
            Runtime.getRuntime().loadLibrary(null);
            return "no-throw";
        });
        rowx("rt.loadLibrary net twice", () -> {
            String a, b;
            try {
                Runtime.getRuntime().loadLibrary("net");
                a = "ok";
            } catch (UnsatisfiedLinkError e) {
                a = err(e, "net");
            }
            try {
                Runtime.getRuntime().loadLibrary("net");
                b = "ok";
            } catch (UnsatisfiedLinkError e) {
                b = err(e, "net");
            }
            return a + " / " + b;
        });
        rowx("sys.loadLibrary missing", () -> {
            try {
                System.loadLibrary("cratonvm_fjp2_no_such_lib2");
                return "loaded";
            } catch (UnsatisfiedLinkError e) {
                return err(e, "cratonvm_fjp2_no_such_lib2");
            }
        });

        // ------------------------------------------------ Signal
        rowx("sig INT", () -> {
            sun.misc.Signal s = new sun.misc.Signal("INT");
            return s.getName() + " " + s.getNumber() + " " + s;
        });
        rowx("sig TERM equals/hash", () -> new sun.misc.Signal("TERM").equals(new sun.misc.Signal("TERM"))
                + " " + (new sun.misc.Signal("TERM").hashCode() == new sun.misc.Signal("TERM").getNumber())
                + " " + new sun.misc.Signal("TERM").equals(new sun.misc.Signal("INT")));
        rowx("sig SIGINT IAE", () -> new sun.misc.Signal("SIGINT"));
        rowx("sig NOPE IAE", () -> new sun.misc.Signal("NOPE"));
        rowx("sig null NPE", () -> new sun.misc.Signal(null));
        for (String name : new String[] {"HUP", "INT", "TERM", "USR1", "USR2", "QUIT", "ALRM", "WINCH", "CONT",
                "SEGV", "FPE", "BUS", "ILL", "PIPE", "TRAP"}) {
            rowx("sig handle " + name, () -> {
                sun.misc.Signal s = new sun.misc.Signal(name);
                sun.misc.SignalHandler h = sig -> {};
                sun.misc.SignalHandler old = sun.misc.Signal.handle(s, h);
                sun.misc.SignalHandler again = sun.misc.Signal.handle(s, old);
                return (old == sun.misc.SignalHandler.SIG_DFL ? "DFL" : old == sun.misc.SignalHandler.SIG_IGN ? "IGN"
                        : old.getClass().getName()) + " back=" + (again == h);
            });
        }
        rowx("sig raise USR1 handler thread", () -> {
            sun.misc.Signal s = new sun.misc.Signal("USR1");
            CountDownLatch l = new CountDownLatch(1);
            AtomicReference<String> seen = new AtomicReference<>();
            sun.misc.SignalHandler old = sun.misc.Signal.handle(s, sig -> {
                seen.set(sig.getName() + " " + (sig == s) + " " + Thread.currentThread().getName());
                l.countDown();
            });
            try {
                sun.misc.Signal.raise(s);
                boolean ok = l.await(10, TimeUnit.SECONDS);
                return ok + " " + seen.get();
            } finally {
                sun.misc.Signal.handle(s, old);
            }
        });
        rowx("sig raise unhandled IAE", () -> {
            sun.misc.Signal.raise(new sun.misc.Signal("ALRM"));
            return "no-throw";
        });
        // Not an NPE on HotSpot: sun.misc wraps the null. Restored at once, so
        // the row is idempotent (it used to leave USR1 holding the null).
        rowx("sig handle null handler", () -> {
            sun.misc.Signal s = new sun.misc.Signal("WINCH");
            sun.misc.SignalHandler old = sun.misc.Signal.handle(s, null);
            sun.misc.SignalHandler back = sun.misc.Signal.handle(s, old);
            return (old == sun.misc.SignalHandler.SIG_DFL) + " " + (back == null);
        });

        // ------------------------------------------------ FFM elements stream
        rowx("ffm.elements skip/count", () -> MemorySegment.ofArray(new int[] {1, 2, 3, 4})
                .elements(ValueLayout.JAVA_INT).skip(1).count());
        rowx("ffm.elements skip/map", () -> MemorySegment.ofArray(new int[] {1, 2, 3, 4})
                .elements(ValueLayout.JAVA_INT).skip(1).map(m -> m.get(ValueLayout.JAVA_INT, 0))
                .collect(Collectors.toList()));
        rowx("ffm.elements native sum", () -> {
            try (Arena a = Arena.ofConfined()) {
                MemorySegment m = a.allocate(ValueLayout.JAVA_LONG, 5);
                for (int i = 0; i < 5; i++) m.setAtIndex(ValueLayout.JAVA_LONG, i, i * 10L);
                return m.elements(ValueLayout.JAVA_LONG).skip(2).mapToLong(x -> x.get(ValueLayout.JAVA_LONG, 0)).sum();
            }
        });
        rowx("ffm.elements stream class", () -> MemorySegment.ofArray(new byte[4]).elements(ValueLayout.JAVA_BYTE)
                .getClass().getName().startsWith("java.util.stream."));
        rowx("ffm.elements parallel", () -> MemorySegment.ofArray(new long[] {1, 2, 3, 4, 5, 6, 7, 8})
                .elements(ValueLayout.JAVA_LONG).isParallel());

        String[] first = new String[ROWS.size()];
        for (int i = 0; i < ROWS.size(); i++) {
            first[i] = safe(ROWS.get(i));
            System.out.println("row " + LABELS.get(i) + " = " + first[i]);
        }
        int drift = 0;
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        for (int k = 0; k < iters; k++) {
            for (int i = 0; i < ROWS.size(); i++) {
                String v = safe(ROWS.get(i));
                if (!v.equals(first[i])) {
                    if (drift < 10) {
                        System.out.println("DRIFT at iter " + k + " " + LABELS.get(i) + " = " + v);
                    }
                    drift++;
                }
            }
        }
        System.out.println("rows " + ROWS.size() + " drift " + drift);
    }
}
