package org.springframework.core.test.tools;

import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicInteger;

public class ForkProbe3 {
    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 50;
        int threads = args.length > 1 ? Integer.parseInt(args[1]) : 8;
        ClassLoader appLoader = ForkProbe3.class.getClassLoader();
        String resPath = "org/springframework/test/context/bean/override/mockito/SpringMockResolver.class";
        String resPathDot = "org.springframework.test.context.bean.override.mockito.SpringMockResolver";

        AtomicInteger fails = new AtomicInteger(0);
        AtomicInteger ok = new AtomicInteger(0);

        for (int round = 0; round < rounds; round++) {
            // A FRESH forked loader each round, like the real test does per-method.
            CompileWithForkedClassLoaderClassLoader forked =
                    new CompileWithForkedClassLoaderClassLoader(appLoader);
            CountDownLatch latch = new CountDownLatch(threads);
            Thread[] ts = new Thread[threads];
            for (int t = 0; t < threads; t++) {
                ts[t] = new Thread(() -> {
                    try {
                        Class<?> c = forked.loadClass(resPathDot);
                        ok.incrementAndGet();
                    } catch (Throwable e) {
                        fails.incrementAndGet();
                        System.out.println("FAIL: " + e);
                    } finally {
                        latch.countDown();
                    }
                });
            }
            for (Thread th : ts) th.start();
            latch.await();
        }
        System.out.println("rounds=" + rounds + " threads=" + threads
                + " ok=" + ok.get() + " fails=" + fails.get());
        System.out.println(fails.get() > 0 ? "REPRO: BUG REPRODUCED" : "REPRO: no bug observed");
    }
}
