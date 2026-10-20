// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.StackWalker.Option;
import java.lang.StackWalker.StackFrame;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;
import java.util.stream.Collectors;

/**
 * SBR round 3 wave 2, lane {@code refmisc}: GC stress of the walker's frame carriers after
 * {@code populate_sfi} began filling the {@code StackTraceElement} origin (module, version, loader
 * name: it allocates) and minting a {@code ResolvedMethodName} per frame. Walkers on several
 * threads retain frames and elements across collections; every retained frame is re-read after the
 * storm and must still answer its class, method, declaring class, module and loader.
 *
 * <pre>
 *   cratonvm --jdk-only -Xmx64m [-XX:+UseG1GC | -XX:+UseZGC] -cp . Sbr3RefmiscWalkGc [seconds]
 * </pre>
 */
public final class Sbr3RefmiscWalkGc {
    static volatile Object sink;

    static int depth(int n, List<StackFrame> keep, List<StackTraceElement> keepSte) {
        if (n > 0) {
            return depth(n - 1, keep, keepSte);
        }
        StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE).forEach(f -> {
            keep.add(f);
            keepSte.add(f.toStackTraceElement());
        });
        return keep.size();
    }

    public static void main(String[] args) throws Exception {
        int seconds = args.length > 0 ? Integer.parseInt(args[0]) : 10;
        AtomicBoolean stop = new AtomicBoolean();
        AtomicLong bad = new AtomicLong();
        AtomicLong walks = new AtomicLong();
        Thread[] ts = new Thread[4];
        for (int t = 0; t < ts.length; t++) {
            ts[t] = new Thread(() -> {
                List<Object> junk = new ArrayList<>();
                while (!stop.get()) {
                    List<StackFrame> keep = new ArrayList<>();
                    List<StackTraceElement> keepSte = new ArrayList<>();
                    depth(3 + (int) (walks.get() % 9), keep, keepSte);
                    for (int i = 0; i < 40; i++) {
                        junk.add(new byte[2048]);
                    }
                    if (junk.size() > 3000) {
                        junk.clear();
                    }
                    int jdkFrames = 0;
                    for (int i = 0; i < keep.size(); i++) {
                        StackFrame f = keep.get(i);
                        StackTraceElement e = keepSte.get(i);
                        Class<?> c = f.getDeclaringClass();
                        boolean ok = c.getName().equals(f.getClassName()) && f.getMethodName().equals(e.getMethodName())
                                && e.getClassName().equals(f.getClassName());
                        if (c == Sbr3RefmiscWalkGc.class) {
                            ok &= "app".equals(e.getClassLoaderName()) && e.getModuleName() == null
                                    && e.toString().startsWith("Sbr3RefmiscWalkGc.");
                        } else if (c.getModule().isNamed() && c.getModule().getName().equals("java.base")) {
                            ok &= "java.base".equals(e.getModuleName()) && e.toString().startsWith("java.base/");
                            jdkFrames++;
                        }
                        if (!ok) {
                            bad.incrementAndGet();
                        }
                    }
                    walks.incrementAndGet();
                    sink = keepSte.stream().map(StackTraceElement::toString).collect(Collectors.toList());
                }
            });
            ts[t].start();
        }
        Thread.sleep(seconds * 1000L);
        stop.set(true);
        for (Thread t : ts) {
            t.join();
        }
        System.out.println("walks>0=" + (walks.get() > 0) + " bad=" + bad.get());
        System.out.println(bad.get() == 0 ? "WALK-GC-OK" : "WALK-GC-FAIL");
    }
}
