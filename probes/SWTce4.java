package cratonvm;

import java.util.List;
import java.util.ArrayList;

/**
 * Is `SWTce`'s permanent frame elimination really permanent, or does that
 * process simply end before the C1-to-C2 supersede lands?
 *
 * Identical to `SWTce` except for the pause between the warm-up and the deep
 * drive. `pauseMs` is argv[0]; 0 reproduces `SWTce` exactly.
 *
 *   RETURNED       -> the C1 body with the tail-JMP is still the live one
 *   STACK_OVERFLOW -> an optimizing body superseded it during the pause
 */
public final class SWTce4 {
    public static void main(String[] args) throws Exception {
        long pauseMs = args.length > 0 ? Long.parseLong(args[0]) : 0L;
        for (int i = 0; i < 200; i++) {
            recurse(64);
        }
        if (pauseMs > 0) {
            Thread.sleep(pauseMs);
        }
        int deep = 4_000_000;
        String outcome;
        try {
            List<String> r = recurse(deep);
            outcome = "RETURNED n=" + r.size();
        } catch (StackOverflowError e) {
            outcome = "STACK_OVERFLOW";
        }
        System.out.println("pauseMs=" + pauseMs + " depth=" + deep + " -> " + outcome);
    }

    private static List<String> recurse(int d) {
        if (d == 0) {
            return new ArrayList<String>();
        }
        return recurse(d - 1);
    }
}
