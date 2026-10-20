// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 51, lane L5 -- the remaining item of
// `docs/known-issues/interpreter/i27-L5-a-non-parallel-capable-loader-gets-a-per-name-lock-20260928.md`:
// the JDK's `ClassLoader.loadClass` runs under
// `synchronized (getClassLoadingLock(name))`, which for a parallel-capable
// loader is a per-NAME lock object. Two threads loading ONE name through such
// a loader therefore run its `findClass` one after the other, and the second
// finds the first's class through `findLoadedClass`. CratonVM's base
// `loadClass` native took no per-name lock, so both threads ran `findClass`
// and the second `defineClass` failed with a duplicate-definition
// `LinkageError`.
//
// `$Loader` registers as parallel-capable, has a null parent, and its
// `findClass` (for `$Target` only) sleeps 300 ms inside, counting how many
// threads are in it at once, before defining `$Target` from the application
// loader's bytes. Two threads start together on a latch; the round runs twice
// on fresh loaders (the first on a cold VM). A third row checks the lock is
// per name: two DIFFERENT names enter `findClass` together.
//
// Run (no setup):
//   javac -d out L5W51ParallelLoaderPerNameLock.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W51ParallelLoaderPerNameLock
//
// Positive control: `CRATONVM_LOADER_PER_NAME_LOCK=0` prints
// `same-name-N calls=2 max=2 same=false errors=1` (timing-dependent: both
// threads are inside `findClass` during the sleep).
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   registered=true
//   same-name-1 calls=1 max=1 same=true errors=0
//   same-name-2 calls=1 max=1 same=true errors=0
//   two-names max=2 errors=0

import java.io.IOException;
import java.io.InputStream;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicInteger;

public class L5W51ParallelLoaderPerNameLock {
    static final String P = "L5W51ParallelLoaderPerNameLock$";

    public static class Target {
    }

    public static class Target2 {
    }

    static byte[] appBytes(String name) throws ClassNotFoundException {
        try (InputStream in =
                ClassLoader.getSystemResourceAsStream(name.replace('.', '/') + ".class")) {
            return in.readAllBytes();
        } catch (IOException e) {
            throw new ClassNotFoundException(name, e);
        }
    }

    static final class Loader extends ClassLoader {
        static {
            registerAsParallelCapable();
        }

        final AtomicInteger calls = new AtomicInteger();
        final AtomicInteger inside = new AtomicInteger();
        final AtomicInteger max = new AtomicInteger();

        Loader() {
            super(null);
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (!name.equals(P + "Target") && !name.equals(P + "Target2")) {
                throw new ClassNotFoundException(name);
            }
            calls.incrementAndGet();
            int now = inside.incrementAndGet();
            max.accumulateAndGet(now, Math::max);
            try {
                Thread.sleep(300);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            } finally {
                inside.decrementAndGet();
            }
            byte[] b = appBytes(name);
            return defineClass(name, b, 0, b.length);
        }
    }

    static List<Object> race(Loader loader, String first, String second) throws Exception {
        CountDownLatch start = new CountDownLatch(1);
        List<Object> results = new ArrayList<>();
        Thread[] threads = new Thread[2];
        String[] names = {first, second};
        Object[] out = new Object[2];
        for (int i = 0; i < 2; i++) {
            int k = i;
            threads[i] = new Thread(() -> {
                try {
                    start.await();
                    out[k] = loader.loadClass(names[k]);
                } catch (Throwable t) {
                    out[k] = t;
                }
            });
            threads[i].start();
        }
        start.countDown();
        for (Thread t : threads) {
            t.join();
        }
        results.add(out[0]);
        results.add(out[1]);
        return results;
    }

    static int errors(List<Object> results) {
        int n = 0;
        for (Object o : results) {
            if (o instanceof Throwable) {
                n++;
            }
        }
        return n;
    }

    public static void main(String[] args) throws Exception {
        System.out.println("registered=" + new Loader().isRegisteredAsParallelCapable());
        for (int round = 1; round <= 2; round++) {
            Loader loader = new Loader();
            List<Object> r = race(loader, P + "Target", P + "Target");
            System.out.println("same-name-" + round + " calls=" + loader.calls.get()
                    + " max=" + loader.max.get() + " same=" + (r.get(0) == r.get(1))
                    + " errors=" + errors(r));
        }
        Loader loader = new Loader();
        List<Object> r = race(loader, P + "Target", P + "Target2");
        System.out.println("two-names max=" + loader.max.get() + " errors=" + errors(r));
    }
}
