// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 79, lane L7: behaviour of the frames
// `interpreter::execute` pushes (reflective calls, class initialisers, `main`)
// with CRATONVM_EXECUTE_CACHED_REENTRY_FRAME=1 (a cached frame from a per-VM
// record) and without it (an owned frame). Every row must print the same in
// both states and match HotSpot. Each reflective row runs three times, so the
// second and third calls are served by the record the first one filed.
//
// Rows:
//   frames       the probe-class frames of a stack trace taken inside a
//                reflectively invoked method
//   walker       the probe-class frames StackWalker sees from the same place
//   thrown       a reflectively invoked method throwing (the cause's class
//                and message through InvocationTargetException)
//   caught       a reflectively invoked method whose own handler catches
//   sync         a static synchronized method invoked reflectively holds the
//                class monitor
//   wide-args    long / double / int parameters through Method.invoke
//   recursion    a method that invokes itself reflectively five deep
//   clinit       a static initialiser with a loop (run from the VM's side)
//
// Run: java L7W79ReentryFrame, in --jdk-only and --compatible, with and
// without --nojit, each with and without CRATONVM_EXECUTE_CACHED_REENTRY_FRAME=1.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// frames target,main x3
// walker walked,main x3
// thrown java.lang.IllegalStateException: boom 7 x3
// caught -1 x3
// sync true x3
// wide-args 11.0 x3
// recursion 15 x3
// clinit 3000
// HOTSPOT_EXPECTED_END
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

public class L7W79ReentryFrame {
    static final String SELF = "L7W79ReentryFrame";

    static final class Table {
        static final int SUM;

        static {
            int[] t = new int[16];
            for (int i = 0; i < 1000; i++) {
                t[i & 15] += 3;
            }
            int s = 0;
            for (int v : t) s += v;
            SUM = s;
        }
    }

    public static String target() {
        List<String> names = new ArrayList<>();
        for (StackTraceElement e : new Throwable().getStackTrace()) {
            if (e.getClassName().equals(SELF)) names.add(e.getMethodName());
        }
        return String.join(",", names);
    }

    public static String walked() {
        List<String> names = StackWalker.getInstance().walk(s -> s
                .filter(f -> f.getClassName().equals(SELF))
                .map(StackWalker.StackFrame::getMethodName)
                .toList());
        return String.join(",", names);
    }

    public static int thrower(int x) {
        throw new IllegalStateException("boom " + x);
    }

    public static int guarded(int a) {
        try {
            if (a < 0) throw new IllegalArgumentException("negative");
            return a + 1;
        } catch (IllegalArgumentException e) {
            return -1;
        }
    }

    public static synchronized boolean locked() {
        return Thread.holdsLock(L7W79ReentryFrame.class);
    }

    public static double mix(long a, double b, int c) {
        return a + b * c;
    }

    public static int down(int depth) throws Exception {
        if (depth == 0) return 0;
        Method self = L7W79ReentryFrame.class.getMethod("down", int.class);
        return depth + (Integer) self.invoke(null, depth - 1);
    }

    static final Map<String, List<String>> rows = new LinkedHashMap<>();

    static void note(String row, Object value) {
        rows.computeIfAbsent(row, k -> new ArrayList<>()).add(String.valueOf(value));
    }

    public static void main(String[] args) throws Exception {
        Class<?> c = L7W79ReentryFrame.class;
        Method target = c.getMethod("target");
        Method walked = c.getMethod("walked");
        Method thrower = c.getMethod("thrower", int.class);
        Method guarded = c.getMethod("guarded", int.class);
        Method locked = c.getMethod("locked");
        Method mix = c.getMethod("mix", long.class, double.class, int.class);
        Method down = c.getMethod("down", int.class);
        for (int round = 0; round < 3; round++) {
            note("frames", target.invoke(null));
            note("walker", walked.invoke(null));
            try {
                thrower.invoke(null, 7);
                note("thrown", "no exception");
            } catch (InvocationTargetException e) {
                Throwable t = e.getCause();
                note("thrown", t.getClass().getName() + ": " + t.getMessage());
            }
            note("caught", guarded.invoke(null, -5));
            note("sync", locked.invoke(null));
            note("wide-args", mix.invoke(null, 5L, 1.5, 4));
            note("recursion", down.invoke(null, 5));
        }
        for (Map.Entry<String, List<String>> e : rows.entrySet()) {
            List<String> vs = e.getValue();
            boolean same = vs.stream().allMatch(v -> v.equals(vs.get(0)));
            System.out.println(e.getKey() + " " + (same ? vs.get(0) + " x" + vs.size() : String.join(" | ", vs)));
        }
        Class.forName(SELF + "$Table");
        System.out.println("clinit " + Table.SUM);
    }
}
