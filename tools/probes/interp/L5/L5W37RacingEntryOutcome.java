// Interpreter round i1, wave 37, lane L5 -- the deterministic two-thread test
// of `docs/internal/fixed-bugs/interpreter-L5-a-racing-resolution-of-one-class-entry-can-leave-threads-disagreeing-FIXED-20261005.md`
// (JVMS §5.4.3: one constant-pool entry has one outcome for every thread).
//
// A PARALLEL-CAPABLE loader (so neither VM locks it around the VM-initiated
// `loadClass`) defines `$Ref`, whose `make()` does `new $Target()`. The
// loader's `loadClass("$Target")` is scripted per call:
//
//  fail-first   call 1 (thread T1) blocks inside `loadClass` until call 2
//               (thread T2) has FAILED the same entry (ClassNotFoundException),
//               then defines `$Target`. T2 resolved first and failed; T1's
//               resolution finishes after the failure was recorded.
//  succeed-first
//               call 1 (T1) blocks until call 2 (T2) has SUCCEEDED (defines
//               `$Target`), then throws ClassNotFoundException. T2's success
//               is published first; T1's failure comes after it.
//
// Each case then calls `make()` again on T1, on T2 and on main, and prints one
// row per call: the class of the object `make()` returned (`ok`) or the
// throwable's class. Every row of one case must agree after the race.
//
// Run (no setup):
//   javac -d out L5W37RacingEntryOutcome.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W37RacingEntryOutcome
//
// Wave 38 (lane L5), `--jdk-only`: the loader's ClassNotFoundException now
// fails T2's resolution (no global fallback), T1's later success meets T2's
// recorded failure and throws it (`recorded_resolution_failure_after_success`
// at every class-entry success site), and in `succeed-first` T1's failure
// adopts the class T2 published (`drive_defining_loader_load_checked`):
// HotSpot's rows expected. `CRATONVM_DBG=access` prints two
// `[ACCESS-DBG] RESOLUTION-RACE` lines (one per case). `--compatible`: every
// row `ok` by design (the fallback answers).
//
// CratonVM before wave 38 (from the code, not run; both modes): `fail-first T1 first=ok` and
// `fail-first T1 again=ok` (T1's success is not re-checked against the
// failure T2 recorded meanwhile, and T1's own site cache keeps it), while T2
// and main rethrow T2's recorded `NoClassDefFoundError` -- the disagreement
// the page describes. `succeed-first` is expected to match (T1's failed
// `loadClass` falls back to the global route, which finds the class T2's call
// defined). What would fix `fail-first` is on the page ("Progress (wave 37)").
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   fail-first T2 first=java.lang.NoClassDefFoundError
//   fail-first T1 first=java.lang.NoClassDefFoundError
//   fail-first T1 again=java.lang.NoClassDefFoundError
//   fail-first T2 again=java.lang.NoClassDefFoundError
//   fail-first main=java.lang.NoClassDefFoundError
//   fail-first loadClass calls=2
//   succeed-first T2 first=ok
//   succeed-first T1 first=ok
//   succeed-first T1 again=ok
//   succeed-first T2 again=ok
//   succeed-first main=ok
//   succeed-first loadClass calls=2

import java.io.IOException;
import java.io.InputStream;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicInteger;

public class L5W37RacingEntryOutcome {
    static final String P = "L5W37RacingEntryOutcome$";

    static final class Scripted extends ClassLoader {
        static {
            registerAsParallelCapable();
        }

        final boolean failFirst;
        final AtomicInteger calls = new AtomicInteger();
        final CountDownLatch firstInside = new CountDownLatch(1);
        final CountDownLatch secondDone = new CountDownLatch(1);

        Scripted(boolean failFirst) {
            super(L5W37RacingEntryOutcome.class.getClassLoader());
            this.failFirst = failFirst;
        }

        byte[] bytes(String name) throws ClassNotFoundException {
            try (InputStream in =
                    ClassLoader.getSystemResourceAsStream(name.replace('.', '/') + ".class")) {
                return in.readAllBytes();
            } catch (IOException e) {
                throw new ClassNotFoundException(name, e);
            }
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.equals(P + "Ref")) {
                synchronized (getClassLoadingLock(name)) {
                    Class<?> c = findLoadedClass(name);
                    if (c != null) {
                        return c;
                    }
                    byte[] b = bytes(name);
                    return defineClass(name, b, 0, b.length);
                }
            }
            if (!name.equals(P + "Target")) {
                return super.loadClass(name, resolve);
            }
            int call = calls.incrementAndGet();
            if (call == 1) {
                firstInside.countDown();
                try {
                    secondDone.await();
                } catch (InterruptedException e) {
                    throw new ClassNotFoundException(name, e);
                }
                if (!failFirst) {
                    throw new ClassNotFoundException(name + " (first call, after the second)");
                }
            } else if (call == 2) {
                if (failFirst) {
                    throw new ClassNotFoundException(name + " (second call)");
                }
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                byte[] b = bytes(name);
                return defineClass(name, b, 0, b.length);
            }
        }
    }

    static String call(Class<?> ref) {
        try {
            Object o = ref.getMethod("make").invoke(null);
            return "ok";
        } catch (java.lang.reflect.InvocationTargetException e) {
            return e.getCause().getClass().getName();
        } catch (Throwable t) {
            return "probe " + t;
        }
    }

    static void run(String label, boolean failFirst) throws Exception {
        Scripted loader = new Scripted(failFirst);
        Class<?> ref = loader.loadClass(P + "Ref");
        String[] t1 = new String[2];
        String[] t2 = new String[2];
        CountDownLatch t1Again = new CountDownLatch(1);
        CountDownLatch t2Again = new CountDownLatch(1);
        CountDownLatch t1Finished = new CountDownLatch(1);
        Thread a = new Thread(() -> {
            t1[0] = call(ref);
            t1Finished.countDown();
            try {
                t1Again.await();
            } catch (InterruptedException e) {
                return;
            }
            t1[1] = call(ref);
        });
        a.start();
        loader.firstInside.await();
        Thread b = new Thread(() -> {
            t2[0] = call(ref);
            loader.secondDone.countDown();
            try {
                t2Again.await();
            } catch (InterruptedException e) {
                return;
            }
            t2[1] = call(ref);
        });
        b.start();
        t1Finished.await();
        t1Again.countDown();
        t2Again.countDown();
        a.join();
        b.join();
        System.out.println(label + " T2 first=" + t2[0]);
        System.out.println(label + " T1 first=" + t1[0]);
        System.out.println(label + " T1 again=" + t1[1]);
        System.out.println(label + " T2 again=" + t2[1]);
        System.out.println(label + " main=" + call(ref));
        System.out.println(label + " loadClass calls=" + loader.calls.get());
    }

    public static void main(String[] args) throws Exception {
        run("fail-first", true);
        run("succeed-first", false);
    }

    public static class Target {
    }

    public static class Ref {
        public static Object make() {
            return new Target();
        }
    }
}
