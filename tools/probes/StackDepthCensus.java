// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// How many frames does each engine report at a known recursion depth? A mode that reports FEWER frames than
// HotSpot does less work per stack walk / Throwable, so a per-call time comparison against it flatters that mode.
//   java StackDepthCensus > hs.txt ; cratonvm [--jdk-only] -cp . StackDepthCensus | diff hs.txt -
import java.util.stream.Collectors;

public class StackDepthCensus {
    static int deep(int d, Runnable r) { if (d == 0) { r.run(); return 0; } return 1 + deep(d - 1, r); }

    public static void main(String[] a) {
        for (int depth : new int[] {0, 5, 50}) {
            deep(depth, () -> {
                StackWalker w = StackWalker.getInstance();
                System.out.println("depth " + depth
                    + "  walk.count=" + w.walk(s -> s.count())
                    + "  getStackTrace=" + new Throwable().getStackTrace().length
                    + "  walk.SHOW_REFLECT=" + StackWalker.getInstance(StackWalker.Option.SHOW_REFLECT_FRAMES).walk(s -> s.count())
                    + "  first=" + w.walk(s -> s.findFirst().get().getMethodName())
                    + "  classes=" + w.walk(s -> s.limit(3).map(f -> f.getClassName()).collect(Collectors.joining(","))));
            });
        }
        // and through reflection, as Mockito does
        try {
            java.lang.reflect.Method m = StackDepthCensus.class.getDeclaredMethod("viaReflection");
            m.invoke(null);
        } catch (Exception e) { throw new RuntimeException(e); }
    }

    static void viaReflection() {
        System.out.println("via Method.invoke  walk.count=" + StackWalker.getInstance().walk(s -> s.count())
            + "  SHOW_REFLECT=" + StackWalker.getInstance(StackWalker.Option.SHOW_REFLECT_FRAMES).walk(s -> s.count())
            + "  getStackTrace=" + new Throwable().getStackTrace().length);
    }
}
