/**
 * The h2-bnf canary: does the forced-native `String` fast path actually EXECUTE?
 *
 * `docs/known-issues/jdk-only/forced-native-string-policy-two-lists-that-disagree.md`
 * records a landed, root-caused, measured performance fix for
 * `substring(I)`/`charAt`/`length`/`isEmpty`/`startsWith` that was **statically
 * unreachable for months** — a whitelist above it excluded the very shapes the
 * block below named. Nothing reported it, because reading the code shows a
 * correct-looking block and reading the test shows a passing test.
 *
 * So this probe does not read anything. It drives the exact loop shape the fix
 * was written for (`org.h2.bnf.RuleFixed` / `RuleElement` / `Bnf` character-by-
 * character grammar scanning) hard enough that the call sites are unambiguously
 * WARM, and then the run is measured from the OUTSIDE with
 * `--dump-native-registry`: `invocations > 0` on the
 * `java/lang/String.charAt(I)C` row is the proof, and it is a proof the source
 * cannot give.
 *
 * It also prints the loop's result, because a fast path that fires and computes
 * the wrong answer is worse than one that never fires.
 */
public class StringForcedNativeCanaryProbe {

    /** The grammar-ish alphabet an H2 BNF rule scanner walks. */
    static final String[] TOKENS = {
        "SELECT", "FROM", "WHERE", "GROUP", "BY", "HAVING", "ORDER",
        "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE", "CREATE",
        "TABLE", "INDEX", "ALTER", "DROP", "JOIN", "LEFT", "RIGHT", "INNER",
    };

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 2000;

        long checksum = 0;
        int matched = 0;
        int consumed = 0;

        for (int iter = 0; iter < iterations; iter++) {
            String sentence = buildSentence(iter);
            // `RuleFixed.autoComplete`-shaped scan: peel one code unit at a
            // time off the head of the query remainder, testing each candidate
            // token as a prefix. Every one of the five shapes the h2-bnf fix
            // names is on this loop.
            String s = sentence;
            while (!s.isEmpty()) {
                char c = s.charAt(0);
                checksum = checksum * 31 + c;
                String up = s;
                for (String name : TOKENS) {
                    if (up.startsWith(name)) {
                        matched++;
                        // A real BNF rule consumes the matched token.
                        s = s.substring(name.length() - 1);
                        break;
                    }
                }
                s = s.substring(1);
                consumed++;
                checksum += s.length();
            }
        }

        System.out.println("CANARY iterations=" + iterations
                + " consumed=" + consumed
                + " matched=" + matched
                + " checksum=" + checksum);
    }

    /** Deterministic, so two runs of this probe are byte-comparable. */
    static String buildSentence(int seed) {
        StringBuilder sb = new StringBuilder();
        int x = seed * 2654435761L > 0 ? (int) ((seed * 2654435761L) & 0x7fffffff) : seed;
        for (int i = 0; i < 6; i++) {
            sb.append(TOKENS[(x + i * 7) % TOKENS.length]);
            sb.append(' ');
            sb.append((char) ('a' + ((x + i) % 26)));
            sb.append((char) ('0' + ((x + i) % 10)));
            sb.append(' ');
        }
        return sb.toString();
    }
}
