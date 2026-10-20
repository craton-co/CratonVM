// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 79, lane L3
// (docs/internal/fixed-bugs/interpreter-i77-L3-method-invoke-depth-guard-throws-stackoverflowerror-at-101-nested-calls-FIXED-20261111.md):
// a static method that recurses through `Method.invoke`. CratonVM used to
// refuse the 101st nested reflective call with StackOverflowError (a fixed
// count of 100); HotSpot runs it. Rows `depth-N` nest N reflective levels on
// the main thread; row `unbounded` recurses without end on a thread with a
// 512 KiB stack request and prints what reaches the top (the outermost
// `Method.invoke` wraps the overflow in InvocationTargetException; the root
// of the cause chain is the StackOverflowError) and that the VM goes on.
//
// Run --compatible and --jdk-only, default and --nojit.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// depth-100 100
// depth-101 101
// depth-250 250
// depth-400 400
// unbounded top=java.lang.reflect.InvocationTargetException root=java.lang.StackOverflowError
// after ok
// HOTSPOT_EXPECTED_END
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L3W79NestedReflectDepth {
    static Method DOWN;
    static Method FOREVER;

    public static int down(int n) throws Exception {
        if (n == 0) {
            return 0;
        }
        return (Integer) DOWN.invoke(null, n - 1) + 1;
    }

    public static int forever(int n) throws Exception {
        return (Integer) FOREVER.invoke(null, n + 1) + 1;
    }

    public static void main(String[] args) throws Exception {
        DOWN = L3W79NestedReflectDepth.class.getMethod("down", int.class);
        FOREVER = L3W79NestedReflectDepth.class.getMethod("forever", int.class);
        for (int n : new int[] {100, 101, 250, 400}) {
            try {
                System.out.println("depth-" + n + " " + DOWN.invoke(null, n));
            } catch (Throwable t) {
                System.out.println("depth-" + n + " " + t.getClass().getName());
            }
        }
        String[] out = new String[1];
        Thread t = new Thread(null, () -> {
            try {
                FOREVER.invoke(null, 0);
                out[0] = "returned";
            } catch (Throwable e) {
                Throwable root = e;
                while (root.getCause() != null && root.getCause() != root) {
                    root = root.getCause();
                }
                out[0] = "top=" + e.getClass().getName() + " root=" + root.getClass().getName();
            }
        }, "deep", 512 * 1024);
        t.start();
        t.join();
        System.out.println("unbounded " + out[0]);
        System.out.println("after " + (DOWN.invoke(null, 10).equals(10) ? "ok" : "bad"));
    }
}
