// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Spins ONE stack/reflection operation for a fixed wall time so the execution sampler
// (CRATONVM_PROFILE_SAMPLE_MS) and the native census (--dump-native-registry) see a steady state.
//   cratonvm [--jdk-only] -cp . StackSpin <throwable|walker|walkerall|invoke> <seconds>
import java.lang.reflect.Method;
import java.util.stream.Collectors;

public class StackSpin {
    static long sink;

    static int deep(int d, Runnable r) { if (d == 0) { r.run(); return 0; } return 1 + deep(d - 1, r); }

    public static void main(String[] a) throws Exception {
        String which = a[0];
        long end = System.nanoTime() + Long.parseLong(a[1]) * 1_000_000_000L;
        StackWalker w = StackWalker.getInstance();
        Method len = String.class.getMethod("length");
        long calls = 0;
        while (System.nanoTime() < end) {
            for (int i = 0; i < 200; i++) {
                switch (which) {
                    case "throwable" -> sink += new Throwable().getStackTrace().length;
                    case "walker" -> sink += w.walk(st -> st.findFirst().get().getMethodName().length());
                    case "walkerall" -> sink += w.walk(st -> st.collect(Collectors.toList())).size();
                    case "invoke" -> sink += (Integer) len.invoke("hello");
                    default -> throw new IllegalArgumentException(which);
                }
            }
            calls += 200;
        }
        System.out.println(which + " calls=" + calls + " sink=" + sink);
    }
}
