// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 121, lane L3: `Reflection.getCallerClass()` under reflective and
// served `Thread.run` calls (`CRATONVM_REFLECTION_CALLER_FAST_UNDER_RECORDS`, default off; the
// i114-L7 page, wave 121). Every caller-sensitive answer below is asked from code that runs
// inside a recorded call: a worker thread (the served `Thread.run`), a method `Method.invoke`
// called (a reflective call's row in `--compatible`), and the asker being the reflective
// call's own first frame (`Class.forName` invoked reflectively: the fast arm must decline).
// The answers must not move with the switch; `CRATONVM_DBG_CALLER=1` prints no
// `FAST MISMATCH` line with it on. The TIME lines (not compared) price `invoke-checked` on the
// main thread and on a worker, the row L7W120ReflectSplitBench measures on main only. HotSpot 25
// -Xint on the lane's machine: main 1115, worker 1119 ns/op (no thread difference; CratonVM's
// worker row took the whole capture per ask before the switch).
//
// Run: cratonvm --java-home <jdk25> --jdk-only -cp <dir> L3W121CallerFastUnderRecords
//      CRATONVM_REFLECTION_CALLER_FAST_UNDER_RECORDS=1 (same lines), and --compatible both ways.
//      With CRATONVM_DBG_CALLER=1: no line contains FAST MISMATCH.
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// main lookup=L3W121CallerFastUnderRecords
// main refused=class L3W121CallerFastUnderRecords cannot access a member of class L3W121Private with modifiers "private static"
// main forName=L3W121Private
// main reflective-forName=L3W121Private
// worker lookup=L3W121CallerFastUnderRecords
// worker refused=class L3W121CallerFastUnderRecords cannot access a member of class L3W121Private with modifiers "private static"
// worker forName=L3W121Private
// worker reflective-forName=L3W121Private
// nested lookup=L3W121CallerFastUnderRecords$Nested
// nested refused=class L3W121CallerFastUnderRecords$Nested cannot access a member of class L3W121Private with modifiers "private static"
// nested forName=L3W121Private
// nested reflective-forName=L3W121Private
// worker-nested lookup=L3W121CallerFastUnderRecords$Nested
// worker-nested refused=class L3W121CallerFastUnderRecords$Nested cannot access a member of class L3W121Private with modifiers "private static"
// worker-nested forName=L3W121Private
// worker-nested reflective-forName=L3W121Private
// checked checksum main=12502500 worker=12502500
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandles;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L3W121CallerFastUnderRecords {
    public static int inc(int x) {
        return x + 1;
    }

    static final Method SECRET;
    static final Method FOR_NAME;
    static final Method INC;

    static {
        try {
            SECRET = L3W121Private.class.getDeclaredMethod("secret");
            FOR_NAME = Class.class.getMethod("forName", String.class);
            INC = L3W121CallerFastUnderRecords.class.getMethod("inc", int.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static void ask(String tag) {
        System.out.println(tag + " lookup=" + MethodHandles.lookup().lookupClass().getName());
        try {
            SECRET.invoke(null);
            System.out.println(tag + " refused=none");
        } catch (IllegalAccessException e) {
            System.out.println(tag + " refused=" + e.getMessage());
        } catch (InvocationTargetException e) {
            System.out.println(tag + " refused=ite " + e.getCause());
        }
        try {
            System.out.println(tag + " forName="
                    + Class.forName("L3W121Private").getName());
            System.out.println(tag + " reflective-forName="
                    + ((Class<?>) FOR_NAME.invoke(null, "L3W121Private")).getName());
        } catch (ReflectiveOperationException e) {
            System.out.println(tag + " forName threw " + e);
        }
    }

    public static class Nested {
        public static void run(String tag) {
            System.out.println(tag + " lookup=" + MethodHandles.lookup().lookupClass().getName());
            try {
                SECRET.invoke(null);
                System.out.println(tag + " refused=none");
            } catch (IllegalAccessException e) {
                System.out.println(tag + " refused=" + e.getMessage());
            } catch (InvocationTargetException e) {
                System.out.println(tag + " refused=ite " + e.getCause());
            }
            try {
                System.out.println(tag + " forName="
                        + Class.forName("L3W121Private").getName());
                System.out.println(tag + " reflective-forName="
                        + ((Class<?>) FOR_NAME.invoke(null, "L3W121Private")).getName());
            } catch (ReflectiveOperationException e) {
                System.out.println(tag + " forName threw " + e);
            }
        }
    }

    static long checked(int n) throws Exception {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += (Integer) INC.invoke(null, i);
        }
        return s;
    }

    static long timed(long[] out) throws Exception {
        final int n = 5000;
        long checksum = checked(n);
        long best = Long.MAX_VALUE;
        for (int rep = 0; rep < 5; rep++) {
            long t0 = System.nanoTime();
            checked(n);
            best = Math.min(best, System.nanoTime() - t0);
        }
        out[0] = best / n;
        return checksum;
    }

    public static void main(String[] args) throws Throwable {
        Method nested = Nested.class.getMethod("run", String.class);
        ask("main");
        Thread t = new Thread(() -> ask("worker"));
        t.start();
        t.join();
        nested.invoke(null, "nested");
        Throwable[] failed = new Throwable[1];
        Thread t2 = new Thread(() -> {
            try {
                nested.invoke(null, "worker-nested");
            } catch (Throwable e) {
                failed[0] = e;
            }
        });
        t2.start();
        t2.join();
        if (failed[0] != null) {
            System.out.println("worker-nested threw " + failed[0]);
        }
        long[] mainTime = new long[1];
        long mainSum = timed(mainTime);
        long[] workerTime = new long[1];
        long[] workerSum = new long[1];
        Thread t3 = new Thread(() -> {
            try {
                workerSum[0] = timed(workerTime);
            } catch (Exception e) {
                failed[0] = e;
            }
        });
        t3.start();
        t3.join();
        System.out.println("checked checksum main=" + mainSum + " worker=" + workerSum[0]);
        System.out.println("TIME invoke-checked main " + mainTime[0] + " ns/op (min of 5)");
        System.out.println("TIME invoke-checked worker " + workerTime[0] + " ns/op (min of 5)");
    }
}

// Not a nestmate of the probe's classes: `Method.invoke`'s access check names the caller.
class L3W121Private {
    private static int secret() {
        return 1;
    }
}
