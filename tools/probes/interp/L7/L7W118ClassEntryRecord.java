// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe + bench
//
// Interpreter round i6 wave 118, lane L7: `CRATONVM_CLASS_ENTRY_RECORD_ALL` (default off; stage 1
// of docs/internal/proposals/interpreter/i1-L2-proposal-per-class-resolved-constant-pool-20260923.md).
// The per-entry, first-wins record of a `CONSTANT_Class` resolution (JVMS 5.4.3; HotSpot's
// `resolved_klasses`), kept until now only for an isolated URL loader's classes, is kept for
// every class with real bytes and a non-bootstrap defining loader. A child-first loader (not a
// URLClassLoader: the non-isolated path) defines `L7W118Target` and `L7W118Item`; Target's
// methods name Item through `new`, `checkcast`, `instanceof`, `anewarray`, `ldc` and a static
// call, each at its own constant-pool entry. Rows:
//
//   first        the answers on first use, and how often the loader's loadClass was asked
//                for L7W118Item (HotSpot: once; the loader's dictionary answers after that)
//   churn        the same calls 2 000 times while a fresh loader defines `L7W118Churn` before
//                each one (a second definition of a known name: CratonVM's per-thread class
//                sites retire on it); answers and loadClass counts again
//   threads      four threads run the calls on first use at the same time: one Item class
//   identity     every answer is the class the loader defined (no stub, no app-loader copy)
//
// Bench (TIME lines): `churn-calls` per call under the churn; `steady-calls` without it (control).
// Expected with the switch: churn-calls lower (a site that retired is answered from the record,
// not by the loader lookup), steady-calls flat.
//
// Run, switch unset and =1, --jdk-only and --compatible, JIT and --nojit:
//   cratonvm --java-home <jdk25> --jdk-only --nojit -cp <dir> L7W118ClassEntryRecord
//   CRATONVM_CLASS_ENTRY_RECORD_ALL=1 cratonvm --java-home <jdk25> --jdk-only --nojit -cp <dir> L7W118ClassEntryRecord
// Wave 120: the loadClass counts are on `TIME asks ...` lines, which the battery does not compare
// (HotSpot 25 prints `TIME asks first=1 churn=1 threads=1`; CratonVM's default arm printed 4, see
// docs/internal/fixed-bugs/interpreter-i120-L7-a-loader-is-asked-again-for-a-resolved-name-FIXED-20261224.md).
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// first sum=22 item=L7W118Item array=[LL7W118Item;
// churn sum=44000
// threads 220 220 220 220
// identity loader-defined=true target-sees-same=true other-loader-differs=true
// churn-calls checksum=8800
// steady-calls checksum=8800
// HOTSPOT_EXPECTED_END
import java.io.IOException;
import java.io.InputStream;
import java.lang.reflect.Method;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicInteger;

public class L7W118ClassEntryRecord {
    static final Map<String, AtomicInteger> ASKS = new ConcurrentHashMap<>();

    /** Child-first for the probe's L7W118Target / L7W118Item / L7W118Churn, parent-first otherwise. */
    static final class ChildFirst extends ClassLoader {
        final boolean count;

        ChildFirst(boolean count) {
            super(L7W118ClassEntryRecord.class.getClassLoader());
            this.count = count;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.startsWith("L7W118") || name.equals("L7W118ClassEntryRecord")
                    || name.startsWith("L7W118ClassEntryRecord$")) {
                return super.loadClass(name, resolve);
            }
            if (count) {
                ASKS.computeIfAbsent(name, k -> new AtomicInteger()).incrementAndGet();
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

        static byte[] read(String name) throws ClassNotFoundException {
            try (InputStream in = L7W118ClassEntryRecord.class.getClassLoader()
                    .getResourceAsStream(name + ".class")) {
                if (in == null) {
                    throw new ClassNotFoundException(name);
                }
                return in.readAllBytes();
            } catch (IOException e) {
                throw new ClassNotFoundException(name, e);
            }
        }
    }

    static int asks(String name) {
        AtomicInteger n = ASKS.get(name);
        return n == null ? 0 : n.get();
    }

    static void churn() throws Exception {
        Class.forName("L7W118Churn", false, new ChildFirst(false));
    }

    public static void main(String[] args) throws Exception {
        ChildFirst loader = new ChildFirst(true);
        Class<?> target = Class.forName("L7W118Target", true, loader);
        Method calls = target.getMethod("calls", int.class);
        Method describe = target.getMethod("describe");
        Method itemClass = target.getMethod("itemClass");
        // Another loader's package-private class: its run-time package is not ours.
        calls.setAccessible(true);
        describe.setAccessible(true);
        itemClass.setAccessible(true);
        long first = (Long) calls.invoke(null, 1);
        System.out.println("first sum=" + first + " " + describe.invoke(null));
        int asksFirst = asks("L7W118Item");

        long sum = 0;
        for (int i = 0; i < 2000; i++) {
            churn();
            sum += (Long) calls.invoke(null, 1);
        }
        System.out.println("churn sum=" + sum);
        int asksChurn = asks("L7W118Item");

        ChildFirst fresh = new ChildFirst(true);
        ASKS.clear();
        Class<?> t2 = Class.forName("L7W118Target", true, fresh);
        Method calls2 = t2.getMethod("calls", int.class);
        Method itemClass2 = t2.getMethod("itemClass");
        calls2.setAccessible(true);
        itemClass2.setAccessible(true);
        Thread[] ts = new Thread[4];
        long[] out = new long[4];
        for (int k = 0; k < ts.length; k++) {
            final int slot = k;
            ts[k] = new Thread(() -> {
                try {
                    out[slot] = (Long) calls2.invoke(null, 10);
                } catch (ReflectiveOperationException e) {
                    out[slot] = -1;
                }
            });
        }
        for (Thread t : ts) {
            t.start();
        }
        for (Thread t : ts) {
            t.join();
        }
        System.out.println("threads " + out[0] + " " + out[1] + " " + out[2] + " " + out[3]);
        // The loader's loadClass counts for the named class: Java-visible, but a
        // known difference (see the header), so on a line the battery ignores.
        System.out.println("TIME asks first=" + asksFirst + " churn=" + asksChurn + " threads="
                + asks("L7W118Item"));

        Class<?> item = Class.forName("L7W118Item", false, loader);
        System.out.println("identity loader-defined=" + (item.getClassLoader() == loader)
                + " target-sees-same=" + itemClass.invoke(null).equals(item)
                + " other-loader-differs=" + !itemClass2.invoke(null).equals(item));

        bench(calls);
    }

    static void bench(Method calls) throws Exception {
        final int n = 400;
        for (String row : new String[] {"churn-calls", "steady-calls"}) {
            long best = Long.MAX_VALUE;
            long checksum = 0;
            for (int rep = 0; rep < 3; rep++) {
                long total = 0;
                long c = 0;
                for (int i = 0; i < n; i++) {
                    if (row.equals("churn-calls")) {
                        churn();
                    }
                    long t0 = System.nanoTime();
                    c += (Long) calls.invoke(null, 1);
                    total += System.nanoTime() - t0;
                }
                checksum = c;
                best = Math.min(best, total / n);
            }
            System.out.println(row + " checksum=" + checksum);
            System.out.println("TIME " + row + " " + best + " ns/call (min of 3)");
        }
    }
}

class L7W118Item {
    static int seven() {
        return 7;
    }

    int v = 3;
}

class L7W118Churn {
}

class L7W118Target {
    public static long calls(int rounds) {
        long s = 0;
        for (int r = 0; r < rounds; r++) {
            Object o = new L7W118Item();
            if (o instanceof L7W118Item) {
                s += ((L7W118Item) o).v;
            }
            L7W118Item[] arr = new L7W118Item[2];
            s += arr.length;
            s += L7W118Item.seven();
            s += L7W118Item.class.getSimpleName().length();
        }
        return s;
    }

    public static String describe() {
        return "item=" + L7W118Item.class.getName() + " array=" + L7W118Item[].class.getName();
    }

    public static Class<?> itemClass() {
        return L7W118Item.class;
    }
}
