/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 */

/**
 * gci i10/cr (2026-10-13): {@code execute_invokestatic}'s by-name fallback
 * on a STATIC call ({@code i9-cs-report.md} section 6 item 2). A JNI-bound
 * static native declared in {@code P} and called through the subclass
 * reference {@code C.pn(..)} (javac emits {@code invokestatic C.pn}) reaches
 * the interpreter's fallback; JVMS 5.5 initializes the DECLARING class
 * {@code P} only, so {@code C}'s initializer runs when {@code C} itself is
 * first touched, after the calls. The instance entry the fallback used
 * ({@code invoke_or_native}, {@code ByNameKind::Virtual}) initialized the
 * symbolic owner {@code C} from its tail.
 *
 * <p>stdout is HotSpot's exact output (compared byte for byte); diagnostics
 * go to stderr. Usage: {@code GciCr10StaticFallbackProbe /abs/path/gcicr10staticfallback.dll [n]}.
 * Native half: {@code tools/probes/jni/GciCr10StaticFallbackProbe.c}.
 */
public class GciCr10StaticFallbackProbe {
    static class P {
        static {
            System.out.println("P-init");
        }

        static native int pn(int x);
    }

    static class C extends P {
        static {
            System.out.println("C-init");
        }

        static int touch() {
            return 1;
        }
    }

    public static void main(String[] args) {
        if (args.length < 1) {
            System.out.println("usage: GciCr10StaticFallbackProbe /abs/path/gcicr10staticfallback.dll [n]");
            return;
        }
        System.load(args[0]);
        final int n = args.length > 1 ? Integer.parseInt(args[1]) : 1;
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += C.pn(i);
        }
        System.out.println("inherited-static: " + s);
        System.out.println("before-touch");
        System.out.println("touch: " + C.touch());
        System.err.println("[gci-cr10-fallback] done n=" + n);
        System.out.println("done");
    }
}
