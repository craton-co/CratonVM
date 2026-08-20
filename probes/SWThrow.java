package cratonvm;

/** Does the same tier-up frame loss hit Throwable.getStackTrace(), not just StackWalker? */
public final class SWThrow {
    private static final int DEPTH = 64;
    private static final int ROUNDS = 32;

    public static void main(String[] args) {
        for (int i = 0; i < ROUNDS; i++) {
            int n = depthOfTrace(DEPTH);
            if (i == 0 || i == 8 || i == ROUNDS - 1) {
                System.out.println("round#" + i + " traceFrames=" + n);
            }
        }
        System.out.println("DONE");
    }

    private static int depthOfTrace(int d) {
        try {
            recurse(d);
            return -1;
        } catch (IllegalStateException e) {
            return e.getStackTrace().length;
        }
    }

    private static void recurse(int d) {
        if (d == 0) {
            throw new IllegalStateException("boom");
        }
        recurse(d - 1);
    }
}
