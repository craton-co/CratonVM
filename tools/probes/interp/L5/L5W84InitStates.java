// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4, wave 84, lane L5 -- JVMS 5.5 class initialization as
// HotSpot 25 runs it: recursive initialization by the initializing thread,
// another thread waiting for an in-progress initialization, the erroneous
// state (`NoClassDefFoundError: Could not initialize class X` with the
// JDK-8048190 `ExceptionInInitializerError` cause naming the original
// exception and thread, an `Error` thrown unwrapped), the interfaces that a
// class's initialization initializes (only those declaring default methods,
// supers first, declaration order) and the uses that initialize nothing.
//
// Run:
//   $J/javac -d out L5W84InitStates.java
//   $J/java -cp out L5W84InitStates                    # HotSpot
//   $B --java-home $J/.. -cp out L5W84InitStates
//   $B --compatible --java-home $J/.. -cp out L5W84InitStates
//   (and each with --nojit)
// Expected block read from the JVMS 5.5 procedure and HotSpot's
// `InstanceKlass::initialize_impl` / `initialize_super_interfaces` /
// `java_lang_Throwable::create_initialization_error`; confirm on the host.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags, both CratonVM modes)
// recursive: RA.a=1 RB.seenA=0 RB.b=2
// waiter: 42 after started
// first-use: java.lang.ExceptionInInitializerError cause=java.lang.RuntimeException: boom
// second-use: java.lang.NoClassDefFoundError: Could not initialize class L5W84InitStates$Bad
// second-use-cause: java.lang.ExceptionInInitializerError: Exception java.lang.RuntimeException: boom [in thread "main"]
// error-first-use: L5W84InitStates$Oops: bad
// error-second-use: java.lang.NoClassDefFoundError: Could not initialize class L5W84InitStates$BadErr
// error-second-use-cause: java.lang.ExceptionInInitializerError: Exception L5W84InitStates$Oops: bad [in thread "main"]
// null-message-cause: java.lang.ExceptionInInitializerError: Exception java.lang.IllegalStateException [in thread "worker-7"]
// default-methods: [S, I0, I1, I2, K]
// interface-alone: [J1]
// no-init: [] after array, forName(false), class literal, constant
// inherited-static: [Sup] value=7
// HOTSPOT_EXPECTED_END

import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;

public class L5W84InitStates {
    static final List<String> LOG = new ArrayList<>();

    static String log(String s) {
        synchronized (LOG) {
            LOG.add(s);
        }
        return s;
    }

    // --- recursive initialization by the same thread ---
    static class RA {
        static int a;
        static {
            int unused = RB.b;
            a = 1;
        }
    }

    static class RB {
        static int seenA;
        static int b;
        static {
            seenA = RA.a; // RA is being initialized by this thread: default value
            b = 2;
        }
    }

    // --- a second thread waits for an in-progress initialization ---
    static final CountDownLatch STARTED = new CountDownLatch(1);

    static class Slow {
        static int v;
        static {
            STARTED.countDown();
            try {
                Thread.sleep(200);
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
            v = 42;
        }
    }

    // --- erroneous state ---
    static class Bad {
        static int v;
        static {
            if (true) {
                throw new RuntimeException("boom");
            }
        }
    }

    static class Oops extends Error {
        Oops(String m) {
            super(m);
        }
    }

    static class BadErr {
        static int v;
        static {
            if (true) {
                throw new Oops("bad");
            }
        }
    }

    static class BadNoMessage {
        static int v;
        static {
            if (true) {
                throw new IllegalStateException();
            }
        }
    }

    // --- interfaces initialized by a class's initialization ---
    interface I0 {
        String T = log("I0");
        default void d0() {}
    }

    interface I1 extends I0 {
        String T = log("I1");
        default void d1() {}
    }

    interface I2 {
        String T = log("I2");
        default void d2() {}
    }

    interface NoDefault {
        String T = log("NoDefault");
        void m();
    }

    static class S {
        static {
            log("S");
        }
    }

    static class K extends S implements I1, NoDefault, I2 {
        static {
            log("K");
        }

        public void m() {}
    }

    // An interface's own initialization initializes no superinterface.
    interface J0 {
        String T = log("J0");
        default void d() {}
    }

    interface J1 extends J0 {
        String T = log("J1");
    }

    // --- uses that initialize nothing ---
    static class Quiet {
        static final int CONST = 5;
        static {
            log("Quiet");
        }
    }

    // --- an inherited static field initializes only its declaring class ---
    static class Sup {
        static int value = 7;
        static {
            log("Sup");
        }
    }

    static class Sub extends Sup {
        static {
            log("Sub");
        }
    }

    static String drain() {
        synchronized (LOG) {
            String s = LOG.toString();
            LOG.clear();
            return s;
        }
    }

    public static void main(String[] args) throws Exception {
        System.out.println("recursive: RA.a=" + RA.a + " RB.seenA=" + RB.seenA + " RB.b=" + RB.b);

        Thread t = new Thread(() -> {
            int unused = Slow.v;
        });
        t.start();
        STARTED.await();
        System.out.println("waiter: " + Slow.v + " after started");
        t.join();

        try {
            int x = Bad.v;
        } catch (Throwable e) {
            System.out.println("first-use: " + e.getClass().getName() + " cause=" + e.getCause());
        }
        try {
            int x = Bad.v;
        } catch (Throwable e) {
            System.out.println("second-use: " + e);
            System.out.println("second-use-cause: " + e.getCause());
        }

        try {
            int x = BadErr.v;
        } catch (Throwable e) {
            System.out.println("error-first-use: " + e);
        }
        try {
            int x = BadErr.v;
        } catch (Throwable e) {
            System.out.println("error-second-use: " + e);
            System.out.println("error-second-use-cause: " + e.getCause());
        }

        Thread w = new Thread(() -> {
            try {
                int x = BadNoMessage.v;
            } catch (Throwable ignored) {
            }
        }, "worker-7");
        w.start();
        w.join();
        try {
            int x = BadNoMessage.v;
        } catch (Throwable e) {
            System.out.println("null-message-cause: " + e.getCause());
        }

        drain();
        new K();
        System.out.println("default-methods: " + drain());

        String j = J1.T;
        System.out.println("interface-alone: " + drain());

        Quiet[] arr = new Quiet[3];
        Class<?> c = Class.forName("L5W84InitStates$Quiet", false, L5W84InitStates.class.getClassLoader());
        Class<?> lit = Quiet.class;
        int k = Quiet.CONST;
        System.out.println("no-init: " + drain() + " after array, forName(false), class literal, constant");

        int v = Sub.value;
        System.out.println("inherited-static: " + drain() + " value=" + v);
    }
}
