// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 101, lane L2 (hunt follow-up; page
// i101-L2-pc-unknown-handler-search-matches-typed-rows-outside-their-range):
// a COMPILED method with two try ranges catching the SAME exception class
// with different handlers, plus a third site outside both, where the
// exception comes from a callee the compiled body reaches through a helper,
// an intrinsic or a native rather than a plain bytecode call:
//   * System.arraycopy with a bad length (native / intrinsic):
//     ArrayIndexOutOfBoundsException;
//   * Math.addExact overflow (intrinsic): ArithmeticException;
//   * String.charAt past the end (intrinsic): StringIndexOutOfBoundsException;
//   * Integer.parseInt of a bad string (JDK bytecode): NumberFormatException.
// Each method returns 1 from the first handler, 2 from the second, and lets a
// throw at the third site escape. If the compiled drain cannot name the throw
// pc and falls back to the pc-unknown search (a typed row matched on class
// alone), the second-range throw returns 1 and the outside throw returns 1
// (or 2) instead of escaping. Each case runs hot (40 000 non-throwing calls)
// before the three throwing calls.
//
// Run, each mode: default, CRATONVM_BG_COMPILE=0, --nojit. Also
// CRATONVM_DBG_JITC=1: a "pc-unknown typed match" line names every search
// that took a typed row without a throw pc (wave 101 diagnostic).
//
// HOTSPOT_EXPECTED_BEGIN
// arraycopy: first=1 second=2 outside=escaped java.lang.ArrayIndexOutOfBoundsException
// addExact: first=1 second=2 outside=escaped java.lang.ArithmeticException
// charAt: first=1 second=2 outside=escaped java.lang.StringIndexOutOfBoundsException
// parseInt: first=1 second=2 outside=escaped java.lang.NumberFormatException
// HOTSPOT_EXPECTED_END

public class L2W101UnknownPcTwoHandlers {
    static final int[] SRC = new int[8];
    static final int[] DST = new int[8];

    // which: 0 = no throw, 1 = throw in range 1, 2 = in range 2, 3 = outside.
    static int arraycopy(int which) {
        try {
            System.arraycopy(SRC, 0, DST, 0, which == 1 ? 99 : 4);
        } catch (ArrayIndexOutOfBoundsException e) {
            return 1;
        }
        int r = DST[0];
        try {
            System.arraycopy(SRC, 0, DST, 0, which == 2 ? 99 : 4);
        } catch (ArrayIndexOutOfBoundsException e) {
            return 2;
        }
        System.arraycopy(SRC, 0, DST, 0, which == 3 ? 99 : 4);
        return r;
    }

    static int addExact(int which) {
        int a = 0;
        try {
            a = Math.addExact(which == 1 ? Integer.MAX_VALUE : 1, 1);
        } catch (ArithmeticException e) {
            return 1;
        }
        try {
            a += Math.addExact(which == 2 ? Integer.MAX_VALUE : 1, 1);
        } catch (ArithmeticException e) {
            return 2;
        }
        a += Math.addExact(which == 3 ? Integer.MAX_VALUE : 1, 1);
        return a & 0;
    }

    static int charAt(int which, String s) {
        int c = 0;
        try {
            c = s.charAt(which == 1 ? 50 : 0);
        } catch (StringIndexOutOfBoundsException e) {
            return 1;
        }
        try {
            c += s.charAt(which == 2 ? 50 : 1);
        } catch (StringIndexOutOfBoundsException e) {
            return 2;
        }
        c += s.charAt(which == 3 ? 50 : 2);
        return c & 0;
    }

    static int parseInt(int which) {
        int v = 0;
        try {
            v = Integer.parseInt(which == 1 ? "x" : "1");
        } catch (NumberFormatException e) {
            return 1;
        }
        try {
            v += Integer.parseInt(which == 2 ? "y" : "2");
        } catch (NumberFormatException e) {
            return 2;
        }
        v += Integer.parseInt(which == 3 ? "z" : "3");
        return v & 0;
    }

    interface Case {
        int run(int which);
    }

    static void report(String name, Case c) {
        long sink = 0;
        for (int i = 0; i < 40_000; i++) {
            sink += c.run(0);
        }
        String outside;
        try {
            outside = "returned " + c.run(3);
        } catch (RuntimeException e) {
            outside = "escaped " + e.getClass().getName();
        }
        System.out.println(name + ": first=" + c.run(1) + " second=" + c.run(2)
                + " outside=" + outside + (sink == 42 ? " !" : ""));
    }

    public static void main(String[] args) {
        report("arraycopy", L2W101UnknownPcTwoHandlers::arraycopy);
        report("addExact", L2W101UnknownPcTwoHandlers::addExact);
        report("charAt", w -> charAt(w, "abc"));
        report("parseInt", L2W101UnknownPcTwoHandlers::parseInt);
    }
}
