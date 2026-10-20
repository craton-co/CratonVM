// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 134, lane L4 (pages
// docs/known-issues/interpreter/i132-L6-interface-invokespecial-skips-the-receiver-subtype-check-20261230.md,
// "Wave 134"; review of w133, doubtful items 1 and 2). An `invokespecial`
// whose sender is an interface (javac's `I.super.m()` inside a default method
// of a subinterface) is never cached and is checked on every execution.
//
//   class-recv    J.n() { I.super.m(); } on a class instance, 300000 calls
//   lambda-recv   the same shape with a lambda proxy as `this`: F extends G,
//                 F.k() { G.super.h(); } on `F f = () -> ...` (a VM-minted
//                 proxy must pass the check, never IllegalAccessError)
//   anon-recv     the same as lambda-recv on an anonymous class
//
// Run: javac -d . L4W134IfaceSuperLoop.java
//      java -cp . L4W134IfaceSuperLoop                 (HotSpot; -Xint prints the same)
//      for each of --jdk-only / --compatible, with and without --nojit:
//        cratonvm $mode -cp . L4W134IfaceSuperLoop
//        CRATONVM_DBG_FLAGREADS=1 cratonvm $mode --nojit -cp . L4W134IfaceSuperLoop
//            (pass: no `CRATONVM_INTERFACE_SPECIAL_RECEIVER_CHECK` row in any report)
//        CRATONVM_INTERFACE_SPECIAL_RECEIVER_CHECK=0 cratonvm $mode -cp . L4W134IfaceSuperLoop
//            (the escape: same output)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags and -Xint)
// class-recv: sum=300000
// lambda-recv: sum=900000 run=300000
// anon-recv: sum=600000
// HOTSPOT_EXPECTED_END

public class L4W134IfaceSuperLoop {
    interface I {
        default int m() {
            return 1;
        }
    }

    interface J extends I {
        default int n() {
            return I.super.m();
        }
    }

    static final class C implements J {
    }

    interface G {
        default int h() {
            return 2;
        }
    }

    interface F extends G {
        int run();

        default int k() {
            return G.super.h() + 1;
        }
    }

    static int runs;

    public static void main(String[] args) {
        final int n = 300_000;

        J j = new C();
        long sum = 0;
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            sum += j.n();
        }
        long t1 = System.nanoTime();
        System.out.println("class-recv: sum=" + sum);
        System.out.println("TIME class-recv ns/op=" + ((t1 - t0) / n));

        F f = () -> {
            runs++;
            return 0;
        };
        sum = 0;
        t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            sum += f.k() + f.run();
        }
        t1 = System.nanoTime();
        System.out.println("lambda-recv: sum=" + sum + " run=" + runs);
        System.out.println("TIME lambda-recv ns/op=" + ((t1 - t0) / n));

        F a = new F() {
            @Override
            public int run() {
                return -1;
            }
        };
        sum = 0;
        for (int i = 0; i < n; i++) {
            sum += a.k() + a.run();
        }
        System.out.println("anon-recv: sum=" + sum);
    }
}
