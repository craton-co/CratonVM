package cratonvm;

import java.util.List;
import java.util.ArrayList;

/**
 * Does the compiled form of SWFrames' self-TAIL-recursive `recurse` still push
 * a native frame per activation?
 *
 * Warm the exact shape up so it tiers up, then drive it far deeper than any
 * stack can hold. A VM that keeps one frame per activation must throw
 * StackOverflowError; one that eliminated the tail call returns normally.
 */
public final class SWTce {
    public static void main(String[] args) {
        // Warm up at the same depth SWFrames uses so `recurse` tiers up.
        for (int i = 0; i < 200; i++) {
            recurse(64);
        }
        int deep = 4_000_000;
        String outcome;
        try {
            List<String> r = recurse(deep);
            outcome = "RETURNED n=" + r.size();
        } catch (StackOverflowError e) {
            outcome = "STACK_OVERFLOW";
        }
        System.out.println("depth=" + deep + " -> " + outcome);
    }

    private static List<String> recurse(int d) {
        if (d == 0) {
            return new ArrayList<String>();
        }
        return recurse(d - 1);
    }
}
