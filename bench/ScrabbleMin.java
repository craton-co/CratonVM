/**
 * Minimal test: does basic int array + loop work?
 */
public class ScrabbleMin {
    static int[] scores = {1, 3, 3, 2, 1, 4, 2, 4, 1, 8, 5, 1, 3, 1, 1, 3, 10, 1, 1, 1, 1, 4, 4, 8, 4, 10};

    static int scoreWord(int[] letters, int len) {
        int score = 0;
        for (int i = 0; i < len; i++) {
            score += scores[letters[i]];
        }
        return score;
    }

    public static void main(String[] args) {
        System.out.println("Start");
        int[] word = new int[]{16, 20, 8, 2, 10}; // Q,U,I,C,K
        int s = scoreWord(word, 5);
        System.out.println("Score: " + s);

        // Benchmark
        long t0 = System.currentTimeMillis();
        long total = 0;
        for (int r = 0; r < 1000000; r++) {
            total += scoreWord(word, 5);
        }
        long elapsed = System.currentTimeMillis() - t0;
        System.out.println("Time: " + elapsed + " ms [" + total + "]");
    }
}
