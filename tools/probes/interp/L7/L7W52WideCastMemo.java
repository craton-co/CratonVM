// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 52, lane L7: the three cast-site memos of
// `CRATONVM_CAST_MEMO_WIDE` (proposal i51-L7): a lambda receiver's admission
// (positive receiver memo), an array `instanceof` refused (negative array
// memo), and a plain receiver refused by an array target (negative receiver
// slots). Every shape cold (r0) and warm (r1..r3), so a memo filled in one
// round answers the next; a site that memoised one receiver is then asked
// about another, which the memo must NOT answer:
//
//   lam       a lambda instanceof / cast to its own interface, then a second
//             lambda class (another interface) at the same instanceof site
//   ser       a plain lambda instanceof Serializable (false), a serializable
//             intersection lambda at the same site (true), and the plain
//             one's cast to Serializable (a ClassCastException)
//   marker    an intersection lambda (Runnable & Marker) instanceof Marker,
//             then the plain Runnable lambda at the same site
//   arrneg    instanceof String[] refusing an Integer[], then admitting a
//             String[] at the same site
//   arrprim   instanceof int[] refusing a long[], then admitting an int[]
//   tgtarr    instanceof Object[] refusing a plain object and a lambda,
//             then admitting a String[] at the same site
//   cast      (Runnable) applied to a lambda, then to a Supplier lambda at
//             the same site (a ClassCastException)
//
// The positive control is `CRATONVM_DBG_FIELD_SITE=1`: its FINAL line's
// `cast-wide-fill:` is non-zero with the switch on, 0 with
// `CRATONVM_CAST_MEMO_WIDE=0`; `CRATONVM_DBG_CAST_MEMO_CROSSCHECK=1` must
// print no `[CAST-MEMO-CROSSCHECK]` line.
//
// Expected (HotSpot 25.0.3, `java L7W52WideCastMemo`, and `-Xint`):
//   r0 lam true 1 false
//   r0 ser false true java.lang.ClassCastException
//   r0 marker true false
//   r0 arrneg false true
//   r0 arrprim false true
//   r0 tgtarr false false true
//   r0 cast 1 java.lang.ClassCastException
//   r1 lam true 1 false
//   r1 ser false true java.lang.ClassCastException
//   r1 marker true false
//   r1 arrneg false true
//   r1 arrprim false true
//   r1 tgtarr false false true
//   r1 cast 1 java.lang.ClassCastException
//   r2 lam true 1 false
//   r2 ser false true java.lang.ClassCastException
//   r2 marker true false
//   r2 arrneg false true
//   r2 arrprim false true
//   r2 tgtarr false false true
//   r2 cast 1 java.lang.ClassCastException
//   r3 lam true 1 false
//   r3 ser false true java.lang.ClassCastException
//   r3 marker true false
//   r3 arrneg false true
//   r3 arrprim false true
//   r3 tgtarr false false true
//   r3 cast 1 java.lang.ClassCastException
//   loop 200000 0 200000
import java.io.Serializable;
import java.util.function.Supplier;

public class L7W52WideCastMemo {
    interface Marker {}

    static boolean isRunnable(Object o) {
        return o instanceof Runnable;
    }

    static boolean isSerializable(Object o) {
        return o instanceof Serializable;
    }

    static boolean isMarker(Object o) {
        return o instanceof Marker;
    }

    static boolean isStrings(Object o) {
        return o instanceof String[];
    }

    static boolean isInts(Object o) {
        return o instanceof int[];
    }

    static boolean isObjects(Object o) {
        return o instanceof Object[];
    }

    static int runIt(Object o) {
        Runnable r = (Runnable) o;
        r.run();
        return 1;
    }

    static String toSerializable(Object o) {
        try {
            Serializable s = (Serializable) o;
            return s == null ? "null" : "cast";
        } catch (ClassCastException e) {
            return e.getClass().getName();
        }
    }

    static String runCatching(Object o) {
        try {
            return String.valueOf(runIt(o));
        } catch (ClassCastException e) {
            return e.getClass().getName();
        }
    }

    public static void main(String[] args) {
        Runnable plain = () -> { };
        Supplier<String> other = () -> "x";
        Runnable serial = (Runnable & Serializable) () -> { };
        Runnable marked = (Runnable & Marker) () -> { };
        for (int round = 0; round < 4; round++) {
            String p = "r" + round + " ";
            System.out.println(p + "lam " + isRunnable(plain) + " " + runIt(plain) + " "
                    + isRunnable(other));
            System.out.println(p + "ser " + isSerializable(plain) + " " + isSerializable(serial)
                    + " " + toSerializable(plain));
            System.out.println(p + "marker " + isMarker(marked) + " " + isMarker(plain));
            System.out.println(p + "arrneg " + isStrings(new Integer[1]) + " "
                    + isStrings(new String[1]));
            System.out.println(p + "arrprim " + isInts(new long[1]) + " " + isInts(new int[1]));
            System.out.println(p + "tgtarr " + isObjects(new Object()) + " " + isObjects(plain)
                    + " " + isObjects(new String[1]));
            System.out.println(p + "cast " + runCatching(plain) + " " + runCatching(other));
        }
        // Warm loop: the memoised answers, many times over, at the same sites.
        int yes = 0, no = 0, arr = 0;
        Object[] ints = new Integer[1];
        Object[] strs = new String[1];
        for (int i = 0; i < 200_000; i++) {
            if (isRunnable(plain)) yes++;
            if (isStrings(ints)) no++;
            if (isObjects(new Object())) no++;
            if (isStrings(strs)) arr++;
        }
        System.out.println("loop " + yes + " " + no + " " + arr);
    }
}
