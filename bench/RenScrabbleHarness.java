import java.util.*;
import java.util.stream.*;

/**
 * Direct harness to run the Renaissance Scrabble algorithm
 * without the Renaissance framework, file I/O, or regex.
 * Implements the same Streams-based scoring algorithm.
 */
public class RenScrabbleHarness {
    static final int[] letterScores = {
        1, 3, 3, 2, 1, 4, 2, 4, 1, 8, 5, 1, 3, 1, 1, 3, 10, 1, 1, 1, 1, 4, 4, 8, 4, 10
    };
    static final int[] scrabbleAvailableLetters = {
        9, 2, 2, 1, 12, 2, 3, 2, 9, 1, 1, 4, 2, 6, 8, 2, 1, 6, 4, 6, 4, 2, 2, 1, 2, 1
    };

    static Set<String> scrabbleWords;
    static String[] allWords;

    static {
        scrabbleWords = new HashSet<>();
        String[] sw = {"AA","AB","AD","AE","AG","AH","AI","AL","AM","AN","AR","AS","AT","AW","AX","AY",
            "BA","BE","BI","BO","BY","DA","DE","DO","ED","EF","EH","EL","EM","EN","ER","ES",
            "ET","EX","FA","FE","GO","HA","HE","HI","HM","HO","ID","IF","IN","IS","IT","JO",
            "KA","LA","LI","LO","MA","ME","MI","MM","MO","MU","MY","NA","NE","NO","NU","OD",
            "OE","OF","OH","OI","OM","ON","OP","OR","OS","OW","OX","OY","PA","PE","PI","QI",
            "RE","SH","SI","SO","TA","TI","TO","UH","UM","UN","UP","US","UT","WE","WO","XI",
            "XU","YA","YE","ZA","QUICKLY","ZEPHYRS","QUALIFY","QUICKEN","QUICKER"};
        for (String w : sw) scrabbleWords.add(w);

        allWords = new String[]{
            "THE","AND","TO","OF","I","A","IN","THAT","IS","YOU","MY","IT","FOR","NOT","WITH",
            "HIS","ME","BUT","BE","HE","HAVE","THIS","WILL","YOUR","HER","WAS","ALL","AS",
            "WHAT","DO","SO","ARE","NO","WOULD","IF","SHALL","WHICH","FROM","THEIR","THAN",
            "HAD","HIM","ONE","OUR","ON","THEY","COME","BEEN","OR","WE","BY","WHEN","AN",
            "MORE","THOU","GOOD","THEM","THEN","NOW","COULD","LORD","SIR","KNOW","HOW","WELL",
            "UPON","MOST","MAN","KING","LOVE","MAKE","LIKE","TIME","VERY","HERE","DID","MADE",
            "SHOULD","THINK","MUST","THESE","SUCH","GREAT","YET","MAY","LET","SAY","MUCH",
            "GIVE","TAKE","BEFORE","SOME","LOOK","SEE","HAS","NEVER","OLD","STILL","OWN",
            "TELL","FIRST","GO","LONG","AFTER","HAND","WORD","TOO","MIGHT","JUST","WHERE",
            "DEATH","HEART","SPEAK","QUICKLY","FAIR","GRACE","BLOOD","TRUE","PRAY","WORLD"
        };
    }

    // Renaissance-style Streams algorithm
    @SuppressWarnings("unchecked")
    static List<Map.Entry<Integer, List<String>>> run() {
        // Build histogram function
        // Score each word using streams
        TreeMap<Integer, List<String>> result = (TreeMap<Integer, List<String>>)
            Arrays.stream(allWords)
                .filter(w -> scrabbleWords.contains(w))
                .collect(
                    () -> new TreeMap<Integer, List<String>>(Comparator.reverseOrder()),
                    (map, word) -> {
                        int score = scoreWord(word);
                        if (score > 0) {
                            map.computeIfAbsent(score, k -> new ArrayList<>()).add(word);
                        }
                    },
                    (m1, m2) -> m1.putAll(m2)
                );

        return result.entrySet().stream()
            .limit(3)
            .collect(Collectors.toList());
    }

    static int scoreWord(String word) {
        // Letter frequency
        int[] freq = new int[26];
        for (int i = 0; i < word.length(); i++) {
            char c = word.charAt(i);
            if (c >= 'A' && c <= 'Z') freq[c - 'A']++;
        }
        // Blanks check
        int blanks = 0;
        for (int i = 0; i < 26; i++) {
            if (freq[i] > scrabbleAvailableLetters[i])
                blanks += freq[i] - scrabbleAvailableLetters[i];
        }
        if (blanks > 2) return 0;

        // Base score
        int score = 0;
        for (int i = 0; i < word.length(); i++) {
            char c = word.charAt(i);
            if (c >= 'A' && c <= 'Z') score += letterScores[c - 'A'];
        }
        // Bonus
        int bonus = 0;
        int len = word.length();
        for (int i = 0; i < Math.min(3, len); i++) {
            char c = word.charAt(i);
            if (c >= 'A' && c <= 'Z') bonus = Math.max(bonus, letterScores[c - 'A']);
        }
        for (int i = Math.max(0, len - 3); i < len; i++) {
            char c = word.charAt(i);
            if (c >= 'A' && c <= 'Z') bonus = Math.max(bonus, letterScores[c - 'A']);
        }
        score = (score + bonus) * 2;
        if (len == 7) score += 50;
        return score;
    }

    public static void main(String[] args) {
        System.out.println("=== Renaissance Scrabble (Streams) ===");

        // Warmup
        for (int i = 0; i < 5; i++) run();

        // Benchmark
        int reps = 1000;
        long t0 = System.currentTimeMillis();
        List<Map.Entry<Integer, List<String>>> result = null;
        for (int i = 0; i < reps; i++) {
            result = run();
        }
        long elapsed = System.currentTimeMillis() - t0;

        System.out.println("Time: " + elapsed + " ms (" + reps + " reps)");
        if (result != null) {
            for (Map.Entry<Integer, List<String>> e : result) {
                System.out.println("  " + e.getKey() + " -> " + e.getValue());
            }
        }
    }
}
