import java.util.NoSuchElementException;
import java.util.Scanner;

/**
 * L3 (jdk-only wave 2) — behavioural oracle for the `java.util.Scanner` rows of
 * the fabricated-layout census.
 *
 * The census only reports a native write whose value KIND disagrees with the
 * real field's declared descriptor, so it saw two of the five slots CratonVM's
 * Scanner natives were writing onto the real layout: model slot 3 (`radix`)
 * landing on `delimPattern` and model slot 4 (`closed`) landing on
 * `hasNextPattern`, both `Int` over `L`. Those two are LOSSY — an `Int` written
 * to a reference slot is coerced to null — so the state was not merely
 * misplaced, it was discarded.
 *
 * That is what this probe measures, and it does so through public API only, so
 * the host JDK is the oracle: every line below must print the same thing under
 * `java`, under `cratonvm --real-jdk` and under `cratonvm --jdk-only`.
 *
 * Deliberately NOT a census re-run. A census row going 1 -> 0 says a write
 * stopped happening; only a behavioural diff against the host JDK says the
 * right thing happens instead. `useRadix`/`radix` is the pair that was broken:
 * before the fix CratonVM printed `radix=10` where the JDK prints `radix=16`.
 */
public final class L3ScannerLayoutProbe {

    public static void main(String[] args) {
        radix();
        delimiter();
        delimiterIsUsable();
        position();
        closed();
        System.out.println("L3ScannerLayoutProbe done");
    }

    /** Model slot 3 -> real `delimPattern`. The `Int` was coerced to null. */
    static void radix() {
        Scanner sc = new Scanner("ff 10 7");
        System.out.println("radix.default=" + sc.radix());
        sc.useRadix(16);
        System.out.println("radix.after-useRadix=" + sc.radix());
        System.out.println("radix.nextInt=" + sc.nextInt());
        sc.useRadix(10);
        System.out.println("radix.back-to-10=" + sc.radix());
        System.out.println("radix.nextInt=" + sc.nextInt());
        sc.close();
    }

    /** Model slot 2 -> real `matcher`. Same-kind, so the census never saw it. */
    static void delimiter() {
        Scanner sc = new Scanner("a,b,c");
        System.out.println("delim.default=" + sc.delimiter().pattern());
        sc.useDelimiter(",");
        System.out.println("delim.after-use=" + sc.delimiter().pattern());
        StringBuilder sb = new StringBuilder();
        while (sc.hasNext()) {
            sb.append(sc.next()).append('|');
        }
        System.out.println("delim.tokens=" + sb);
        sc.close();
    }

    /**
     * The `Pattern` `delimiter()` returns has to be a real one. Both delimiter
     * sites used to fabricate it — two field pokes, no `compile()` — which
     * satisfies our own readers (they want the source string) and no census
     * (the two slots it writes are the real class's first two). It is still
     * unusable: `matcher(...).find()` threw `ArrayIndexOutOfBoundsException`
     * inside `Matcher.search`, because everything a real `compile()` fills in
     * was left zeroed.
     */
    static void delimiterIsUsable() {
        Scanner sc = new Scanner("a,b,c");
        sc.useDelimiter(",");
        java.util.regex.Pattern p = sc.delimiter();
        System.out.println("usable.find=" + p.matcher("x,y").find());
        System.out.println("usable.split=" + String.join("|", p.split("1,2,3")));
        System.out.println("usable.matches=" + p.matcher(",").matches());
        System.out.println("usable.default-compiles="
                + new Scanner("q").delimiter().matcher(" ").matches());
        sc.close();
    }

    /** Model slot 1 -> real `position`. Right field by coincidence. */
    static void position() {
        Scanner sc = new Scanner("10 20 hello\nsecond line");
        System.out.println("pos.i1=" + sc.nextInt());
        System.out.println("pos.i2=" + sc.nextInt());
        System.out.println("pos.word=" + sc.next());
        System.out.println("pos.line=[" + sc.nextLine() + "]");
        System.out.println("pos.line2=[" + sc.nextLine() + "]");
        System.out.println("pos.hasNext=" + sc.hasNext());
        sc.close();
    }

    /**
     * Model slot 4 -> real `closed`. Nothing in CratonVM read the flag back, so
     * a closed Scanner kept answering; the JDK's `ensureOpen()` raises
     * `IllegalStateException`.
     */
    static void closed() {
        Scanner sc = new Scanner("one two");
        System.out.println("closed.first=" + sc.next());
        sc.close();
        System.out.println("closed.after-close=" + call(sc));
        // toString() is one of the few methods that does NOT call ensureOpen().
        System.out.println("closed.toString-works=" + (sc.toString() != null));
        // close() is idempotent.
        sc.close();
        System.out.println("closed.double-close=ok");
    }

    private static String call(Scanner sc) {
        try {
            return "returned:" + sc.next();
        } catch (IllegalStateException e) {
            return "IllegalStateException";
        } catch (NoSuchElementException e) {
            return "NoSuchElementException";
        }
    }
}
