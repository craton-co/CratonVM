// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 8, lane reflect: finalizer registration and Reference
// discovery of an instance built by each allocation route (page
// docs/internal/fixed-bugs/r21-reflective-new-skips-finalizer-and-reference-registration-FIXED-20261019.md).
//
// Routes: bytecode `new` (the control), Constructor.newInstance (JDK 25:
// DirectConstructorHandleAccessor -> constructor MethodHandle ->
// Unsafe.allocateInstance + linkToSpecial <init>), MethodHandles findConstructor
// invokeExact, a constructor that throws after super() (HotSpot registers: its
// Object.<init> returned), and sun.misc.Unsafe.allocateInstance with no
// constructor (HotSpot does NOT register: no Object.<init> runs). The Reference
// rows build a WeakReference subclass registered with a queue by new / reflection
// / handle; HotSpot clears and enqueues every one.
//
// Run:
//   javac -d out R21ReflectCtorRegistration.java
//   java -Xmx256m -cp out R21ReflectCtorRegistration                       # HotSpot 25
//   cratonvm -Xmx256m -cp out R21ReflectCtorRegistration                   # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21ReflectCtorRegistration
//   cratonvm --nojit -Xmx256m -cp out R21ReflectCtorRegistration
//   (also -XX:+UseG1GC and -XX:+UseZGC)
// Kill-switch controls (each expected to differ from HotSpot on its rows only):
//   CRATONVM_MH_CTOR_REGISTERS_FINALIZABLE=0  -> fin ctor / mh / throwing-ctor false 0
//   CRATONVM_NATIVE_ALLOC_NOTES_REFERENCES=0  -> wref ctor / mh enqueued 0 live <n>
// HotSpot expectation (every arm, both modes):
//   fin new true 20000
//   fin ctor true 20000
//   fin mh true 20000
//   fin throwing-ctor true 20000
//   fin unsafe-no-ctor 0
//   wref new enqueued 20000 live 0
//   wref ctor enqueued 20000 live 0
//   wref mh enqueued 20000 live 0

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.WeakReference;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.InvocationTargetException;
import java.util.concurrent.atomic.AtomicInteger;

public class R21ReflectCtorRegistration {
    static final int N = 20_000;
    static final AtomicInteger FIN = new AtomicInteger();
    static final ReferenceQueue<Object> Q = new ReferenceQueue<>();

    static class Fin {
        final int v;

        Fin(int v) {
            this.v = v;
        }

        @Override
        @SuppressWarnings("removal")
        protected void finalize() {
            FIN.incrementAndGet();
        }
    }

    static class Thrower extends Fin {
        Thrower(int v) {
            super(v);
            throw new IllegalStateException("after super " + v);
        }
    }

    public static class WRef extends WeakReference<Object> {
        public WRef(Object r) {
            super(r, Q);
        }
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

    static String finRow(String route, Runnable make) {
        FIN.set(0);
        make.run();
        for (int i = 0; i < 400 && FIN.get() < N; i++) {
            collect();
        }
        return "fin " + route + " " + (FIN.get() == N) + " " + FIN.get();
    }

    interface Maker {
        Object make(int r) throws Throwable;
    }

    static String wrefRow(String route, Maker m) throws Throwable {
        while (Q.poll() != null) {
            // drain earlier rows
        }
        Object[] keep = new Object[N];
        for (int r = 0; r < N; r++) {
            keep[r] = m.make(r);
        }
        int enq = 0;
        for (int i = 0; i < 400 && enq < N; i++) {
            System.gc();
            while (Q.poll() != null) {
                enq++;
            }
        }
        int live = 0;
        for (Object o : keep) {
            live += ((WRef) o).get() == null ? 0 : 1;
        }
        return "wref " + route + " enqueued " + enq + " live " + live;
    }

    public static void main(String[] args) throws Throwable {
        Constructor<Fin> fin = Fin.class.getDeclaredConstructor(int.class);
        Constructor<Thrower> thrower = Thrower.class.getDeclaredConstructor(int.class);
        Constructor<WRef> wref = WRef.class.getConstructor(Object.class);
        MethodHandle finMh = MethodHandles.lookup()
                .findConstructor(Fin.class, MethodType.methodType(void.class, int.class));
        MethodHandle wrefMh = MethodHandles.lookup()
                .findConstructor(WRef.class, MethodType.methodType(void.class, Object.class));

        System.out.println(finRow("new", () -> {
            for (int r = 0; r < N; r++) {
                new Fin(r);
            }
        }));
        System.out.println(finRow("ctor", () -> {
            try {
                for (int r = 0; r < N; r++) {
                    fin.newInstance(r);
                }
            } catch (ReflectiveOperationException e) {
                throw new RuntimeException(e);
            }
        }));
        System.out.println(finRow("mh", () -> {
            try {
                for (int r = 0; r < N; r++) {
                    Fin f = (Fin) finMh.invokeExact(r);
                }
            } catch (Throwable t) {
                throw new RuntimeException(t);
            }
        }));
        System.out.println(finRow("throwing-ctor", () -> {
            int caught = 0;
            for (int r = 0; r < N; r++) {
                try {
                    thrower.newInstance(r);
                } catch (InvocationTargetException e) {
                    caught++;
                } catch (ReflectiveOperationException e) {
                    throw new RuntimeException(e);
                }
            }
            if (caught != N) {
                throw new AssertionError("caught " + caught);
            }
        }));
        Field f = sun.misc.Unsafe.class.getDeclaredField("theUnsafe");
        f.setAccessible(true);
        sun.misc.Unsafe u = (sun.misc.Unsafe) f.get(null);
        FIN.set(0);
        for (int r = 0; r < N; r++) {
            u.allocateInstance(Fin.class);
        }
        for (int i = 0; i < 40; i++) {
            collect();
        }
        System.out.println("fin unsafe-no-ctor " + FIN.get());

        System.out.println(wrefRow("new", r -> new WRef(new int[] {r})));
        System.out.println(wrefRow("ctor", r -> wref.newInstance(new int[] {r})));
        System.out.println(wrefRow("mh", r -> (WRef) wrefMh.invokeExact((Object) new int[] {r})));
    }
}
