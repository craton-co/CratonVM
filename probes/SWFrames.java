package cratonvm;

import java.lang.StackWalker;
import java.util.List;
import java.util.Set;
import java.util.stream.Collectors;

/**
 * StackWalker arm of the "compiled self-recursion is one frame" defect
 * (docs/known-issues/jit/jit-self-recursive-activations-invisible-to-stack-walks-20260818.md).
 *
 * Same shape as StackWalkerLog4jStressProbe but prints the frame list, so the
 * loss is visible as a COUNT rather than as a wrong caller class. Early rounds
 * are interpreted and report ~68 frames; once the recursion tiers up the walk
 * reports 3, and cratonvm.SWFrames$LoggerFactory drops out of it entirely.
 * `--nojit` and HotSpot hold 68 in every round.
 */
public final class SWFrames {
    private static final String OWNER = SWFrames.class.getName();
    private static final int DEPTH = 64;
    private static final int WALKS = 32;
    private static final StackWalker WALKER =
            StackWalker.getInstance(Set.of(StackWalker.Option.RETAIN_CLASS_REFERENCE));

    static final class Locator {
        static List<String> frames() {
            return WALKER.walk(s -> s.map(f -> f.getClassName() + "." + f.getMethodName())
                    .collect(Collectors.toList()));
        }
    }

    static final class LoggerFactory {
        static List<String> resolveCaller() {
            return Locator.frames();
        }
    }

    public static void main(String[] args) {
        for (int i = 0; i < WALKS; i++) {
            List<String> f = recurse(DEPTH);
            boolean hasLF = false;
            for (String s : f) {
                if (s.startsWith(OWNER + "$LoggerFactory")) { hasLF = true; break; }
            }
            if (i == 0 || i == 8 || i == WALKS - 1) {
                System.out.println("walk#" + i + " hasLoggerFactory=" + hasLF
                        + " n=" + f.size() + " frames=" + f.subList(0, Math.min(4, f.size())));
            }
        }
        System.out.println("DONE");
    }

    private static List<String> recurse(int d) {
        if (d == 0) return LoggerFactory.resolveCaller();
        return recurse(d - 1);
    }
}
