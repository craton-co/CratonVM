/**
 * Full Scrabble benchmark — uses int[][] for word data, no String operations.
 */
public class ScrabbleFull {
    static final int[] letterScores = {
        1, 3, 3, 2, 1, 4, 2, 4, 1, 8, 5, 1, 3, 1, 1, 3, 10, 1, 1, 1, 1, 4, 4, 8, 4, 10
    };
    static final int[] available = {
        9, 2, 2, 1, 12, 2, 3, 2, 9, 1, 1, 4, 2, 6, 8, 2, 1, 6, 4, 6, 4, 2, 2, 1, 2, 1
    };

    static int scoreWord(int[] letters, int len) {
        int[] freq = new int[26];
        for (int i = 0; i < len; i++) freq[letters[i]]++;

        int blanks = 0;
        for (int i = 0; i < 26; i++) {
            if (freq[i] > available[i]) blanks += freq[i] - available[i];
        }
        if (blanks > 2) return 0;

        int score = 0;
        for (int i = 0; i < len; i++) score += letterScores[letters[i]];

        int bonus = 0;
        int end = len < 3 ? len : 3;
        for (int i = 0; i < end; i++) {
            int s = letterScores[letters[i]];
            if (s > bonus) bonus = s;
        }
        int start = len - 3;
        if (start < 0) start = 0;
        for (int i = start; i < len; i++) {
            int s = letterScores[letters[i]];
            if (s > bonus) bonus = s;
        }
        score = (score + bonus) * 2;
        if (len == 7) score += 50;
        return score;
    }

    public static void main(String[] args) {
        System.out.println("Generating words...");
        int numWords = 300;
        int[][] words = new int[numWords][];
        int[] wordLens = new int[numWords];
        for (int w = 0; w < numWords; w++) {
            int len = 3 + (w % 8);
            words[w] = new int[len];
            wordLens[w] = len;
            for (int i = 0; i < len; i++) {
                words[w][i] = (w * 7 + i * 13) % 26;
            }
        }
        System.out.println("Warming up...");

        // Warmup
        for (int r = 0; r < 500; r++) {
            long s = 0;
            for (int w = 0; w < numWords; w++) {
                s += scoreWord(words[w], wordLens[w]);
            }
        }

        System.out.println("Benchmarking...");
        int reps = 10000;
        long t0 = System.currentTimeMillis();
        long totalScore = 0;
        for (int r = 0; r < reps; r++) {
            for (int w = 0; w < numWords; w++) {
                totalScore += scoreWord(words[w], wordLens[w]);
            }
        }
        long elapsed = System.currentTimeMillis() - t0;
        System.out.println("=== Scrabble Full (" + reps + "x" + numWords + " words) ===");
        System.out.println("Time: " + elapsed + " ms  [" + totalScore + "]");
    }
}
