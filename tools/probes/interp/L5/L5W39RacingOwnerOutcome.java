// Interpreter round i1, wave 39, lane L5 -- the MEMBER-OWNER twin of
// `L5W37RacingEntryOutcome` (JVMS §5.4.3: one constant-pool entry has one
// outcome for every thread): `$Ref.make()` calls `invokestatic
// $Target.value()`, so the racing resolution is the `Methodref`'s owner.
//
// A parallel-capable loader scripts `loadClass("$Target")`: in `fail-first`
// call 1 (T1) blocks until call 2 (T2) has failed the entry with
// ClassNotFoundException, then defines `$Target`; in `succeed-first` call 1
// blocks until call 2 has defined it, then throws.
//
// Fix for `docs/internal/fixed-bugs/interpreter-L5-a-racing-resolution-of-one-class-entry-can-leave-threads-disagreeing-FIXED-20261005.md`
// (the member-owner remainder): `constants.rs`
// `recorded_member_owner_failure_after_success`, called after the
// `invokestatic` owner's loader-driven success (`dispatch_static.rs`) and the
// two receiver-call owner resolutions (`invoke.rs`), `--jdk-only`. Before
// wave 39 `fail-first T1 first=ok` (T1's success was not re-checked against
// the failure T2 recorded meanwhile); the rest as HotSpot.
//
// Positive control: `CRATONVM_DBG=access` prints one
// `[ACCESS-DBG] RESOLUTION-RACE #n success meets a failure recorded meanwhile: cp#<k> in L5W39RacingOwnerOutcome$Ref`
// line for `fail-first` and one `... refusal adopts the class published
// meanwhile ...` line for `succeed-first`. `--compatible`: every row `ok` by
// design (a failed `loadClass` falls back to the global store).
//
// Run (no setup):
//   javac -d out L5W39RacingOwnerOutcome.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W39RacingOwnerOutcome
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

public class L5W39RacingOwnerOutcome {
    static final String P = "L5W39RacingOwnerOutcome$";

    static final class Scripted extends ClassLoader {
        static {
            registerAsParallelCapable();
        }

        final boolean failFirst;
        final AtomicInteger calls = new AtomicInteger();
        final CountDownLatch firstInside = new CountDownLatch(1);
        final CountDownLatch secondDone = new CountDownLatch(1);

        Scripted(boolean failFirst) {
            super(L5W39RacingOwnerOutcome.class.getClassLoader());
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
        public static Object value() {
            return "ok";
        }
    }

    public static class Ref {
        public static Object make() {
            return Target.value();
        }
    }
}
