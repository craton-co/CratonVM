// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.function.IntSupplier;

/** A throw loop whose stack holds a lambda body called through an interface (JUnit's shape). */
public class ThrowableMemoLambdaTaskProbe {
    static int sink;

    static void work(Runnable task) {
        task.run();
    }

    static void descend(int depth, Runnable task) {
        if (depth == 0) {
            work(task);
        } else {
            descend(depth - 1, task);
        }
    }

    static void dump(Throwable t) {
        for (StackTraceElement e : t.getStackTrace()) {
            System.out.println(e.getClassName() + "." + e.getMethodName() + ":" + e.getLineNumber());
        }
    }

    public static void main(String[] args) {
        Throwable[] last = new Throwable[1];
        Runnable task = () -> {
            Throwable t = new Throwable();
            sink++;
            last[0] = t;
        };
        for (int i = 0; i < 200_000; i++) {
            descend(12, task);
        }
        dump(last[0]);
        IntSupplier depth = () -> last[0].getStackTrace().length;
        System.out.println("depth=" + depth.getAsInt() + " sink=" + sink);
        System.out.println("LAMBDA-MEMO-OK");
    }
}
