// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 tc21b (TYPECHECK21-2 of jit-r21-typecheck-proposals-20261014):
// `Class.isAssignableFrom` / `Class.isInstance` against the targets the VM's
// by-name `synthetic_implements` rules name (List, Set, Collection, Iterable,
// Iterator, Comparable, Serializable, Deque, Queue, Map.Entry, Cloneable), plus
// one control target (Runnable). The compiled arm now answers a MISS inline for
// those targets too when the source's display verdict is 1; every answer must
// stay the class hierarchy's, in particular `BitSet` is not a `Set` and a user
// class named `...$Itr` is not an `Iterator` (HotSpot says false to both).
//
// Expected output is DERIVED from JLS 4.10 / the java.base declarations of
// ArrayList, LinkedList, BitSet, HashMap, String and Integer, NOT measured (no
// JDK 25 HotSpot on the authoring host). Trues per pass over the 7 sources:
//   List 2 (ArrayList, LinkedList)   Set 0            Collection 2   Iterable 2
//   Iterator 0                       Comparable 2 (String, Integer)
//   Serializable 6 (all but Itr)     Deque 1 (LinkedList)   Queue 1
//   Map.Entry 0                      Cloneable 4 (ArrayList, BitSet, HashMap, LinkedList)
//   Runnable 0
// = 20 trues per pass; isAssignableFrom and isInstance give the same table.
// With REPS = 20_000 passes each per-target count is the figure above * REPS.
//
// Run:
//   javac -d out R21Tc21bAssignableSynthetic1.java
//   java -Xmx256m -cp out R21Tc21bAssignableSynthetic1                     # HotSpot 25 (when available)
//   cratonvm -Xmx256m -cp out R21Tc21bAssignableSynthetic1
//   cratonvm --compatible -Xmx256m -cp out R21Tc21bAssignableSynthetic1
//   CRATONVM_JIT_TYPECHECK_DISPLAY_MISS_ANY_TARGET=0 cratonvm -Xmx256m -cp out R21Tc21bAssignableSynthetic1
//   CRATONVM_C2_ACCEPT=never cratonvm -Xmx256m -cp out R21Tc21bAssignableSynthetic1
// PASS: every "fact" line reads OK on every arm in both modes (the facts are
// equal on all arms; only the "time" lines may differ, and the default arm is
// expected faster than the =0 arm on the `time assignable` row).

import java.util.ArrayList;
import java.util.BitSet;
import java.util.Deque;
import java.util.HashMap;
import java.util.Iterator;
import java.util.LinkedList;
import java.util.List;
import java.util.Map;
import java.util.Queue;
import java.util.Set;
import java.util.Collection;

public class R21Tc21bAssignableSynthetic1 {
    static final int REPS = 20_000;

    /** A user class whose simple name a by-name rule would read as a cursor. */
    static final class Itr { }

    static final Class<?>[] TGT = {
        List.class, Set.class, Collection.class, Iterable.class, Iterator.class,
        Comparable.class, java.io.Serializable.class, Deque.class, Queue.class,
        Map.Entry.class, Cloneable.class, Runnable.class,
    };
    static final String[] TGT_NAME = {
        "List", "Set", "Collection", "Iterable", "Iterator", "Comparable",
        "Serializable", "Deque", "Queue", "Map.Entry", "Cloneable", "Runnable",
    };
    static final long[] EXPECT = {2, 0, 2, 2, 0, 2, 6, 1, 1, 0, 4, 0};

    static Object[] objects() {
        return new Object[] {
            new ArrayList<String>(), new BitSet(), new HashMap<String, String>(), "s",
            Integer.valueOf(3), new Itr(), new LinkedList<String>(),
        };
    }

    static void assignable(Class<?>[] src, long[] out, int reps) {
        for (int r = 0; r < reps; r++) {
            for (int s = 0; s < src.length; s++) {
                Class<?> c = src[s];
                for (int t = 0; t < TGT.length; t++) {
                    if (TGT[t].isAssignableFrom(c)) {
                        out[t]++;
                    }
                }
            }
        }
    }

    static void instance(Object[] objs, long[] out, int reps) {
        for (int r = 0; r < reps; r++) {
            for (int s = 0; s < objs.length; s++) {
                Object o = objs[s];
                for (int t = 0; t < TGT.length; t++) {
                    if (TGT[t].isInstance(o)) {
                        out[t]++;
                    }
                }
            }
        }
    }

    static boolean report(String what, long[] got, int reps) {
        boolean ok = true;
        for (int t = 0; t < TGT.length; t++) {
            long want = EXPECT[t] * reps;
            boolean good = got[t] == want;
            ok &= good;
            System.out.println("fact " + what + " " + TGT_NAME[t] + " " + got[t]
                + (good ? " OK" : " FAIL want " + want));
        }
        return ok;
    }

    public static void main(String[] args) {
        Object[] objs = objects();
        Class<?>[] src = new Class<?>[objs.length];
        for (int i = 0; i < objs.length; i++) {
            src[i] = objs[i].getClass();
        }
        // Warm both kernels so the optimizing tier compiles them, then count.
        assignable(src, new long[TGT.length], 3_000);
        instance(objs, new long[TGT.length], 3_000);
        long[] a = new long[TGT.length];
        long[] b = new long[TGT.length];
        long t0 = System.nanoTime();
        assignable(src, a, REPS);
        long t1 = System.nanoTime();
        instance(objs, b, REPS);
        long t2 = System.nanoTime();
        boolean ok = report("assignable", a, REPS);
        ok &= report("instance", b, REPS);
        System.out.println("time assignable ns/check "
            + (t1 - t0) / ((long) REPS * src.length * TGT.length));
        System.out.println("time instance ns/check "
            + (t2 - t1) / ((long) REPS * objs.length * TGT.length));
        System.out.println(ok ? "ALL OK" : "FAILED");
    }
}
