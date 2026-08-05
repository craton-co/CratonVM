import java.util.Scanner;
import java.util.regex.MatchResult;
import java.util.regex.Pattern;

/**
 * `java.util.Scanner.match()` and the match-validity protocol, against the host
 * JDK.
 *
 * CratonVM's Scanner natives tokenize with Rust string scanning and never
 * record what they matched, so `match()` — which is not registered, and so runs
 * real JDK bytecode that checks `matchValid` — threw `IllegalStateException`
 * after every operation, where HotSpot returns the last match.
 *
 * Which operations leave a match available is NOT obvious from the javadoc and
 * must not be guessed: `hasNext()` and `useDelimiter()` plausibly either
 * preserve or clear it, and getting that backwards is a silent divergence.
 * This probe is the oracle. Every line must print the same thing under `java`,
 * `cratonvm --real-jdk` and `cratonvm --jdk-only`.
 *
 * The `group(n)` / `start` / `end` / `groupCount` lines matter as much as the
 * text: a `match()` that returns the right string from the wrong offsets, or
 * with the capture groups dropped, is exactly the "looks fixed" result this
 * work item keeps producing.
 */
public final class ScannerMatchStateProbe {

    public static void main(String[] args) {
        beforeAnything();
        afterNext();
        afterNextInt();
        afterNextLine();
        afterFindInLine();
        afterFindWithinHorizon();
        afterSkip();
        afterHasNext();
        afterUseDelimiter();
        afterReset();
        otherLookaheads();
        failedSearch();
        nonAscii();
        crlfLine();
        twiceInARow();
        captureGroups();
        offsets();
        afterClose();
        resultClass();
        System.out.println("ScannerMatchStateProbe done");
    }

    static void beforeAnything() {
        System.out.println("before=" + m(new Scanner("z")));
    }

    static void afterNext() {
        Scanner sc = new Scanner("ab cd");
        sc.next();
        System.out.println("after-next=" + m(sc));
        sc.close();
    }

    static void afterNextInt() {
        Scanner sc = new Scanner("7 8");
        sc.nextInt();
        System.out.println("after-nextInt=" + m(sc));
        sc.close();
    }

    static void afterNextLine() {
        Scanner sc = new Scanner("one two\nsecond");
        sc.nextLine();
        System.out.println("after-nextLine=" + m(sc));
        sc.close();
    }

    static void afterFindInLine() {
        Scanner sc = new Scanner("x 42 y");
        sc.findInLine("\\d+");
        System.out.println("after-findInLine=" + m(sc));
        sc.close();
    }

    static void afterFindWithinHorizon() {
        Scanner sc = new Scanner("aa bb 99 cc");
        sc.findWithinHorizon("\\d+", 0);
        System.out.println("after-findWithinHorizon=" + m(sc));
        sc.close();
    }

    static void afterSkip() {
        Scanner sc = new Scanner("xxxyyy");
        sc.skip(Pattern.compile("x+"));
        System.out.println("after-skip=" + m(sc));
        sc.close();
    }

    /** Does a lookahead clear the previous match? */
    static void afterHasNext() {
        Scanner sc = new Scanner("ab cd");
        sc.next();
        boolean h = sc.hasNext();
        System.out.println("after-next-then-hasNext=" + m(sc) + " (hasNext=" + h + ")");
        sc.close();
    }

    /** Does reconfiguring the delimiter clear it? */
    static void afterUseDelimiter() {
        Scanner sc = new Scanner("ab cd");
        sc.next();
        sc.useDelimiter(",");
        System.out.println("after-next-then-useDelimiter=" + m(sc));
        sc.close();
    }

    static void afterReset() {
        Scanner sc = new Scanner("ab cd");
        sc.next();
        sc.reset();
        System.out.println("after-next-then-reset=" + m(sc));
        sc.close();
    }

    /** Do the other lookaheads clear it too, or only `hasNext()`? */
    static void otherLookaheads() {
        Scanner a = new Scanner("ab cd");
        a.next();
        a.hasNextLine();
        System.out.println("after-next-then-hasNextLine=" + m(a));
        a.close();
        Scanner b = new Scanner("7 8");
        b.nextInt();
        b.hasNextInt();
        System.out.println("after-nextInt-then-hasNextInt=" + m(b));
        b.close();
    }

    /** Does a search that finds nothing clear the previous match? */
    static void failedSearch() {
        Scanner sc = new Scanner("ab cd");
        sc.next();
        String miss = sc.findInLine("zzz");
        System.out.println("after-failed-findInLine=" + m(sc) + " (returned " + miss + ")");
        sc.close();
    }

    /**
     * The offsets are Java char indices, which are not byte offsets: the second
     * token of `éé ab` starts at char 3 and byte 5.
     *
     * Prints offsets and lengths, never the matched text — the text is
     * non-ASCII and the console encoding differs between runtimes, which would
     * make this line differ for a reason that has nothing to do with the VM.
     */
    static void nonAscii() {
        Scanner sc = new Scanner("éé ab");
        sc.next();
        System.out.println("nonascii.first=" + offsets(sc));
        sc.next();
        System.out.println("nonascii.second=" + offsets(sc));
        sc.close();
    }

    /**
     * What CLASS `match()` hands back. HotSpot returns the JDK's immutable
     * snapshot; CratonVM returns it too wherever `Matcher.toMatchResult()`
     * works, and the live `Matcher` — which also implements `MatchResult` —
     * where it does not. Printed rather than hidden: under `--jdk-only`,
     * `toMatchResult()` throws `NoClassDefFoundError:
     * cratonvm/internal/UnmodifiableMap` from plain Java, so this line is the
     * one that differs, and it will stop differing when that is fixed.
     */
    static void resultClass() {
        Scanner sc = new Scanner("ab cd");
        sc.next();
        try {
            System.out.println("result.class=" + sc.match().getClass().getName());
        } catch (RuntimeException e) {
            System.out.println("result.class=" + e.getClass().getName());
        }
        sc.close();
    }

    private static String offsets(Scanner sc) {
        try {
            MatchResult r = sc.match();
            return "len=" + (r.end() - r.start()) + "@" + r.start() + "," + r.end();
        } catch (RuntimeException e) {
            return e.getClass().getName();
        }
    }

    /** How much of a CRLF terminator does nextLine's match cover? */
    static void crlfLine() {
        Scanner sc = new Scanner("a\r\nb");
        sc.nextLine();
        MatchResult r;
        try {
            r = sc.match();
            System.out.println("crlf.len=" + (r.end() - r.start()) + " end=" + r.end());
        } catch (RuntimeException e) {
            System.out.println("crlf=" + e.getClass().getName());
        }
        sc.close();
    }

    /** match() is a read, not a consume: twice must give the same answer. */
    static void twiceInARow() {
        Scanner sc = new Scanner("ab cd");
        sc.next();
        System.out.println("twice-1=" + m(sc));
        System.out.println("twice-2=" + m(sc));
        sc.close();
    }

    /** A search pattern's capture groups have to survive into the result. */
    static void captureGroups() {
        Scanner sc = new Scanner("id=4711;");
        sc.findInLine("id=(\\d+)(;)");
        try {
            MatchResult r = sc.match();
            System.out.println("groups.count=" + r.groupCount());
            System.out.println("groups.0=" + r.group(0));
            System.out.println("groups.1=" + r.group(1));
            System.out.println("groups.2=" + r.group(2));
        } catch (RuntimeException e) {
            System.out.println("groups=" + e.getClass().getName());
        }
        sc.close();
    }

    /** The offsets are into the whole input, not into the matched region. */
    static void offsets() {
        Scanner sc = new Scanner("aaaa bbbb cccc");
        sc.next();
        System.out.println("offsets.first=" + m(sc));
        sc.next();
        System.out.println("offsets.second=" + m(sc));
        sc.close();
    }

    static void afterClose() {
        Scanner sc = new Scanner("ab cd");
        sc.next();
        sc.close();
        System.out.println("after-close=" + m(sc));
    }

    private static String m(Scanner sc) {
        try {
            MatchResult r = sc.match();
            return "[" + r.group() + "]@" + r.start() + "," + r.end();
        } catch (IllegalStateException e) {
            return "IllegalStateException";
        } catch (RuntimeException e) {
            return e.getClass().getName();
        }
    }
}
