// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 79, lane L2 -- two `--compatible` stack-trace
// defects, which must read the HotSpot block in both modes:
//
//   compiler-* / base-* / app-*: the `format` bits and the printed module
//       version of a frame of an UPGRADEABLE JDK module (`java.compiler`, not
//       hashed in `java.base`): HotSpot keeps its `@version`
//       (`java.compiler@25.0.4/javax.tools...`); `--compatible` decided by a
//       `java.` / `jdk.` name screen and dropped it, in the element of
//       Throwable.getStackTrace(), Thread.getStackTrace(), the walker's
//       toStackTraceElement() and the printStackTrace text.
//       docs/internal/fixed-bugs/interpreter-i74-L2-compatible-stack-trace-format-uses-a-module-name-screen-FIXED-20261111.md,
//       docs/internal/fixed-bugs/interpreter-i77-L2-compatible-format-bits-screen-module-names-FIXED-20261111.md
//   race-*: StackFrameInfo.toStackTraceElement() asked for the same frames by
//       eight threads at once answers ONE element per frame (the JDK body
//       memoises under `synchronized (this)`); `--compatible`'s native stored
//       without a lock.
//       docs/internal/fixed-bugs/interpreter-i77-L2-compatible-walker-element-memo-unsynchronized-FIXED-20261111.md
//
// The module version is printed as "<v>" and the "(File.java:NN)" tail is cut,
// so the block does not depend on the update release or on line numbers.
//
// Run each of (stdout must be the block below, byte for byte):
//   $J/javac -d out tools/probes/interp/L2/L2W79StackTraceDefects.java
//   $B --java-home $J/.. -cp out L2W79StackTraceDefects
//   $B --compatible --java-home $J/.. -cp out L2W79StackTraceDefects
//   $B --compatible --nojit --java-home $J/.. -cp out L2W79StackTraceDefects
//   $J/java -cp out L2W79StackTraceDefects
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// compiler-throwable java.compiler ver=true loader=platform java.compiler@<v>/javax.tools.ForwardingJavaFileManager.getClassLoader
// compiler-thread java.compiler ver=true loader=platform java.compiler@<v>/javax.tools.ForwardingJavaFileManager.getClassLoader
// compiler-walker java.compiler ver=true loader=platform java.compiler@<v>/javax.tools.ForwardingJavaFileManager.getClassLoader
// compiler-print <TAB>at java.compiler@<v>/javax.tools.ForwardingJavaFileManager.getClassLoader
// base-throwable java.base ver=true loader=null java.base/java.util.ArrayList.forEach
// base-print <TAB>at java.base/java.util.ArrayList.forEach
// app-throwable null ver=false loader=app L2W79StackTraceDefects$Grab.accept
// race-frames true
// race-identity true
// race-main-identity true
// HOTSPOT_EXPECTED_END
import java.io.PrintWriter;
import java.io.StringWriter;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.function.Consumer;
import java.util.function.Function;
import java.util.stream.Stream;
import javax.tools.ForwardingJavaFileManager;
import javax.tools.JavaFileManager;
import javax.tools.StandardLocation;

public class L2W79StackTraceDefects {
    static final String FWD = "javax.tools.ForwardingJavaFileManager";

    static StackTraceElement thrownCompiler;
    static StackTraceElement threadCompiler;
    static StackTraceElement walkedCompiler;
    static String printedCompiler;

    static String shape(StackTraceElement e) {
        if (e == null) {
            return "missing";
        }
        String text = e.toString();
        String v = e.getModuleVersion();
        if (v != null && !v.isEmpty()) {
            text = text.replace("@" + v, "@<v>");
        }
        int paren = text.indexOf('(');
        if (paren >= 0) {
            text = text.substring(0, paren);
        }
        return e.getModuleName() + " ver=" + (v != null) + " loader=" + e.getClassLoaderName() + " " + text;
    }

    static StackTraceElement find(StackTraceElement[] st, String cls) {
        for (StackTraceElement e : st) {
            if (e.getClassName().equals(cls)) {
                return e;
            }
        }
        return null;
    }

    static String printedLine(Throwable t, String needle) {
        StringWriter sw = new StringWriter();
        t.printStackTrace(new PrintWriter(sw));
        for (String line : sw.toString().split("\n")) {
            if (line.contains(needle)) {
                String text = line.replace("\r", "").replace("\t", "<TAB>");
                int at = text.indexOf("@");
                int slash = text.indexOf('/');
                if (at >= 0 && slash > at) {
                    text = text.substring(0, at) + "@<v>" + text.substring(slash);
                }
                int paren = text.indexOf('(');
                return paren >= 0 ? text.substring(0, paren) : text;
            }
        }
        return "missing";
    }

    // The innermost file manager: never called.
    static JavaFileManager innermost() {
        InvocationHandler h = new InvocationHandler() {
            @Override
            public Object invoke(Object proxy, Method m, Object[] a) {
                return null;
            }
        };
        return (JavaFileManager) Proxy.newProxyInstance(
                L2W79StackTraceDefects.class.getClassLoader(), new Class<?>[] {JavaFileManager.class}, h);
    }

    // Reached from the JDK's own ForwardingJavaFileManager.getClassLoader
    // (java.compiler, the platform loader, an upgradeable module).
    static class Inner extends ForwardingJavaFileManager<JavaFileManager> {
        Inner(JavaFileManager m) {
            super(m);
        }

        @Override
        public ClassLoader getClassLoader(Location location) {
            Throwable t = new Throwable("compiler");
            thrownCompiler = find(t.getStackTrace(), FWD);
            threadCompiler = find(Thread.currentThread().getStackTrace(), FWD);
            walkedCompiler = StackWalker.getInstance().walk(
                    new Function<Stream<StackWalker.StackFrame>, StackTraceElement>() {
                        @Override
                        public StackTraceElement apply(Stream<StackWalker.StackFrame> s) {
                            for (Object o : s.toArray()) {
                                StackWalker.StackFrame f = (StackWalker.StackFrame) o;
                                if (f.getClassName().equals(FWD)) {
                                    return f.toStackTraceElement();
                                }
                            }
                            return null;
                        }
                    });
            printedCompiler = printedLine(t, FWD + ".getClassLoader");
            return null;
        }
    }

    static class Outer extends ForwardingJavaFileManager<JavaFileManager> {
        Outer(JavaFileManager m) {
            super(m);
        }
    }

    static Throwable baseThrown;

    static class Grab implements Consumer<Integer> {
        @Override
        public void accept(Integer i) {
            baseThrown = new Throwable("base");
        }
    }

    static void compilerRows() {
        JavaFileManager fm = new Outer(new Inner(innermost()));
        fm.getClassLoader(StandardLocation.CLASS_PATH);
        System.out.println("compiler-throwable " + shape(thrownCompiler));
        System.out.println("compiler-thread " + shape(threadCompiler));
        System.out.println("compiler-walker " + shape(walkedCompiler));
        System.out.println("compiler-print " + printedCompiler);
    }

    static void baseRows() {
        List<Integer> one = new ArrayList<>();
        one.add(1);
        one.forEach(new Grab());
        StackTraceElement[] st = baseThrown.getStackTrace();
        System.out.println("base-throwable " + shape(find(st, "java.util.ArrayList")));
        System.out.println("base-print " + printedLine(baseThrown, "java.util.ArrayList.forEach"));
        System.out.println("app-throwable " + shape(st[0]));
    }

    static List<StackWalker.StackFrame> walkFrames() {
        return StackWalker.getInstance().walk(
                new Function<Stream<StackWalker.StackFrame>, List<StackWalker.StackFrame>>() {
                    @Override
                    public List<StackWalker.StackFrame> apply(Stream<StackWalker.StackFrame> s) {
                        List<StackWalker.StackFrame> out = new ArrayList<>();
                        for (Object o : s.toArray()) {
                            out.add((StackWalker.StackFrame) o);
                        }
                        return out;
                    }
                });
    }

    static void raceRows() throws InterruptedException {
        final int threads = 8;
        boolean framesSeen = true;
        boolean identity = true;
        boolean mainIdentity = true;
        for (int round = 0; round < 40; round++) {
            final List<StackWalker.StackFrame> frames = walkFrames();
            framesSeen &= !frames.isEmpty();
            final StackTraceElement[][] got = new StackTraceElement[threads][frames.size()];
            final CountDownLatch start = new CountDownLatch(1);
            Thread[] ts = new Thread[threads];
            for (int t = 0; t < threads; t++) {
                final int me = t;
                ts[t] = new Thread(new Runnable() {
                    @Override
                    public void run() {
                        try {
                            start.await();
                        } catch (InterruptedException e) {
                            return;
                        }
                        for (int i = 0; i < frames.size(); i++) {
                            got[me][i] = frames.get(i).toStackTraceElement();
                        }
                    }
                });
                ts[t].start();
            }
            start.countDown();
            for (Thread t : ts) {
                t.join();
            }
            for (int i = 0; i < frames.size(); i++) {
                for (int t = 1; t < threads; t++) {
                    identity &= got[t][i] == got[0][i];
                }
                mainIdentity &= frames.get(i).toStackTraceElement() == got[0][i];
            }
        }
        System.out.println("race-frames " + framesSeen);
        System.out.println("race-identity " + identity);
        System.out.println("race-main-identity " + mainIdentity);
    }

    public static void main(String[] args) throws Exception {
        compilerRows();
        baseRows();
        raceRows();
    }
}
