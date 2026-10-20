// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcj j2/jn2 (2026-10-19): a JNI {@code CallIntMethod} up-call runs the method
 * its method ID names, selected as HotSpot's {@code jni_invoke_nonstatic}
 * selects it (JVMS 5.4.6): a private method ID runs as resolved, whatever
 * same-signature private method the receiver's class declares
 * ({@code docs/known-issues/gc/gcj-j2jn2-jni-virtual-upcall-runs-a-private-or-non-overriding-hider-20261019.md}).
 * Native half: {@code tools/probes/jni/Gcj2Jn2UpcallSelectProbe.c} (build line
 * there).
 *
 * <p>HotSpot ({@code java -XX:+UseSerialGC}) prints:
 *
 * <pre>
 *   private-mid-on-sub 1
 *   private-mid-on-own 1
 *   override-from-super-mid 200
 *   override-from-sub-mid 200
 *   inherited-from-sub-mid 7
 *   private-mid-via-a 1
 *   PASS
 * </pre>
 *
 * <p>Usage: {@code Gcj2Jn2UpcallSelectProbe <library> [reps]} -- {@code reps}
 * repeats every call (default 1) and prints nothing more; the answer of the
 * last repetition is printed.
 */
public final class Gcj2Jn2UpcallSelectProbe {

    static class A {
        private int m() {
            return 1;
        }

        public int v() {
            return 100;
        }

        public int inherited() {
            return 7;
        }
    }

    static final class B extends A {
        // Not an override: A.m is private (JVMS 5.4.5).
        private int m() {
            return 2;
        }

        @Override
        public int v() {
            return 200;
        }
    }

    /** {@code GetMethodID(clazz, name, sig)}, then {@code CallIntMethod(recv, mid)}. */
    static native int callInt(Object recv, Class<?> clazz, String name, String sig);

    /** As {@link #callInt}, through {@code CallIntMethodA}. */
    static native int callIntA(Object recv, Class<?> clazz, String name, String sig);

    public static void main(String[] args) {
        System.load(args[0]);
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 1;
        A a = new A();
        B b = new B();
        String[] names = {
            "private-mid-on-sub",
            "private-mid-on-own",
            "override-from-super-mid",
            "override-from-sub-mid",
            "inherited-from-sub-mid",
            "private-mid-via-a",
        };
        int[] expected = {1, 1, 200, 200, 7, 1};
        int[] got = new int[names.length];
        for (int r = 0; r < reps; r++) {
            got[0] = callInt(b, A.class, "m", "()I");
            got[1] = callInt(a, A.class, "m", "()I");
            got[2] = callInt(b, A.class, "v", "()I");
            got[3] = callInt(b, B.class, "v", "()I");
            got[4] = callInt(b, B.class, "inherited", "()I");
            got[5] = callIntA(b, A.class, "m", "()I");
        }
        boolean ok = true;
        for (int i = 0; i < names.length; i++) {
            System.out.println(names[i] + " " + got[i]);
            ok &= got[i] == expected[i];
        }
        System.out.println(ok ? "PASS" : "FAIL");
    }
}
