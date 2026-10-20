import java.lang.ref.WeakReference;
import java.util.*;
import java.util.concurrent.*;

/**
 * SBR round 2, wave 8, lane TLOCAL: ThreadLocal / InheritableThreadLocal
 * semantics over the real Thread.threadLocals table. Every line is
 * deterministic; diff the output against HotSpot in --jdk-only and
 * --compatible.
 *
 *   java Sbr2W8TlocalProbe
 */
public class Sbr2W8TlocalProbe {
    static void p(String k, Object v) {
        System.out.println(k + " = " + v);
    }

    static class Sub<T> extends ThreadLocal<T> {
        int inits;
        @Override
        protected T initialValue() {
            inits++;
            return null;
        }
    }

    static class SubItl extends InheritableThreadLocal<String> {
        @Override
        protected String childValue(String parent) {
            return "child(" + parent + ")";
        }
    }

    static class OverGet extends ThreadLocal<String> {
        @Override
        public String get() {
            return "over:" + super.get();
        }
    }

    static String nullReceiver() {
        ThreadLocal<String> nul = null;
        String last = "none";
        for (int i = 0; i < 30000; i++) {
            try {
                if (i < 29990) {
                    nul = new ThreadLocal<>();
                } else {
                    nul = null;
                }
                last = String.valueOf(nul.get());
            } catch (NullPointerException e) {
                last = "NPE:" + e.getMessage();
            }
        }
        return last;
    }

    static <T> T inThread(Callable<T> c) throws Exception {
        Object[] box = new Object[2];
        Thread t = new Thread(() -> {
            try {
                box[0] = c.call();
            } catch (Throwable e) {
                box[1] = e;
            }
        });
        t.start();
        t.join();
        if (box[1] != null) {
            return (T) ("EXC " + box[1]);
        }
        return (T) box[0];
    }

    public static void main(String[] args) throws Exception {
        // --- basics
        ThreadLocal<String> a = new ThreadLocal<>();
        p("basic.initial", a.get());
        a.set("x");
        p("basic.set", a.get());
        a.set("y");
        p("basic.overwrite", a.get());
        a.set(null);
        p("basic.setNull", a.get());
        a.remove();
        p("basic.removed", a.get());
        a.set("z");
        p("basic.afterRemove", a.get());

        // --- initialValue / withInitial
        Sub<String> sub = new Sub<>();
        p("sub.get1", sub.get());
        p("sub.get2", sub.get());
        p("sub.inits", sub.inits);
        sub.remove();
        sub.get();
        p("sub.inits.afterRemove", sub.inits);
        ThreadLocal<Integer> wi = ThreadLocal.withInitial(() -> 41);
        p("withInitial.get", wi.get());
        wi.set(7);
        p("withInitial.set", wi.get());
        wi.remove();
        p("withInitial.removed", wi.get());
        p("withInitial.otherThread", inThread(() -> wi.get()));
        // initialValue that sets the local itself: the set is overwritten
        ThreadLocal<String> self = new ThreadLocal<>() {
            @Override
            protected String initialValue() {
                this.set("inner");
                return "outer";
            }
        };
        p("selfSet", self.get());
        p("selfSet.again", self.get());

        // --- overridden get() is the subclass's own body
        OverGet og = new OverGet();
        og.set("v");
        p("overGet", og.get());

        // --- many locals: collisions, probe wrap, resize, rehash
        List<ThreadLocal<Integer>> many = new ArrayList<>();
        for (int i = 0; i < 300; i++) {
            ThreadLocal<Integer> t = new ThreadLocal<>();
            t.set(i);
            many.add(t);
        }
        long sum = 0;
        for (ThreadLocal<Integer> t : many) {
            sum += t.get();
        }
        p("many.sum", sum);
        for (int i = 0; i < 300; i += 2) {
            many.get(i).remove();
        }
        int nulls = 0;
        long sum2 = 0;
        for (ThreadLocal<Integer> t : many) {
            Integer v = t.get();
            if (v == null) {
                nulls++;
            } else {
                sum2 += v;
            }
        }
        p("many.afterRemove.nulls", nulls);
        p("many.afterRemove.sum", sum2);
        for (int i = 0; i < 300; i += 2) {
            many.get(i).set(1000 + i);
        }
        long sum3 = 0;
        for (ThreadLocal<Integer> t : many) {
            sum3 += t.get();
        }
        p("many.reset.sum", sum3);

        // --- GC-cleared keys (stale entries) must not break lookups
        for (int round = 0; round < 3; round++) {
            for (int i = 0; i < 200; i++) {
                ThreadLocal<byte[]> dead = new ThreadLocal<>();
                dead.set(new byte[8]);
            }
            System.gc();
            Thread.sleep(20);
        }
        long sum4 = 0;
        for (ThreadLocal<Integer> t : many) {
            sum4 += t.get();
        }
        p("stale.sum", sum4);
        ThreadLocal<String> late = new ThreadLocal<>();
        late.set("late");
        p("stale.newLocal", late.get());

        // --- inheritance
        InheritableThreadLocal<String> itl = new InheritableThreadLocal<>();
        itl.set("parent");
        p("itl.child", inThread(() -> itl.get()));
        p("itl.childSetDoesNotLeak", inThread(() -> {
            itl.set("changed");
            return itl.get();
        }) + "/" + itl.get());
        SubItl sitl = new SubItl();
        sitl.set("p");
        p("itl.childValue", inThread(() -> sitl.get()));
        ThreadLocal<String> plain = new ThreadLocal<>();
        plain.set("plain");
        p("plain.notInherited", inThread(() -> plain.get()));
        itl.remove();
        p("itl.removedParent", inThread(() -> itl.get()));
        itl.set("again");
        ExecutorService ex = Executors.newFixedThreadPool(1);
        p("itl.executor", ex.submit(() -> itl.get()).get());
        ex.shutdown();
        Thread nonInherit = new Thread(null, () -> {
            System.out.println("itl.noInheritCtor = " + itl.get());
        }, "n", 0, false);
        nonInherit.start();
        nonInherit.join();
        // ITL and plain in one thread do not cross
        InheritableThreadLocal<String> itl2 = new InheritableThreadLocal<>();
        ThreadLocal<String> tl2 = new ThreadLocal<>();
        itl2.set("I");
        tl2.set("T");
        p("itl.vs.tl", itl2.get() + tl2.get());
        p("itl.vs.tl.child", inThread(() -> itl2.get() + tl2.get()));

        // --- virtual threads
        ThreadLocal<String> vtl = new ThreadLocal<>();
        vtl.set("main");
        Thread vt = Thread.ofVirtual().unstarted(() -> {
            System.out.println("vt.initial = " + vtl.get());
            vtl.set("virt");
            System.out.println("vt.set = " + vtl.get());
            vtl.remove();
            System.out.println("vt.removed = " + vtl.get());
        });
        vt.start();
        vt.join();
        p("vt.mainIntact", vtl.get());
        Thread vt2 = Thread.ofVirtual().unstarted(() -> {
            System.out.println("vt.itl.inherited = " + itl.get());
        });
        vt2.start();
        vt2.join();
        Thread vt3 = Thread.ofVirtual().inheritInheritableThreadLocals(false).unstarted(() -> {
            System.out.println("vt.itl.notInherited = " + itl.get());
        });
        vt3.start();
        vt3.join();

        // --- several threads, own values
        ThreadLocal<Integer> counter = ThreadLocal.withInitial(() -> 0);
        int n = 8;
        Thread[] ts = new Thread[n];
        int[] res = new int[n];
        for (int i = 0; i < n; i++) {
            final int k = i;
            ts[i] = new Thread(() -> {
                for (int j = 0; j < 1000; j++) {
                    counter.set(counter.get() + 1);
                }
                res[k] = counter.get();
            });
            ts[i].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        p("threads.counts", Arrays.toString(res));
        p("threads.mainCounter", counter.get());

        // --- value identity and null handling
        Object o = new Object();
        ThreadLocal<Object> ol = new ThreadLocal<>();
        ol.set(o);
        p("identity", ol.get() == o);
        ThreadLocal<String> nl = ThreadLocal.withInitial(() -> null);
        p("withInitial.null", nl.get());
        nl.set("s");
        p("withInitial.null.set", nl.get());

        // --- hot loops: the JIT's ThreadLocal.get thin helper must answer exactly
        // what the bytecode answers, for every receiver class, through a site whose
        // static type is ThreadLocal
        {
            ThreadLocal<String> staticTl = new ThreadLocal<>();
            staticTl.set("plain");
            ThreadLocal<String> viaOverGet = new OverGet();
            viaOverGet.set("v2");
            ThreadLocal<String> viaSub = new Sub<String>();
            viaSub.set("sub");
            ThreadLocal<String> viaItl = new InheritableThreadLocal<>();
            viaItl.set("itl");
            ThreadLocal<String> viaSupplied = ThreadLocal.withInitial(() -> "supplied");
            ThreadLocal<String> viaSitl = new SubItl();
            viaSitl.set("s");
            ThreadLocal<String> unset = new ThreadLocal<>();
            long h = 0;
            for (int i = 0; i < 60000; i++) {
                String x0 = staticTl.get();
                h += x0 == null ? 7 : x0.length();
                h += viaOverGet.get().length();
                h += viaSub.get().length();
                h += viaItl.get().length();
                h += viaSupplied.get().length();
                h += viaSitl.get().length();
                if (unset.get() == null) {
                    h += 1;
                }
                // found-key overwrites through every receiver class, null values
                // included (the helper's SATB pre-barrier and card-marking store)
                staticTl.set(((i & 3) == 0) ? null : "plain" + (i & 7));
                viaSub.set("sub" + (i & 15));
                viaItl.set("itl" + (i & 1));
                viaSitl.set("s" + (i & 7));
                viaOverGet.set("v" + (i & 3));
                if ((i & 1023) == 0) {
                    unset.set("x" + i);
                    h += unset.get().length();
                    unset.remove();
                }
            }
            p("hot.sum", h);
            p("hot.child", inThread(() -> {
                long c = 0;
                for (int i = 0; i < 20000; i++) {
                    c += staticTl.get() == null ? 1 : 100;
                    c += viaItl.get() == null ? 1 : 100;
                    c += viaSitl.get() == null ? 1 : 100;
                }
                return c;
            }));
            p("hot.nullReceiver", nullReceiver());
        }

        // --- Thread.exit clears (a finished thread's locals are released)
        WeakReference<Object>[] ref = new WeakReference[1];
        ThreadLocal<Object> holder = new ThreadLocal<>();
        Thread th = new Thread(() -> {
            Object v = new Object();
            ref[0] = new WeakReference<>(v);
            holder.set(v);
        });
        th.start();
        th.join();
        for (int i = 0; i < 5 && ref[0].get() != null; i++) {
            System.gc();
            Thread.sleep(20);
        }
        p("exit.released", ref[0].get() == null);
        p("done", true);
    }
}
