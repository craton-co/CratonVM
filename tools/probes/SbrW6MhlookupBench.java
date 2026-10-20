// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandleInfo;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodHandles.Lookup;
import java.lang.invoke.MethodType;
import java.util.List;

/**
 * SBR wave 6 lane mhlookup: per-op cost of the rows the lane retires, in
 * microseconds. Usage: {@code SbrW6MhlookupBench [rows] [iters]} where rows is a
 * comma list of the labels below (default all).
 */
public class SbrW6MhlookupBench {
    interface Op {
        Object run() throws Throwable;
    }

    static volatile Object sink;

    static void time(String label, int iters, Op op) throws Throwable {
        for (int i = 0; i < Math.max(200, iters / 5); i++) {
            sink = op.run();
        }
        long best = Long.MAX_VALUE;
        for (int round = 0; round < 5; round++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < iters; i++) {
                sink = op.run();
            }
            long dt = System.nanoTime() - t0;
            best = Math.min(best, dt);
        }
        System.out.printf("%-28s %9.3f us/op%n", label, best / 1000.0 / iters);
    }

    static int inc(int x) { return x + 1; }
    public String hello(String s) { return s; }

    static class Nested {}

    public static void main(String[] args) throws Throwable {
        String only = args.length > 0 ? "," + args[0] + "," : null;
        int iters = args.length > 1 ? Integer.parseInt(args[1]) : 20000;
        Lookup L = MethodHandles.lookup();
        Class<?>[] pa = {String.class, long.class};
        MethodHandle mh = L.findStatic(SbrW6MhlookupBench.class, "inc", MethodType.methodType(int.class, int.class));
        MethodHandle mhv = L.findVirtual(SbrW6MhlookupBench.class, "hello", MethodType.methodType(String.class, String.class));
        MethodType mt3 = MethodType.methodType(int.class, String.class, long.class);
        String[] labels = {"mt1", "mt2", "mt3", "mtarr", "mtptype", "mtpcount", "mtptypes", "lookup", "publicLookup",
                "privateLookupIn", "in", "in-self", "ensureInit", "lookupClass", "lookupModes", "lkToString", "findClass",
                "mhtype", "mhtype-bound", "revealS", "revealV", "findStatic", "findVirtual"};
        Op[] ops = {
                () -> MethodType.methodType(int.class),
                () -> MethodType.methodType(int.class, String.class),
                () -> MethodType.methodType(int.class, String.class, long.class),
                () -> MethodType.methodType(int.class, pa),
                () -> mt3.parameterType(1),
                () -> mt3.parameterCount(),
                () -> mt3.parameterArray(),
                () -> MethodHandles.lookup(),
                () -> MethodHandles.publicLookup(),
                () -> MethodHandles.privateLookupIn(SbrW6MhlookupBench.class, L),
                () -> L.in(Nested.class),
                () -> L.in(SbrW6MhlookupBench.class),
                () -> L.ensureInitialized(SbrW6MhlookupBench.class),
                () -> L.lookupClass(),
                () -> L.lookupModes(),
                () -> L.toString(),
                () -> L.findClass("java.lang.String"),
                () -> mh.type(),
                () -> mhv.type(),
                () -> L.revealDirect(mh).getName(),
                () -> L.revealDirect(mhv).getReferenceKind(),
                () -> L.findStatic(SbrW6MhlookupBench.class, "inc", MethodType.methodType(int.class, int.class)),
                () -> L.findVirtual(SbrW6MhlookupBench.class, "hello", MethodType.methodType(String.class, String.class)),
        };
        for (int i = 0; i < labels.length; i++) {
            if (only != null && !only.equals(",all,") && !only.contains("," + labels[i] + ",")) {
                continue;
            }
            try {
                time(labels[i], iters, ops[i]);
            } catch (Throwable t) {
                System.out.println(labels[i] + " !" + t.getClass().getName());
            }
        }
    }
}
