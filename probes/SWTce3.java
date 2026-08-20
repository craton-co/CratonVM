package cratonvm;

/**
 * Isolates the gate on the compiled self-tail-call elimination.
 *
 * Five shapes, each warmed at depth 64 and then driven 4,000,000 deep. They
 * differ in exactly one respect at a time:
 *
 *   tailRef    (I)Ljava/lang/Object;  areturn        single-pass, in-window
 *   tailLong   (I)J                   lreturn        single-pass, in-window
 *   tailVoid   (I)V                   return (0xb1)  single-pass, OUT of window
 *   tailRefTry (I)Ljava/lang/Object;  areturn in try single-pass, protected pc
 *   tailInt    (I)I                   ireturn        promoted to the IR tier
 *
 * A shape whose frames are eliminated RETURNS; one that keeps a frame per
 * activation throws StackOverflowError, as --nojit and HotSpot both do for all
 * five.
 */
public final class SWTce3 {
    private static final int DEEP = 4_000_000;

    public static void main(String[] args) {
        for (int i = 0; i < 200; i++) {
            tailRef(64);
            tailLong(64);
            tailVoid(64);
            tailRefTry(64);
            tailInt(64);
        }
        System.out.println("depth=" + DEEP);
        System.out.println("  tailRef    areturn            -> " + run(0));
        System.out.println("  tailLong   lreturn            -> " + run(1));
        System.out.println("  tailVoid   return             -> " + run(2));
        System.out.println("  tailRefTry areturn inside try -> " + run(3));
        System.out.println("  tailInt    ireturn, (I)I      -> " + run(4));
    }

    static String run(int which) {
        try {
            switch (which) {
                case 0: tailRef(DEEP); return "RETURNED";
                case 1: tailLong(DEEP); return "RETURNED";
                case 2: tailVoid(DEEP); return "RETURNED";
                case 3: tailRefTry(DEEP); return "RETURNED";
                default: tailInt(DEEP); return "RETURNED";
            }
        } catch (StackOverflowError e) {
            return "STACK_OVERFLOW";
        }
    }

    static Object tailRef(int d) {
        if (d == 0) {
            return null;
        }
        return tailRef(d - 1);
    }

    static long tailLong(int d) {
        if (d == 0) {
            return 0L;
        }
        return tailLong(d - 1);
    }

    static void tailVoid(int d) {
        if (d == 0) {
            return;
        }
        tailVoid(d - 1);
    }

    static Object tailRefTry(int d) {
        try {
            if (d == 0) {
                return null;
            }
            return tailRefTry(d - 1);
        } catch (ArithmeticException e) {
            return null;
        }
    }

    static int tailInt(int d) {
        if (d == 0) {
            return 0;
        }
        return tailInt(d - 1);
    }
}
