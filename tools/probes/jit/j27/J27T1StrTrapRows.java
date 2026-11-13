// JIT round j27 wave 7, lane T1EA: String accessor raises from warm compiled code (r18 backlog rows
// TP16-5 / TP16-13 / TP16-16, page docs/feature-designs/jit-ir-optimizations.md).
// Recreates the rows of the lost R21Strintr2Rows probe (C:\craton\jr21-ssh\probes is gone):
// oob-half, oob-rare, npe-half, charat-hot (control), each over a LATIN1 and a UTF16 receiver.
//
// Each row runs ROUNDS passes of N calls of a small method whose `charAt` is caught in the same
// method; facts are from the last pass (every pass computes the same numbers). `time` lines only
// with -Dj27.time=true (ns per call, best pass).
//
// Build WITHOUT -g (javac's default), so a helpful NPE names `<parameter1>`:
//   javac -d tools/probes/out tools/probes/jit/j27/J27T1StrTrapRows.java
// Run: java -cp tools/probes/out J27T1StrTrapRows        (HotSpot 25)
//      cratonvm [--compatible] -cp tools/probes/out J27T1StrTrapRows
// Arms (facts identical in each): CRATONVM_JIT_IR_STRING_ACCESS_TRAP_WITHDRAW=0,
//   CRATONVM_JIT_IR_STRING_ACCESSOR_CALL_PREFIX=0, CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES=1, --nojit.
//
// Expected HotSpot 25 output (JDK 25 String.charAt -> StringLatin1/StringUTF16.charAt ->
// String.checkIndex -> Preconditions.checkIndex(SIOOBE_FORMATTER): "Index %d out of bounds for
// length %d", the length in chars):
//   fact oob-half-l1 970000
//   fact oob-half-l1-msg java.lang.StringIndexOutOfBoundsException: Index 5 out of bounds for length 3
//   fact oob-half-l1-caller java.lang.String.charAt
//   fact oob-half-u16 83630000
//   fact oob-half-u16-msg java.lang.StringIndexOutOfBoundsException: Index 5 out of bounds for length 3
//   fact oob-half-u16-caller java.lang.String.charAt
//   fact oob-rare-l1 1955200
//   fact oob-rare-l1-msg java.lang.StringIndexOutOfBoundsException: Index 7 out of bounds for length 3
//   fact npe-half-l1 970000
//   fact npe-half-l1-msg java.lang.NullPointerException: null
//   fact npe-half-l1-top none
// (j27 wave 9, lane W9EXC: HotSpot's default `java` answers C2's preallocated stackless NPE here --
// OmitStackTraceInFastThrow. `java -XX:-OmitStackTraceInFastThrow` and `java -Xint` answer instead
//   fact npe-half-l1-msg java.lang.NullPointerException: Cannot invoke "String.charAt(int)" because "<parameter1>" is null
//   fact npe-half-l1-top J27T1StrTrapRows.charAtOrNpe
// CratonVM's fast throw is opt-in (owner's decision): its default equals `java -XX:-OmitStackTraceInFastThrow`;
// `cratonvm -XX:+OmitStackTraceInFastThrow` or `CRATONVM_JIT=omit-stack-trace-in-fast-throw` equals default `java`.
// A fact battery should run HotSpot with -XX:-OmitStackTraceInFastThrow for this probe.)
//   fact charat-hot-l1 1960098
//   fact charat-hot-u16 57069520

public class J27T1StrTrapRows {
    static final int ROUNDS = 6;
    static final String L1 = "abc";
    static final String U16 = "a\u20acc";

    static String lastMsg;
    static String lastCaller;
    static String lastTop;

    static void note(RuntimeException e) {
        lastMsg = e.getClass().getName() + ": " + e.getMessage();
        StackTraceElement[] st = e.getStackTrace();
        lastTop = st.length == 0 ? "none" : st[0].getClassName() + "." + st[0].getMethodName();
        lastCaller = "none";
        for (int k = 1; k < st.length; k++) {
            if (st[k].getClassName().equals("J27T1StrTrapRows")) {
                lastCaller = st[k - 1].getClassName() + "." + st[k - 1].getMethodName();
                break;
            }
        }
    }

    static int charAtOrMinus(String s, int i) {
        try {
            return s.charAt(i);
        } catch (StringIndexOutOfBoundsException e) {
            note(e);
            return -1;
        }
    }

    static int charAtOrNpe(String s, int i) {
        try {
            return s.charAt(i);
        } catch (NullPointerException e) {
            note(e);
            return -2;
        }
    }

    static int charAtHot(String s, int i) {
        return s.charAt(i);
    }

    interface Pass {
        long run();
    }

    // One in-range call (index 1) and one out-of-range call (index 5) alternating.
    static long oobHalf(String s) {
        long sum = 0;
        for (int k = 0; k < 20_000; k++) {
            sum += charAtOrMinus(s, (k & 1) == 0 ? 1 : 5);
        }
        return sum;
    }

    // Out of range (index 7) on one call in 64, else index 0.
    static long oobRare(String s) {
        long sum = 0;
        for (int k = 0; k < 20_480; k++) {
            sum += charAtOrMinus(s, (k & 63) == 63 ? 7 : 0);
        }
        return sum;
    }

    // A null receiver on every other call; index 2.
    static long npeHalf(String s) {
        long sum = 0;
        for (int k = 0; k < 20_000; k++) {
            sum += charAtOrNpe((k & 1) == 0 ? s : null, 2);
        }
        return sum;
    }

    static long charatHot(String s) {
        long sum = 0;
        for (int k = 0; k < 20_001; k++) {
            sum += charAtHot(s, k % 3);
        }
        return sum;
    }

    static final boolean TIME = Boolean.getBoolean("j27.time");

    static void row(String name, int calls, Pass p, boolean msg, boolean caller, boolean top) {
        long v = 0;
        long best = Long.MAX_VALUE;
        for (int r = 0; r < ROUNDS; r++) {
            lastMsg = lastCaller = lastTop = "none";
            long t0 = System.nanoTime();
            v = p.run();
            best = Math.min(best, System.nanoTime() - t0);
        }
        System.out.println("fact " + name + " " + v);
        if (msg) {
            System.out.println("fact " + name + "-msg " + lastMsg);
        }
        if (caller) {
            System.out.println("fact " + name + "-caller " + lastCaller);
        }
        if (top) {
            System.out.println("fact " + name + "-top " + lastTop);
        }
        if (TIME) {
            System.out.println("time " + name + " " + (best / calls) + " ns/call");
        }
    }

    public static void main(String[] args) {
        row("oob-half-l1", 20_000, () -> oobHalf(L1), true, true, false);
        row("oob-half-u16", 20_000, () -> oobHalf(U16), true, true, false);
        row("oob-rare-l1", 20_480, () -> oobRare(L1), true, false, false);
        row("npe-half-l1", 20_000, () -> npeHalf(L1), true, false, true);
        row("charat-hot-l1", 20_001, () -> charatHot(L1), false, false, false);
        row("charat-hot-u16", 20_001, () -> charatHot(U16), false, false, false);
    }
}
