import java.util.NoSuchElementException;
import java.util.Scanner;
import java.util.regex.Pattern;

/**
 * L3 follow-up — the `java.util.Scanner` search methods, against the host JDK.
 *
 * `findWithinHorizon` has natives in `native-builtins/src/phases_early.rs` that
 * are registered in NO configuration: their only registrar,
 * `register_t2_3_completion_natives`, has no caller, and
 * `--dump-native-registry` shows no `findWithinHorizon` row among the 35 live
 * `java/util/Scanner` entries. So the call falls through to real JDK bytecode —
 * which reads `buf`, `matcher` and `source`, none of which our Scanner natives
 * populate.
 *
 * This probe decides what to do about that, rather than assuming. Either the
 * real bytecode copes (in which case the dead natives should be deleted, which
 * is the direction the whole `--jdk-only` feature is going) or it does not (in
 * which case they need registering, and their implementation needs to match
 * what is printed here).
 *
 * `findInLine` and `skip` are in the same family and ARE registered, so they
 * are here as the control: whatever the answer is for `findWithinHorizon`,
 * these two must not move.
 */
public final class L3ScannerSearchProbe {

    public static void main(String[] args) {
        findWithinHorizonString();
        findWithinHorizonPattern();
        findWithinHorizonBounded();
        findInLine();
        skip();
        System.out.println("L3ScannerSearchProbe done");
    }

    static void findWithinHorizonString() {
        Scanner sc = new Scanner("prefix abc123 suffix");
        System.out.println("fwh.s.found=" + call(sc, "\\d+", 0));
        System.out.println("fwh.s.next=" + next(sc));
        sc.close();
    }

    static void findWithinHorizonPattern() {
        Scanner sc = new Scanner("prefix abc123 suffix");
        Pattern p = Pattern.compile("[a-z]+\\d+");
        System.out.println("fwh.p.found=" + call(sc, p, 0));
        System.out.println("fwh.p.next=" + next(sc));
        sc.close();
    }

    /** A horizon shorter than the distance to the match must not match. */
    static void findWithinHorizonBounded() {
        Scanner sc = new Scanner("aaaaaaaaaa42");
        System.out.println("fwh.b.short=" + call(sc, "\\d+", 4));
        System.out.println("fwh.b.long=" + call(sc, "\\d+", 40));
        System.out.println("fwh.b.negative=" + call(sc, "\\d+", -1));
        sc.close();
    }

    static void findInLine() {
        Scanner sc = new Scanner("alpha 77 beta\nsecond");
        System.out.println("fil.found=" + fil(sc, "\\d+"));
        System.out.println("fil.next=" + next(sc));
        System.out.println("fil.miss=" + fil(sc, "zzz"));
        sc.close();
    }

    static void skip() {
        Scanner sc = new Scanner("xxxyyy zzz");
        try {
            sc.skip(Pattern.compile("x+"));
            System.out.println("skip.ok next=" + next(sc));
        } catch (NoSuchElementException e) {
            System.out.println("skip.NoSuchElementException");
        }
        sc.close();
    }

    private static String call(Scanner sc, String regex, int horizon) {
        try {
            return String.valueOf(sc.findWithinHorizon(regex, horizon));
        } catch (IllegalArgumentException e) {
            return "IllegalArgumentException";
        } catch (RuntimeException e) {
            return e.getClass().getName();
        }
    }

    // Deliberately does NOT call `sc.match()`. That is a separate, wider gap:
    // CratonVM's Scanner natives never populate the real `matcher` /
    // `matchValid` state, so `match()` throws `IllegalStateException` after
    // `next()`, `nextInt()` and `findInLine()` alike, where HotSpot 25 returns
    // `ab`, `7` and `42`. Filed rather than folded in here — closing it means
    // running a real `Matcher` on every token path, which is a redesign of
    // these natives, not a fix to this one.
    private static String call(Scanner sc, Pattern p, int horizon) {
        try {
            return String.valueOf(sc.findWithinHorizon(p, horizon));
        } catch (IllegalArgumentException e) {
            return "IllegalArgumentException";
        } catch (RuntimeException e) {
            return e.getClass().getName();
        }
    }

    private static String fil(Scanner sc, String regex) {
        try {
            return String.valueOf(sc.findInLine(regex));
        } catch (RuntimeException e) {
            return e.getClass().getName();
        }
    }

    private static String next(Scanner sc) {
        try {
            return sc.next();
        } catch (NoSuchElementException e) {
            return "NoSuchElementException";
        } catch (RuntimeException e) {
            return e.getClass().getName();
        }
    }
}
