import java.text.Collator;

/** Splits Collator.getInstance / setStrength / equals so the hot one is named. */
public class CollatorSplit {
    static Object sink;
    public static void main(String[] args) throws Exception {
        Collator warm = Collator.getInstance();
        warm.setStrength(Collator.PRIMARY);
        warm.equals("SELECT", "select");
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 2000;
        for (int r = 0; r < 2; r++) {
            long s = System.nanoTime();
            for (int i = 0; i < n; i++) { sink = Collator.getInstance(); }
            long a = System.nanoTime();
            for (int i = 0; i < n; i++) { warm.setStrength(Collator.PRIMARY); }
            long b = System.nanoTime();
            for (int i = 0; i < n; i++) { sink = warm.equals("SELECT", "select") ? warm : null; }
            long c = System.nanoTime();
            for (int i = 0; i < n; i++) { sink = "SELECT".substring(0, 6); }
            long d = System.nanoTime();
            System.out.println("n=" + n
                    + " getInstance=" + ((a - s) / 1000000) + "ms"
                    + " setStrength=" + ((b - a) / 1000000) + "ms"
                    + " equals=" + ((c - b) / 1000000) + "ms"
                    + " substring=" + ((d - c) / 1000000) + "ms");
        }
    }
}
