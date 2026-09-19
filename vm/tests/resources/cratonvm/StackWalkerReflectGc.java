// JAVA21+
package cratonvm;

import java.lang.StackWalker;
import java.util.stream.Collectors;

public class StackWalkerReflectGc {
    private static final StackWalker WALKER =
            StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE);
    private static volatile Object sink;

    public static void main(String[] args) {
        String frames = recurse(0);
        if (frames.isEmpty() || !frames.contains("StackWalkerReflectGc#recurse")) {
            throw new AssertionError(frames);
        }
        System.out.println("STACKWALKER_REFLECT_GC_OK");
    }

    private static String recurse(int depth) {
        Object[] live = new Object[8];
        for (int i = 0; i < live.length; i++) {
            live[i] = new byte[4096];
        }
        sink = live;
        if (depth >= 48) {
            return leaf();
        }
        return recurse(depth + 1);
    }

    private static String leaf() {
        for (int i = 0; i < 32; i++) {
            sink = new byte[8192];
        }
        return WALKER.walk(frames -> frames
                .skip(32)
                .limit(16)
                .map(f -> f.getDeclaringClass().getName() + "#" + f.getMethodName())
                .collect(Collectors.joining("|")));
    }
}
