// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: TYPECHECK21-1 part A from the GC side.
// A header-only `instanceof` (`CRATONVM_JIT_IR_INSTANCEOF_HEADER_READ`, and
// `_OFF_CHAIN`) is no longer a loop writer (`loop_writes_memory`), so the
// restricted read hoist and the array-load hoist now lift loop-invariant
// loads OUT of a loop whose body type-tests -- but the test's slow path
// (`jit_instanceof`) is still a GC point (`effect_of_node` keeps
// `safepoint: true`).
//
// Risk targeted: a reference loaded before the loop (hoisted `h.ref`,
// `h.ref.arr`) lives across every slow-path call; a moving collection at that
// call must find it in the map and update it. If the hoisted value is in a
// register or slot the call's map omits, the loop reads a stale address
// after a relocation (garbage sums, a crash). The loop body allocates
// nothing (so it really is hoist-eligible), tests against eight interfaces
// over receivers of eleven classes (interface tests miss the inline arm and
// call the helper), while a second thread allocates and calls System.gc()
// continuously. The sums are exact. Then a deopt trigger (a class first seen
// now) and the loop again. Interface tests are also asked against a lambda
// and a Proxy (the helper's name-based answers).
//
// Run:
//   javac -d out R21Fz2InstMem2.java
//   java -Xmx256m -cp out R21Fz2InstMem2                                 # HotSpot 25
//   CRATONVM_JIT_IR_INSTANCEOF_HEADER_READ=1 cratonvm -Xmx256m -cp out R21Fz2InstMem2
//   CRATONVM_JIT_IR_INSTANCEOF_HEADER_READ=1 CRATONVM_JIT_IR_INSTANCEOF_OFF_CHAIN=1 cratonvm -Xmx256m -cp out R21Fz2InstMem2
//   the same two with -XX:+UseG1GC and with -XX:+UseZGC; then -Xmx64m (more collections)
//   cratonvm -Xmx256m -cp out R21Fz2InstMem2, cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.lang.reflect.Proxy;
import java.util.ArrayList;

public class R21Fz2InstMem2 {
    interface I0 {
    }

    interface I1 {
    }

    interface I2 extends I0 {
    }

    interface I3 {
    }

    interface I4 extends I1, I3 {
    }

    interface I5 {
    }

    interface I6 extends I5 {
    }

    interface I7 {
    }

    static class K0 implements I0 {
    }

    static class K1 implements I1 {
    }

    static class K2 implements I2 {
    }

    static class K3 implements I3, I7 {
    }

    static class K4 implements I4 {
    }

    static class K5 extends K4 implements I5 {
    }

    static class K6 implements I6 {
    }

    static class K7 extends K2 implements I7 {
    }

    static class K8 {
    }

    static class K9 extends K8 implements I1, I5 {
    }

    static final class Payload {
        final long val;
        final int[] arr;

        Payload(long val) {
            this.val = val;
            this.arr = new int[8];
            for (int i = 0; i < 8; i++) {
                arr[i] = (int) (val * (i + 1));
            }
        }
    }

    static final class Holder {
        Payload ref;
    }

    static volatile boolean stop;

    static long loop(Holder h, Object[] os, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            Object o = os[i % os.length];
            int bits = (o instanceof I0 ? 1 : 0) | (o instanceof I1 ? 2 : 0) | (o instanceof I2 ? 4 : 0)
                    | (o instanceof I3 ? 8 : 0) | (o instanceof I4 ? 16 : 0) | (o instanceof I5 ? 32 : 0)
                    | (o instanceof I6 ? 64 : 0) | (o instanceof I7 ? 128 : 0);
            s += bits * h.ref.val + h.ref.arr[i & 7];
        }
        return s;
    }

    static String bits(Object o) {
        int bits = (o instanceof I0 ? 1 : 0) | (o instanceof I1 ? 2 : 0) | (o instanceof I2 ? 4 : 0)
                | (o instanceof I3 ? 8 : 0) | (o instanceof I4 ? 16 : 0) | (o instanceof I5 ? 32 : 0)
                | (o instanceof I6 ? 64 : 0) | (o instanceof I7 ? 128 : 0);
        return Integer.toString(bits);
    }

    public static void main(String[] args) throws Exception {
        Object[] os = {new K0(), new K1(), new K2(), new K3(), new K4(), new K5(), new K6(), new K7(), new K8(),
            new K9(), "s", null};
        StringBuilder sb = new StringBuilder("bits cold");
        for (Object o : os) {
            sb.append(' ').append(bits(o));
        }
        System.out.println(sb);
        Holder h = new Holder();
        h.ref = new Payload(3);
        System.out.println("loop cold " + loop(h, os, 1000));
        // Warm without the collector thread.
        long w = 0;
        for (int r = 0; r < 300; r++) {
            w += loop(h, os, 1000);
        }
        System.out.println("warm " + w);
        Thread churn = new Thread(() -> {
            ArrayList<Object> junk = new ArrayList<>();
            int k = 0;
            while (!stop) {
                for (int i = 0; i < 2000; i++) {
                    junk.add(new long[16]);
                }
                if (junk.size() > 40_000) {
                    junk.clear();
                }
                if ((++k & 3) == 0) {
                    System.gc();
                }
            }
        });
        churn.start();
        long total = 0;
        for (int round = 0; round < 12; round++) {
            h.ref = new Payload(round + 5);
            total = total * 31 + loop(h, os, 250_000);
        }
        System.out.println("under gc " + total);
        // Deopt trigger: a class, a lambda and a Proxy first seen now.
        Object lambda = (I7 & Runnable) () -> { };
        Object proxy = Proxy.newProxyInstance(R21Fz2InstMem2.class.getClassLoader(), new Class<?>[] {I6.class, I3.class},
                (p, m, a) -> null);
        Object late = new K9() {
        };
        Object[] os2 = {late, lambda, proxy, new K5(), new K0()};
        sb = new StringBuilder("bits late");
        for (Object o : os2) {
            sb.append(' ').append(bits(o));
        }
        System.out.println(sb);
        long total2 = 0;
        for (int round = 0; round < 6; round++) {
            h.ref = new Payload(round + 50);
            total2 = total2 * 31 + loop(h, os2, 250_000) + loop(h, os, 1000);
        }
        stop = true;
        churn.join();
        System.out.println("after gc " + total2);
        sb = new StringBuilder("bits after");
        for (Object o : os) {
            sb.append(' ').append(bits(o));
        }
        System.out.println(sb);
    }
}
