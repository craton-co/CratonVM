// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 13, lane fuzz3: finalizer registration for a FINALIZABLE
// HIDDEN class whose constructors delegate with this(..) and throw. Wave 11
// (page docs/known-issues/jit/r21-review-w10-findings-20261019.md item 1)
// fixed `ctor_target_may_initialize`: a hidden class is stored as `P/0x..`
// while its constant pool still names `P`, so the plain-name compare took
// `this()` for a third-class call and withdrew a registration HotSpot keeps.
// The page notes that no probe arm covered it ("needs class bytes built at run
// time"); this probe reads its own payload's class bytes and defines them
// hidden, like R21HiddenLookupIn.
//
// Arms: `loader` (the same bytes defined by a child-first class loader: an
// ordinary class, the control), `hidden` (defineHiddenClass), `hiddenNest`
// (defineHiddenClass NESTMATE). Routes: `mh` (constructor handles), `ctor`
// (Constructor.newInstance), `self` (the payload's own static make(): the
// creator frame IS the hidden class, `new` resolving this_class), `loop` (the
// payload's own hot loop, OSR inside the hidden class), each segment settled.
// Shapes per segment of n constructions (all throw):
//   P(int)    { this(); k = 1; throw }             -> finalized (bucket k1)
//   P(int,int){ this(check(v)); }                  -> never (would land in k0)
//   P(long)   { super(); k = 2; throw }            -> finalized (bucket k2)
//   P(String) { this(Integer.parseInt(s)); } "r"   -> finalized via P(int) (k1)
//   P(String) "x"+r: NumberFormatException first   -> never (k0)
// So every segment should read k0=0 k1=2n k2=n. The throw texts use string
// concatenation inside the hidden class (indy StringConcatFactory, the wave 8
// hidden runtime-package fix). Access facts (Constructor.newInstance without
// setAccessible from the outer class) are printed with hidden names sanitized.
//
// Run:
//   javac -d out R21Fz3FinHidden1.java
//   java -Xmx256m -cp out R21Fz3FinHidden1                     # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz3FinHidden1                 # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz3FinHidden1
//   cratonvm --nojit -Xmx256m -cp out R21Fz3FinHidden1
//   cratonvm -XX:+UseG1GC ... / -XX:+UseZGC ...
// HotSpot expectation: every line identical on every arm.

import java.io.InputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodHandles.Lookup;
import java.lang.invoke.MethodType;
import java.lang.reflect.Constructor;
import java.lang.reflect.InvocationTargetException;
import java.util.concurrent.atomic.AtomicInteger;

public class R21Fz3FinHidden1 {
    static final String PAYLOAD = "R21Fz3FinHidden1Payload";
    static final int N = 3000;
    static final int HOT = 15_000;
    public static final AtomicInteger[] FIN = {new AtomicInteger(), new AtomicInteger(), new AtomicInteger()};
    public static final AtomicInteger TOTAL = new AtomicInteger();

    public static void fin(int k) {
        FIN[k < 0 || k > 2 ? 0 : k].incrementAndGet();
        TOTAL.incrementAndGet();
    }

    public static int check(int v) {
        if (v >= 0) {
            throw new IllegalArgumentException("check " + v);
        }
        return v;
    }

    static String clean(String s) {
        if (s == null) {
            return "null";
        }
        return s.replaceAll("/0x[0-9a-fA-F]+", "/0x#").replaceAll("@[0-9a-fA-F]+", "@#");
    }

    static void collect() {
        System.gc();
        System.runFinalization();
        try {
            Thread.sleep(2);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }

    static void settle() {
        int last = -1;
        int still = 0;
        for (int i = 0; i < 400; i++) {
            collect();
            int now = TOTAL.get();
            if (now == last) {
                still++;
            } else {
                still = 0;
                last = now;
            }
            if (i >= 20 && still >= 10) {
                return;
            }
        }
    }

    /** Child-first for the payload only: an ordinary (not hidden) copy of the same bytes. */
    static final class OneClassLoader extends ClassLoader {
        final byte[] bytes;

        OneClassLoader(byte[] bytes) {
            super(R21Fz3FinHidden1.class.getClassLoader());
            this.bytes = bytes;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.equals(PAYLOAD)) {
                synchronized (getClassLoadingLock(name)) {
                    Class<?> c = findLoadedClass(name);
                    if (c == null) {
                        c = defineClass(name, bytes, 0, bytes.length);
                    }
                    return c;
                }
            }
            return super.loadClass(name, resolve);
        }
    }

    interface Route {
        int run(int n) throws Throwable;
    }

    static int thrownOf(Throwable t) {
        Throwable c = t instanceof InvocationTargetException ? t.getCause() : t;
        if (c instanceof IllegalArgumentException || c instanceof IllegalStateException) {
            return 1;
        }
        throw new AssertionError("unexpected " + clean(String.valueOf(c)));
    }

    static void segment(String arm, String route, int n, Route r) throws Throwable {
        settle();
        for (AtomicInteger c : FIN) {
            c.set(0);
        }
        int thrown = r.run(n);
        settle();
        System.out.println("seg " + arm + " " + route + " thrown=" + thrown + " k0=" + FIN[0].get() + " k1="
                + FIN[1].get() + " k2=" + FIN[2].get());
    }

    static void arm(String arm, Class<?> p, Lookup full) throws Throwable {
        System.out.println("arm " + arm + " hidden=" + p.isHidden() + " name=" + clean(p.getName()) + " pkg='"
                + p.getPackageName() + "'");
        // Access facts from this class, without setAccessible.
        for (Class<?>[] sig : new Class<?>[][] {{int.class}, {long.class}, {String.class}}) {
            Constructor<?> c = p.getDeclaredConstructor(sig);
            String fact;
            try {
                Object arg = sig[0] == int.class ? (Object) (-1) : sig[0] == long.class ? (Object) (-1L) : "-1";
                Object o = c.newInstance(arg);
                fact = "ok " + clean(o.getClass().getName());
            } catch (IllegalAccessException e) {
                fact = "IAE " + clean(e.getMessage());
            } catch (InvocationTargetException e) {
                fact = "ITE " + e.getCause().getClass().getName() + " " + clean(e.getCause().getMessage());
            }
            System.out.println("access " + arm + " " + sig[0].getSimpleName() + " " + fact);
        }

        MethodType toObj1 = MethodType.methodType(Object.class, int.class);
        MethodHandle mInt = full.findConstructor(p, MethodType.methodType(void.class, int.class)).asType(toObj1);
        MethodHandle mInt2 = full.findConstructor(p, MethodType.methodType(void.class, int.class, int.class))
                .asType(MethodType.methodType(Object.class, int.class, int.class));
        MethodHandle mLong = full.findConstructor(p, MethodType.methodType(void.class, long.class))
                .asType(MethodType.methodType(Object.class, long.class));
        MethodHandle mStr = full.findConstructor(p, MethodType.methodType(void.class, String.class))
                .asType(MethodType.methodType(Object.class, String.class));
        segment(arm, "mh", N, n -> {
            int t = 0;
            for (int r = 0; r < n; r++) {
                try {
                    Object o = (Object) mInt.invokeExact(r);
                } catch (Throwable e) {
                    t += thrownOf(e);
                }
                try {
                    Object o = (Object) mInt2.invokeExact(r, r);
                } catch (Throwable e) {
                    t += thrownOf(e);
                }
                try {
                    Object o = (Object) mLong.invokeExact((long) r);
                } catch (Throwable e) {
                    t += thrownOf(e);
                }
                try {
                    Object o = (Object) mStr.invokeExact("" + r);
                } catch (Throwable e) {
                    t += thrownOf(e);
                }
                try {
                    Object o = (Object) mStr.invokeExact("x" + r);
                } catch (Throwable e) {
                    t += thrownOf(e);
                }
            }
            return t;
        });

        Constructor<?> cInt = p.getDeclaredConstructor(int.class);
        Constructor<?> cInt2 = p.getDeclaredConstructor(int.class, int.class);
        Constructor<?> cLong = p.getDeclaredConstructor(long.class);
        Constructor<?> cStr = p.getDeclaredConstructor(String.class);
        for (Constructor<?> c : new Constructor<?>[] {cInt, cInt2, cLong, cStr}) {
            c.setAccessible(true);
        }
        segment(arm, "ctor", N, n -> {
            int t = 0;
            for (int r = 0; r < n; r++) {
                Object[][] argss = {{r}, {r, r}, {(long) r}, {"" + r}, {"x" + r}};
                Constructor<?>[] cs = {cInt, cInt2, cLong, cStr, cStr};
                for (int s = 0; s < 5; s++) {
                    try {
                        cs[s].newInstance(argss[s]);
                    } catch (Throwable e) {
                        t += thrownOf(e);
                    }
                }
            }
            return t;
        });

        MethodHandle self = full.findStatic(p, "make", MethodType.methodType(int.class, int.class, int.class));
        segment(arm, "self", N, n -> {
            int t = 0;
            for (int r = 0; r < n; r++) {
                for (int s = 0; s < 5; s++) {
                    t += (int) self.invokeExact(s, r);
                }
            }
            return t;
        });

        MethodHandle loop = full.findStatic(p, "loop", MethodType.methodType(int.class, int.class));
        segment(arm, "loop", HOT, n -> (int) loop.invokeExact(n));
        segment(arm, "loop-again", HOT, n -> (int) loop.invokeExact(n));
    }

    public static void main(String[] args) throws Throwable {
        byte[] bytes;
        try (InputStream in = R21Fz3FinHidden1.class.getResourceAsStream(PAYLOAD + ".class")) {
            bytes = in.readAllBytes();
        }
        Lookup l = MethodHandles.lookup();

        Class<?> lp = Class.forName(PAYLOAD, true, new OneClassLoader(bytes));
        arm("loader", lp, MethodHandles.privateLookupIn(lp, l));

        Lookup hl = l.defineHiddenClass(bytes, true);
        arm("hidden", hl.lookupClass(), hl);

        Lookup hnl = l.defineHiddenClass(bytes, true, Lookup.ClassOption.NESTMATE);
        arm("hiddenNest", hnl.lookupClass(), hnl);
        System.out.println("done");
    }
}

/** The payload: never loaded by name from the probe class; defined from its bytes. */
class R21Fz3FinHidden1Payload {
    int k;

    R21Fz3FinHidden1Payload() {
    }

    R21Fz3FinHidden1Payload(int v) {
        this();
        k = 1;
        if (v >= 0) {
            throw new IllegalStateException("this-then " + v + " in " + k);
        }
    }

    R21Fz3FinHidden1Payload(int v, int w) {
        this(R21Fz3FinHidden1.check(v));
    }

    R21Fz3FinHidden1Payload(long v) {
        super();
        k = 2;
        if (v >= 0) {
            throw new IllegalStateException("after " + v + ":" + k);
        }
    }

    R21Fz3FinHidden1Payload(String s) {
        this(Integer.parseInt(s));
    }

    @Override
    @SuppressWarnings("removal")
    protected void finalize() {
        R21Fz3FinHidden1.fin(k);
    }

    /** The creator is this class: `new` names this_class. Answers 1 when the shape threw. */
    static int make(int shape, int r) {
        try {
            switch (shape) {
                case 0: new R21Fz3FinHidden1Payload(r); break;
                case 1: new R21Fz3FinHidden1Payload(r, r); break;
                case 2: new R21Fz3FinHidden1Payload((long) r); break;
                case 3: new R21Fz3FinHidden1Payload("" + r); break;
                default: new R21Fz3FinHidden1Payload("x" + r); break;
            }
            return 0;
        } catch (IllegalArgumentException | IllegalStateException e) {
            return 1;
        }
    }

    static int loop(int n) {
        int t = 0;
        for (int r = 0; r < n; r++) {
            for (int s = 0; s < 5; s++) {
                t += make(s, r);
            }
        }
        return t;
    }
}
