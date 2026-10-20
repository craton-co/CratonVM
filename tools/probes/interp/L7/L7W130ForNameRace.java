// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe
//
// Interpreter round i6 wave 130, lane L7: `CRATONVM_LOADER_DRIVE_UNDER_LOADER_LOCK` (default ON,
// `=0` off) on the natives' VM-side door (`classloader_real::invoke_load_class_as_the_vm_publishing`),
// page docs/internal/fixed-bugs/interpreter-i120-L7-a-loader-is-asked-again-for-a-resolved-name-FIXED-20261224.md.
// HotSpot's `Class.forName(name, init, loader)` goes through `SystemDictionary::resolve_or_fail`:
// for a loader that is NOT parallel-capable it takes the loader's monitor, checks the loader's
// dictionary again under it, and only then calls `loadClass`, recording an answer of another
// loader before it releases the monitor. So four threads racing `forName` of one name through one
// such loader call its `loadClass` once. Rows (each: four threads released together by a latch;
// the loader's `loadClass` sleeps 50 ms after counting, so the others arrive while it runs):
//
//   defined    a name the child-first loader defines itself
//   delegated  a name the loader hands to its parent (the app loader's class: an initiating record)
//   array      `forName("[L...;")`: the element is asked through the loader
//   parallel   control: a parallel-capable child-first loader (no VM monitor; HotSpot asks it
//              once per racing thread, 4, timing-dependent)
//
// The compared lines say which class each thread got; the loadClass counts are on the `TIME asks`
// line (not compared). HotSpot 25 (default flags and -Xint):
//   TIME asks defined=1 delegated=1 array=1 parallel=4
// CratonVM before wave 130: defined=4 delegated=4 array=4 (the door took the monitor but did not
// re-check under it).
//
//   javac -d <dir> tools/probes/interp/L7/L7W130ForNameRace.java
//   java -cp <dir> L7W130ForNameRace
//   cratonvm --java-home <jdk25> --jdk-only [--nojit] -cp <dir> L7W130ForNameRace
//   CRATONVM_LOADER_DRIVE_UNDER_LOADER_LOCK=0 cratonvm ... L7W130ForNameRace   (control: 4 4 4)
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// defined same=true loader-defined=true name=L7W130Def
// delegated same=true app-defined=true name=L7W130Del
// array same=true element-loader-defined=true name=[LL7W130Arr;
// parallel same=true loader-defined=true name=L7W130Par
// HOTSPOT_EXPECTED_END
import java.io.IOException;
import java.io.InputStream;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicInteger;

public class L7W130ForNameRace {
    static final AtomicInteger ASKS = new AtomicInteger();

    static byte[] read(String name) throws ClassNotFoundException {
        try (InputStream in = L7W130ForNameRace.class.getClassLoader()
                .getResourceAsStream(name + ".class")) {
            if (in == null) {
                throw new ClassNotFoundException(name);
            }
            return in.readAllBytes();
        } catch (IOException e) {
            throw new ClassNotFoundException(name, e);
        }
    }

    /** Child-first for L7W130Def / L7W130Arr / L7W130Par, delegates L7W130Del. */
    abstract static class Base extends ClassLoader {
        static {
            // Registered so that a subclass may register too; registration is per class.
            registerAsParallelCapable();
        }

        Base() {
            super(L7W130ForNameRace.class.getClassLoader());
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.startsWith("L7W130") || name.startsWith("L7W130ForNameRace")) {
                return super.loadClass(name, resolve);
            }
            ASKS.incrementAndGet();
            try {
                Thread.sleep(50);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
            if (name.equals("L7W130Del")) {
                return super.loadClass(name, resolve);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    byte[] bytes = read(name);
                    c = defineClass(name, bytes, 0, bytes.length);
                }
                return c;
            }
        }
    }

    /** Not parallel-capable (does not register). */
    static final class ChildFirst extends Base {
    }

    /** The control: the same loader, parallel-capable. */
    static final class ParallelChildFirst extends Base {
        static {
            registerAsParallelCapable();
        }
    }

    static Class<?>[] race(String name, ClassLoader loader) throws Exception {
        ASKS.set(0);
        Class<?>[] out = new Class<?>[4];
        Thread[] ts = new Thread[4];
        CountDownLatch go = new CountDownLatch(1);
        for (int k = 0; k < ts.length; k++) {
            final int slot = k;
            ts[k] = new Thread(() -> {
                try {
                    go.await();
                    out[slot] = Class.forName(name, false, loader);
                } catch (Throwable t) {
                    System.out.println("thread " + slot + " threw " + t);
                }
            });
            ts[k].start();
        }
        go.countDown();
        for (Thread t : ts) {
            t.join();
        }
        return out;
    }

    static boolean same(Class<?>[] cs) {
        for (Class<?> c : cs) {
            if (c == null || c != cs[0]) {
                return false;
            }
        }
        return true;
    }

    public static void main(String[] args) throws Exception {
        ChildFirst loader = new ChildFirst();
        ClassLoader app = L7W130ForNameRace.class.getClassLoader();

        Class<?>[] def = race("L7W130Def", loader);
        int asksDef = ASKS.get();
        System.out.println("defined same=" + same(def) + " loader-defined="
                + (def[0] != null && def[0].getClassLoader() == loader)
                + " name=" + (def[0] == null ? "null" : def[0].getName()));

        Class<?>[] del = race("L7W130Del", loader);
        int asksDel = ASKS.get();
        System.out.println("delegated same=" + same(del) + " app-defined="
                + (del[0] != null && del[0].getClassLoader() == app)
                + " name=" + (del[0] == null ? "null" : del[0].getName()));

        Class<?>[] arr = race("[LL7W130Arr;", loader);
        int asksArr = ASKS.get();
        System.out.println("array same=" + same(arr) + " element-loader-defined="
                + (arr[0] != null && arr[0].getComponentType().getClassLoader() == loader)
                + " name=" + (arr[0] == null ? "null" : arr[0].getName()));

        ParallelChildFirst parallel = new ParallelChildFirst();
        Class<?>[] par = race("L7W130Par", parallel);
        int asksPar = ASKS.get();
        System.out.println("parallel same=" + same(par) + " loader-defined="
                + (par[0] != null && par[0].getClassLoader() == parallel)
                + " name=" + (par[0] == null ? "null" : par[0].getName()));

        System.out.println("TIME asks defined=" + asksDef + " delegated=" + asksDel + " array="
                + asksArr + " parallel=" + asksPar);
    }
}

class L7W130Def {
}

class L7W130Del {
}

class L7W130Arr {
}

class L7W130Par {
}
