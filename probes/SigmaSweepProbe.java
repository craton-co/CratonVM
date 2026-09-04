import java.util.Locale;

/** Isolates which preceding context makes the final-sigma rule fire. */
public class SigmaSweepProbe {
    static void t(String label, String s) {
        StringBuilder sb = new StringBuilder();
        for (int cp : s.toLowerCase(Locale.ROOT).codePoints().toArray()) {
            if (sb.length() > 0) sb.append(',');
            sb.append(cp);
        }
        System.out.println(label + " => " + sb);
    }

    public static void main(String[] a) {
        String S = "Σ";      // GREEK CAPITAL SIGMA
        String s = "σ";      // medial
        t("[S]        ", S);
        t("[A S]      ", "A" + S);
        t("[a S]      ", "a" + S);
        t("[1 S]      ", "1" + S);
        t("[space S]  ", " " + S);
        t("[Alpha S]  ", "Α" + S);
        t("[sigma S]  ", s + S);
        t("[S S]      ", S + S);
        t("[A S A]    ", "A" + S + "A");
        t("[A B S]    ", "AB" + S);
        t("[A S space]", "A" + S + " ");
        t("[Agrave S] ", "À" + S);
        t("[A quote S]", "A'" + S);      // apostrophe is case-ignorable
        System.out.println("RESULT done");
    }
}
