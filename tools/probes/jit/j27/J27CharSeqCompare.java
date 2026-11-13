// j27 wave 2 (lane NATMISC): CharSequence.compare answers the JDK body's
// UTF-16 unit difference (else the length difference) for any CharSequence.
// Page: docs/known-issues/jit/j27-charsequence-compare-native-answers-sign-in-utf8-order-20261028.md
public class J27CharSeqCompare {
    static int hot(CharSequence a, CharSequence b) {
        return CharSequence.compare(a, b);
    }

    public static void main(String[] args) {
        System.out.println("fact cmp-a-c " + CharSequence.compare("a", "c"));
        System.out.println("fact cmp-surrogate-vs-ffff "
                + CharSequence.compare("😀", "￿"));
        System.out.println("fact cmp-sb-vs-string "
                + CharSequence.compare(new StringBuilder("x"), "x"));
        System.out.println("fact cmp-length " + CharSequence.compare("ab", "abcd"));
        System.out.println("fact cmp-sb-sb "
                + CharSequence.compare(new StringBuilder("abc"), new StringBuilder("abd")));
        CharSequence[] l = {"a", "ab", new StringBuilder("x")};
        CharSequence[] r = {"c", "abcd", "x"};
        long sum = 0;
        for (int i = 0; i < 300_000; i++) {
            sum += hot(l[i % 3], r[i % 3]);
        }
        System.out.println("fact cmp-hot-sum " + sum);
    }
}
