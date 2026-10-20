/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 */

/**
 * gci i11/cf (2026-10-13): soundness of the virtual / interface JNI call-site
 * rows ({@code vm_exec} "gci i11/cf" block, inside
 * {@code CRATONVM_JNI_DISPATCH_MEMO}). Every case runs one call site long
 * enough for its row to serve most calls, then changes what the site sees:
 *
 * <ul>
 *   <li>{@code iface}: an {@code invokeinterface} of an instance JNI native;
 *   <li>{@code phases}: ONE {@code invokevirtual} site fed runs of receivers of
 *       four classes in turn -- the declaring class {@code A} (JNI, x+1), an
 *       heir {@code B} (A's native), {@code C} with its own JNI override (x+2)
 *       and {@code D} with a bytecode override (x+3): a row is for one receiver
 *       class, and every switch must run the receiver's own selection;
 *   <li>{@code args}: a reference argument allocated per call (the hit pops
 *       and forwards it like the slow path; run it with a GC stress env);
 *   <li>{@code throws}: a native that throws every 16th call.
 * </ul>
 *
 * <p>stdout is HotSpot's exact answers; diagnostics on stderr. Usage:
 * {@code GciCf11JniVirtualSiteProbe /abs/path/gcicf11.dll [n]}. Native half:
 * {@code tools/probes/jni/GciCf11StaticArgLoaderProbe.c} (one library for both
 * i11/cf probes).
 */
public final class GciCf11JniVirtualSiteProbe {
    interface I {
        int get(int x);
    }

    static final class Impl implements I {
        @Override
        public native int get(int x);
    }

    static class A {
        native int val(int x);

        native int len(byte[] b);

        native int throwIf(int x);
    }

    static class B extends A {
    }

    static class C extends A {
        @Override
        native int val(int x);
    }

    static class D extends A {
        @Override
        int val(int x) {
            return x + 3;
        }
    }

    static long iface(I i, int n) {
        long s = 0;
        for (int k = 0; k < n; k++) {
            s += i.get(k);
        }
        return s;
    }

    static long phase(A a, int from, int n) {
        long s = 0;
        for (int k = from; k < from + n; k++) {
            s += a.val(k);
        }
        return s;
    }

    public static void main(String[] args) {
        if (args.length < 1) {
            System.out.println("usage: GciCf11JniVirtualSiteProbe /abs/path/gcicf11.dll [n]");
            return;
        }
        System.load(args[0]);
        final int n = args.length > 1 ? Integer.parseInt(args[1]) : 200_000;

        System.out.println("iface: " + iface(new Impl(), n));

        A[] kinds = {new A(), new B(), new C(), new D()};
        long s = 0;
        int from = 0;
        final int run = 997;
        for (int round = 0; round < 12; round++) {
            for (int k = 0; k < kinds.length; k++) {
                // A, B, C, D, then B, A, D, C, ...: every neighbour pair.
                A a = kinds[(round & 1) == 0 ? k : (k + 1) % kinds.length];
                s += phase(a, from, run);
                from += run;
            }
        }
        long mixed = 0;
        for (int k = 0; k < n; k++) {
            mixed += kinds[k % 3 == 0 ? 2 : (k & 1)].val(k);
        }
        System.out.println("phases: " + s + " mixed: " + mixed);

        A b = new B();
        long lens = 0;
        for (int k = 0; k < n; k++) {
            lens += b.len(new byte[k & 63]);
        }
        System.out.println("args: " + lens);

        int caught = 0;
        long thrownSum = 0;
        for (int k = 0; k < n; k++) {
            try {
                thrownSum += b.throwIf(k % 16);
            } catch (IllegalStateException e) {
                if ("seven".equals(e.getMessage())) {
                    caught++;
                }
            }
        }
        System.out.println("throws: " + caught + " sum: " + thrownSum);
        System.err.println("[gci-cf11-vsite] done n=" + n);
        System.out.println("done");
    }
}
