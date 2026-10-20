// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 122, lane L4
// (`i114-L6-proposal-the-virtual-door-on-virtual-threads`, item 5;
// `CRATONVM_VTHREAD_SIG_POLY_SITES`, default off): signature-polymorphic sites
// (VarHandle access modes, `MethodHandle.invokeExact` / `invoke`) executed
// warm on JDK virtual threads, where the switch lets the site route cache
// serve them. Every row runs on a virtual thread, many times, so the cache fills
// and hits. Rows:
//
//   vh-field        VarHandle get/set/getAndAdd on an instance field
//   vh-cas          compareAndSet / compareAndExchange on int and long fields
//   mh-static       invokeExact of a static (int,int)int
//   mh-virtual      invokeExact of a virtual method over 2 receiver classes
//   mh-parks        invokeExact whose target sleeps (the thread unmounts or pins)
//   mh-throws       invokeExact whose target throws; the exception is caught
//   mcs-retarget    a MutableCallSite's dynamicInvoker, retargeted mid-loop
//   wrong-type      invokeExact with a mismatched type: WrongMethodTypeException
//   null-vh         VarHandle.get on a null receiver: NullPointerException
//
// Run: javac -d out L4W122VirtualThreadSigPolySites.java
//      cratonvm --java-home <jdk25> [--nojit] [--compatible] \
//          -Djdk.virtualThreadScheduler.parallelism=2 -cp out L4W122VirtualThreadSigPolySites
//      arms: (a) none; (b) CRATONVM_VTHREAD_SIG_POLY_SITES=1;
//            (c) (b) + CRATONVM_MH_INTERP_BASIC_FRAME_PUSH=1 (--nojit). All SAME.
//
// Expected HotSpot 25.0.3 output (default and -Xint):
//   vh-field: 500500 1000
//   vh-cas: 2000 2000 true
//   mh-static: 1001000
//   mh-virtual: 751000
//   mh-parks: 20
//   mh-throws: 1000 IllegalStateException
//   mcs-retarget: 1500
//   wrong-type: java.lang.invoke.WrongMethodTypeException
//   null-vh: java.lang.NullPointerException
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; added by the wave-123 L9 review, re-run 2026-10-19)
// vh-field: 500500 1000
// vh-cas: 2000 2000 true
// mh-static: 1001000
// mh-virtual: 751000
// mh-parks: 20
// mh-throws: 1000 IllegalStateException
// mcs-retarget: 1500
// wrong-type: java.lang.invoke.WrongMethodTypeException
// null-vh: java.lang.NullPointerException
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.MutableCallSite;
import java.lang.invoke.VarHandle;
import java.util.concurrent.atomic.AtomicReference;

public class L4W122VirtualThreadSigPolySites {
    int counter;
    int ci;
    long cl;

    static final VarHandle COUNTER;
    static final VarHandle CI;
    static final VarHandle CL;
    static final MethodHandle ADD;
    static final MethodHandle NAP;
    static final MethodHandle BOOM;
    static final MethodHandle WEIGHT;

    static class Base {
        int weight(int x) {
            return x;
        }
    }

    static class Heavy extends Base {
        @Override
        int weight(int x) {
            return 2 * x;
        }
    }

    static int add(int a, int b) {
        return a + b;
    }

    static int nap(int x) throws InterruptedException {
        Thread.sleep(1);
        return x;
    }

    static int boom(int x) {
        throw new IllegalStateException("boom " + x);
    }

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            COUNTER = l.findVarHandle(L4W122VirtualThreadSigPolySites.class, "counter", int.class);
            CI = l.findVarHandle(L4W122VirtualThreadSigPolySites.class, "ci", int.class);
            CL = l.findVarHandle(L4W122VirtualThreadSigPolySites.class, "cl", long.class);
            MethodType ii = MethodType.methodType(int.class, int.class);
            ADD = l.findStatic(L4W122VirtualThreadSigPolySites.class, "add",
                    MethodType.methodType(int.class, int.class, int.class));
            NAP = l.findStatic(L4W122VirtualThreadSigPolySites.class, "nap", ii);
            BOOM = l.findStatic(L4W122VirtualThreadSigPolySites.class, "boom", ii);
            WEIGHT = l.findVirtual(Base.class, "weight", ii);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    interface Body {
        String call() throws Throwable;
    }

    static String onVirtual(Body body) throws Exception {
        AtomicReference<String> out = new AtomicReference<>();
        Thread t = Thread.ofVirtual().start(() -> {
            try {
                out.set(body.call());
            } catch (Throwable e) {
                out.set("failed: " + e);
            }
        });
        t.join();
        return out.get();
    }

    public static void main(String[] args) throws Exception {
        System.out.println("vh-field: " + onVirtual(() -> {
            L4W122VirtualThreadSigPolySites o = new L4W122VirtualThreadSigPolySites();
            long sum = 0;
            for (int i = 1; i <= 1000; i++) {
                COUNTER.set(o, i);
                sum += (int) COUNTER.get(o);
            }
            int last = (int) COUNTER.getAndAdd(o, 0);
            return sum + " " + last;
        }));
        System.out.println("vh-cas: " + onVirtual(() -> {
            L4W122VirtualThreadSigPolySites o = new L4W122VirtualThreadSigPolySites();
            boolean all = true;
            for (int i = 0; i < 1000; i++) {
                all &= CI.compareAndSet(o, i, i + 1);
                int w = (int) CI.compareAndExchange(o, i + 1, i + 1);
                all &= w == i + 1;
                all &= CL.compareAndSet(o, (long) 2 * i, (long) 2 * i + 2);
            }
            return (o.ci * 2) + " " + o.cl + " " + all;
        }));
        System.out.println("mh-static: " + onVirtual(() -> {
            long sum = 0;
            for (int i = 0; i < 1000; i++) {
                sum += (int) ADD.invokeExact(i, 1001 - i);
            }
            for (int i = 0; i < 1000; i++) {
                sum += (int) ADD.invokeExact(0, 0);
            }
            return String.valueOf(sum);
        }));
        System.out.println("mh-virtual: " + onVirtual(() -> {
            Base light = new Base();
            Base heavy = new Heavy();
            long sum = 0;
            for (int i = 1; i <= 1000; i++) {
                Base receiver = (i & 1) == 0 ? heavy : light;
                sum += (int) WEIGHT.invokeExact(receiver, i);
            }
            return String.valueOf(sum);
        }));
        System.out.println("mh-parks: " + onVirtual(() -> {
            int sum = 0;
            for (int i = 0; i < 20; i++) {
                sum += (int) NAP.invokeExact(1);
            }
            return String.valueOf(sum);
        }));
        System.out.println("mh-throws: " + onVirtual(() -> {
            int caught = 0;
            String name = "";
            for (int i = 0; i < 1000; i++) {
                try {
                    int unused = (int) BOOM.invokeExact(i);
                } catch (IllegalStateException e) {
                    caught++;
                    name = e.getClass().getSimpleName();
                }
            }
            return caught + " " + name;
        }));
        System.out.println("mcs-retarget: " + onVirtual(() -> {
            MutableCallSite site = new MutableCallSite(MethodHandles.constant(int.class, 1));
            MethodHandle inv = site.dynamicInvoker();
            int sum = 0;
            for (int i = 0; i < 1000; i++) {
                if (i == 500) {
                    site.setTarget(MethodHandles.constant(int.class, 2));
                }
                sum += (int) inv.invokeExact();
            }
            return String.valueOf(sum);
        }));
        System.out.println("wrong-type: " + onVirtual(() -> {
            String seen = "none";
            for (int i = 0; i < 100; i++) {
                try {
                    long unused = (long) ADD.invokeExact(1, 2);
                } catch (Throwable e) {
                    seen = e.getClass().getName();
                }
            }
            return seen;
        }));
        System.out.println("null-vh: " + onVirtual(() -> {
            L4W122VirtualThreadSigPolySites o = new L4W122VirtualThreadSigPolySites();
            String seen = "none";
            for (int i = 0; i < 100; i++) {
                try {
                    int unused = (int) COUNTER.get(i == 99 ? null : o);
                } catch (NullPointerException e) {
                    seen = e.getClass().getName();
                }
            }
            return seen;
        }));
    }
}
