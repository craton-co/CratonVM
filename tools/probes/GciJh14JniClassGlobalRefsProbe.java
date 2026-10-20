/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 */

/**
 * gci i14/jh (2026-10-13): a {@code jclass} reached through every handle kind
 * -- the local {@code GetObjectClass} answer, a {@code Class} parameter, a
 * {@code NewGlobalRef} / {@code NewWeakGlobalRef} of either, and a
 * {@code NewLocalRef} of a global -- driven through every JNI function that
 * consumes a {@code jclass}, against HotSpot 25; plus {@code GetStringLength}
 * of LATIN1, UTF16, empty, lone-surrogate and long strings. Every stdout line
 * is HotSpot's exact answer; diagnostics go to stderr. Native half:
 * {@code tools/probes/jni/GciJh14JniClassGlobalRefsProbe.c}.
 *
 * <p>Usage: {@code GciJh14JniClassGlobalRefsProbe /abs/path/gcijh14.dll}.
 *
 * <ul>
 *   <li>{@code cls-<kind>:} for each handle kind, the answers of IsSameObject,
 *       GetSuperclass, IsAssignableFrom (both ways), IsInstanceOf, GetMethodID
 *       + CallIntMethodA, CallNonvirtualIntMethodA, GetStaticMethodID +
 *       CallStaticIntMethodA, GetFieldID + GetIntField, GetStaticFieldID +
 *       Get/SetStaticIntField, NewObjectA, AllocObject, NewObjectArray +
 *       SetObjectArrayElement, ToReflectedMethod / ToReflectedField /
 *       FromReflectedMethod, MonitorEnter / MonitorExit, GetModule,
 *       UnregisterNatives / RegisterNatives + a call of the bound native,
 *       GetObjectClass, and how many of them left an exception pending;
 *   <li>{@code cls-reftype:} GetObjectRefType of each handle kind (HotSpot:
 *       1 for a local, 2 for a global, 3 for a weak global);
 *   <li>{@code throw-<k>:} ThrowNew through a Class parameter, a global and a
 *       weak global of it, a global of FindClass's answer, and FindClass's
 *       answer itself;
 *   <li>{@code strlen:} / {@code strlen-loop:} GetStringLength.
 * </ul>
 */
public final class GciJh14JniClassGlobalRefsProbe {
    static class Base {
    }

    static final class Box extends Base {
        static int S = 9;

        int v;

        Box(int v) {
            this.v = v;
        }

        int get() {
            return v + 4;
        }

        static int twice(int x) {
            return 2 * x;
        }

        /** Bound only through RegisterNatives (no exported symbol). */
        static native int reg();
    }

    static final String[] KINDS = {
        "local", "param", "global", "weak", "global-param", "weak-param", "local-of-global"
    };

    /** The handle of kind {@code kind} for {@code param}, through every consumer. */
    static native String classRefs(Class<?> param, Object instance, Class<?> base, int kind);

    /** GetObjectRefType of the handle of kind {@code kind}. */
    static native int refType(Class<?> param, Object instance, int kind);

    /** ThrowNew("via-k") through handle kind k of {@code exc}. */
    static native void throwVia(Class<?> exc, int kind);

    static native int strLen(String s);

    /** NewStringUTF(GetStringUTFChars(s)). */
    static native String utfRoundTrip(String s);

    static native int utfLen(String s);

    /** NewStringUTF of GetStringUTFRegion(s, start, len) (len at most 16). */
    static native String utfRegion(String s, int start, int len);

    static String units(String s) {
        if (s == null) {
            return "null";
        }
        StringBuilder sb = new StringBuilder("[");
        for (int i = 0; i < s.length(); i++) {
            if (i > 0) {
                sb.append(' ');
            }
            sb.append(Integer.toHexString(s.charAt(i)));
        }
        return sb.append(']').toString();
    }

    public static void main(String[] args) {
        if (args.length < 1) {
            System.out.println("usage: GciJh14JniClassGlobalRefsProbe /abs/path/gcijh14.dll");
            return;
        }
        System.load(args[0]);
        Box b = new Box(3);
        for (int k = 0; k < KINDS.length; k++) {
            System.out.println("cls-" + KINDS[k] + ": " + classRefs(Box.class, b, Base.class, k));
        }
        System.out.println("cls-static-after: S=" + Box.S);
        StringBuilder rt = new StringBuilder("cls-reftype:");
        for (int k = 0; k < KINDS.length; k++) {
            rt.append(' ').append(KINDS[k]).append('=').append(refType(Box.class, b, k));
        }
        System.out.println(rt);
        for (int k = 0; k < 5; k++) {
            try {
                throwVia(IllegalStateException.class, k);
                System.out.println("throw-" + k + ": none");
            } catch (IllegalStateException e) {
                System.out.println("throw-" + k + ": " + e.getClass().getName() + " "
                        + e.getMessage());
            }
        }
        String big = "x".repeat(100000);
        String bigUtf16 = "ā".repeat(70001);
        String built = new StringBuilder("ab").append('€').toString();
        System.out.println("strlen: latin1=" + strLen("abc") + " utf16=" + strLen("aαb")
                + " empty=" + strLen("") + " surrogate=" + strLen("a\uD800b") + " big="
                + strLen(big) + " big-utf16=" + strLen(bigUtf16) + " built=" + strLen(built)
                + " sub=" + strLen(big.substring(5, 17)));
        long sum = 0;
        for (int i = 0; i < 20000; i++) {
            sum += strLen((i & 1) == 0 ? built : bigUtf16);
        }
        System.out.println("strlen-loop: " + sum);
        String lone = "a\uD800b";
        String pair = "😀";
        String nul = "a\0b";
        System.out.println("utf: lone=" + units(utfRoundTrip(lone)) + " lone-len=" + utfLen(lone)
                + " pair=" + units(utfRoundTrip(pair)) + " pair-len=" + utfLen(pair) + " nul="
                + units(utfRoundTrip(nul)) + " nul-len=" + utfLen(nul) + " region-half="
                + units(utfRegion(pair, 0, 1)) + " region-lone=" + units(utfRegion(lone, 1, 2)));
        System.out.println("done");
    }
}
