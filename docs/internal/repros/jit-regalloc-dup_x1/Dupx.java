public class Dupx {
    // Mimics WeakHashMap$ValueSpliterator: per-call advance, state in FIELDS,
    // `current = tab[index++]` => dup_x1 (field post-increment as array index).
    static final class Spl {
        final String[] tab;
        int index;
        String current;
        Spl(String[] t) { tab = t; }
        // like tryAdvance: returns one element per call, advancing FIELDS
        String step() {
            while (current != null || index < tab.length) {
                if (current == null) {
                    current = tab[index++];   // dup_x1 idiom on FIELD index
                } else {
                    String v = current;
                    current = null;           // single-element "bucket"
                    if (v != null) return v;
                }
            }
            return null;
        }
    }

    public static void main(String[] a) {
        String[] t = new String[16];
        t[2] = "a"; t[5] = "b"; t[9] = "c"; t[13] = "d"; t[15] = "e";
        long total = 0;
        for (int r = 0; r < 3_000_000; r++) {
            Spl s = new Spl(t);
            String v; int guard = 0;
            while ((v = s.step()) != null) {
                total++;
                if (++guard > 1000) { System.out.println("HANG at r=" + r + " index=" + s.index); return; }
            }
        }
        System.out.println("total=" + total + " (expected " + (3_000_000L * 5) + ")");
    }
}
