// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i5 wave 109, lane L4: the A/B of `CRATONVM_MH_IWA_JDK_BODY`
// (default off). With the switch, `MethodHandle.invokeWithArguments` on a
// real handle runs the JDK's own body (asType + spread invoker's spun
// LambdaForms) instead of the served door, so a SHOW_HIDDEN_FRAMES walk lists
// HotSpot's two `LambdaForm$MH::invoke` frames (page
// `gap-hidden-walks-lack-hotspots-spun-lambda-form-frames`). This bench
// measures what that costs per call, warm, one row at a time:
//
//   static-int        findStatic (int)int, invokeWithArguments(Integer)
//   static-ref        findStatic (String,String)String
//   virtual           findVirtual String.length, invokeWithArguments(receiver)
//   bound             bindTo of a (String,String)String virtual, one argument
//   list              the List overload of static-ref
//   varargs-control   CONTROL: a varargs collector (stays on the door in both arms)
//   wmte              arity mismatch: the WrongMethodTypeException's class
//                     (checked once, not timed)
//
// Output: "TIME <row> med <ns> min <ns> ns/op" and "checksum <row> <n>"
// (deterministic, in HOTSPOT_EXPECTED below for a run with no arguments).
// Args: a number = calls per sample (default 20000); a comma-separated list
// of rows (default all).
//
// Run: javac -d . L4W109InvokeWithArgumentsBodyBench.java
//      java -cp . L4W109InvokeWithArgumentsBodyBench                     (HotSpot reference)
//      cratonvm --jdk-only -cp . L4W109InvokeWithArgumentsBodyBench
//      CRATONVM_MH_IWA_JDK_BODY=1 cratonvm --jdk-only -cp . L4W109InvokeWithArgumentsBodyBench
//      (and the same two with --compatible, and with --nojit)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// checksum static-int 20000
// checksum static-ref 20000
// checksum virtual 20000
// checksum bound 20000
// checksum list 20000
// checksum varargs-control 20000
// wmte java.lang.invoke.WrongMethodTypeException
// HOTSPOT_EXPECTED_END

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

public class L4W109InvokeWithArgumentsBodyBench {
    static int twice(int x) {
        return x * 2;
    }

    static String cat(String a, String b) {
        return a + b;
    }

    static int count(Object... xs) {
        return xs.length;
    }

    interface Body {
        boolean run() throws Throwable;
    }

    static int calls = 20000;

    static void row(String name, Body body) throws Throwable {
        for (int i = 0; i < calls; i++) {
            body.run(); // warm
        }
        long[] samples = new long[7];
        int ok = 0;
        for (int s = 0; s < samples.length; s++) {
            int good = 0;
            long t0 = System.nanoTime();
            for (int i = 0; i < calls; i++) {
                if (body.run()) {
                    good++;
                }
            }
            samples[s] = (System.nanoTime() - t0) / calls;
            ok = good;
        }
        Arrays.sort(samples);
        System.out.println("TIME " + name + " med " + samples[samples.length / 2] + " min "
                + samples[0] + " ns/op");
        System.out.println("checksum " + name + " " + ok);
    }

    public static void main(String[] args) throws Throwable {
        List<String> rows = null;
        for (String a : args) {
            if (a.matches("\\d+")) {
                calls = Integer.parseInt(a);
            } else if (!a.equals("all")) {
                rows = Arrays.asList(a.split(","));
            }
        }
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> me = L4W109InvokeWithArgumentsBodyBench.class;
        MethodHandle twice = l.findStatic(me, "twice", MethodType.methodType(int.class, int.class));
        MethodHandle cat = l.findStatic(me, "cat",
                MethodType.methodType(String.class, String.class, String.class));
        MethodHandle length = l.findVirtual(String.class, "length", MethodType.methodType(int.class));
        MethodHandle concat = l.findVirtual(String.class, "concat",
                MethodType.methodType(String.class, String.class)).bindTo("x");
        MethodHandle count = l.findStatic(me, "count",
                MethodType.methodType(int.class, Object[].class));
        Integer seven = 7;
        List<Object> pair = new ArrayList<>(List.of("a", "b"));

        if (rows == null || rows.contains("static-int")) {
            row("static-int", () -> ((Integer) twice.invokeWithArguments(seven)) == 14);
        }
        if (rows == null || rows.contains("static-ref")) {
            row("static-ref", () -> "ab".equals(cat.invokeWithArguments("a", "b")));
        }
        if (rows == null || rows.contains("virtual")) {
            row("virtual", () -> ((Integer) length.invokeWithArguments("abc")) == 3);
        }
        if (rows == null || rows.contains("bound")) {
            row("bound", () -> "xy".equals(concat.invokeWithArguments("y")));
        }
        if (rows == null || rows.contains("list")) {
            row("list", () -> "ab".equals(cat.invokeWithArguments(pair)));
        }
        if (rows == null || rows.contains("varargs-control")) {
            row("varargs-control", () -> ((Integer) count.invokeWithArguments(1, 2, 3)) == 3);
        }
        if (rows == null || rows.contains("wmte")) {
            try {
                twice.invokeWithArguments(1, 2);
                System.out.println("wmte none");
            } catch (Throwable t) {
                System.out.println("wmte " + t.getClass().getName());
            }
        }
    }
}
