import java.util.Scanner;

/**
 * The cost of a token loop, so the match-recording added for `Scanner.match()`
 * has a measured price rather than an asserted one.
 *
 * Prints a checksum as well as the timing: a run that tokenized nothing would
 * otherwise look fast.
 */
public final class ScannerTokenCostProbe {

    public static void main(String[] args) {
        int tokens = args.length > 0 ? Integer.parseInt(args[0]) : 50_000;
        StringBuilder sb = new StringBuilder(tokens * 6);
        for (int i = 0; i < tokens; i++) {
            sb.append(i % 100000).append(' ');
        }
        String input = sb.toString();

        // Warm up, then measure.
        for (int r = 0; r < 2; r++) {
            long sum = 0;
            int n = 0;
            long t0 = System.nanoTime();
            Scanner sc = new Scanner(input);
            while (sc.hasNext()) {
                sum += sc.next().length();
                n++;
            }
            sc.close();
            long ms = (System.nanoTime() - t0) / 1_000_000;
            System.out.println("run" + r + " tokens=" + n + " sum=" + sum + " ms=" + ms);
        }
    }
}
