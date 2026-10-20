// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 45, lane L2 (review of compiled dispatch): a call
// site compiled while its receiver's declared class had ONE implementation of
// the method (class-hierarchy analysis may bind it directly, or inline it)
// must dispatch to an override that a class loaded LATER declares, in both a
// method-entry body and a loop already running compiled (OSR) in another
// thread. HotSpot deoptimizes the dependent code when the subclass loads.
//
// Rows:
//   entry    -- `callF(A)` compiled hot on A receivers, then called with a
//               freshly loaded B (B.f overrides A.f): B's answer.
//   osr      -- a thread spinning in a compiled loop over a volatile A field,
//               summing `a.f()`; main loads C (C.f overrides A.f) and stores a
//               C: the loop must see C's answer (`saw-override=true`).
//   iface    -- `callG(I)` compiled hot on one implementation of interface
//               I's default method, then called with a later-loaded class
//               overriding the default: its answer.
//   abstract -- `callH(Base)` hot on the one concrete subclass of an abstract
//               class, then a second concrete subclass loaded later: its
//               answer.
//
// HotSpot 25 prints (the same with -Xint):
//     entry a=1 b=2
//     osr saw-override=true
//     iface d=10 e=20
//     abstract p=100 q=200
// No setup; `--compatible` prints the same.
public class L2W45ChaTargetGainsAnOverride {
    public static class A {
        public int f() {
            return 1;
        }
    }

    public static class B extends A {
        @Override
        public int f() {
            return 2;
        }
    }

    public static class C extends A {
        @Override
        public int f() {
            return 3;
        }
    }

    public interface I {
        default int g() {
            return 10;
        }
    }

    public static class D implements I {
    }

    public static class E implements I {
        @Override
        public int g() {
            return 20;
        }
    }

    public abstract static class Base {
        public abstract int h();
    }

    public static class P extends Base {
        @Override
        public int h() {
            return 100;
        }
    }

    public static class Q extends Base {
        @Override
        public int h() {
            return 200;
        }
    }

    static int callF(A a) {
        return a.f();
    }

    static int callG(I i) {
        return i.g();
    }

    static int callH(Base b) {
        return b.h();
    }

    static volatile A shared = new A();
    static volatile boolean stop;
    static volatile boolean sawOverride;

    static void spin() {
        long sum = 0;
        while (!stop) {
            int v = shared.f();
            if (v == 3) {
                sawOverride = true;
            }
            sum += v;
        }
        if (sum == 42) {
            System.out.println("unlikely");
        }
    }

    public static void main(String[] args) throws Exception {
        A a = new A();
        long sink = 0;
        for (int n = 0; n < 200_000; n++) {
            sink += callF(a);
        }
        A b = (A) Class.forName("L2W45ChaTargetGainsAnOverride$B").getDeclaredConstructor().newInstance();
        System.out.println("entry a=" + callF(a) + " b=" + callF(b));

        Thread t = new Thread(L2W45ChaTargetGainsAnOverride::spin, "spinner");
        t.setDaemon(true);
        t.start();
        Thread.sleep(300);
        A c = (A) Class.forName("L2W45ChaTargetGainsAnOverride$C").getDeclaredConstructor().newInstance();
        shared = c;
        long deadline = System.nanoTime() + 30_000_000_000L;
        while (!sawOverride && System.nanoTime() < deadline) {
            Thread.sleep(1);
        }
        stop = true;
        t.join(30_000);
        System.out.println("osr saw-override=" + sawOverride);

        I d = new D();
        for (int n = 0; n < 200_000; n++) {
            sink += callG(d);
        }
        I e = (I) Class.forName("L2W45ChaTargetGainsAnOverride$E").getDeclaredConstructor().newInstance();
        System.out.println("iface d=" + callG(d) + " e=" + callG(e));

        Base p = new P();
        for (int n = 0; n < 200_000; n++) {
            sink += callH(p);
        }
        Base q = (Base) Class.forName("L2W45ChaTargetGainsAnOverride$Q").getDeclaredConstructor().newInstance();
        System.out.println("abstract p=" + callH(p) + " q=" + callH(q));
        if (sink == 7) {
            System.out.println("unlikely");
        }
    }
}
