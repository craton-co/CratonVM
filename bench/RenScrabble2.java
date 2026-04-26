import java.util.*;

/**
 * Scrabble benchmark using HashMap/TreeMap/ArrayList but no Streams.
 * Tests collections + autoboxing + Comparator.
 */
public class RenScrabble2 {
    static final int[] letterScores = {
        1, 3, 3, 2, 1, 4, 2, 4, 1, 8, 5, 1, 3, 1, 1, 3, 10, 1, 1, 1, 1, 4, 4, 8, 4, 10
    };
    static final int[] scrabbleAvailableLetters = {
        9, 2, 2, 1, 12, 2, 3, 2, 9, 1, 1, 4, 2, 6, 8, 2, 1, 6, 4, 6, 4, 2, 2, 1, 2, 1
    };

    static HashSet<String> scrabbleWords;
    static String[] allWords;

    static int scoreWord(String word) {
        int[] freq = new int[26];
        for (int i = 0; i < word.length(); i++) {
            char c = word.charAt(i);
            if (c >= 'A' && c <= 'Z') freq[c - 'A']++;
        }
        int blanks = 0;
        for (int i = 0; i < 26; i++) {
            if (freq[i] > scrabbleAvailableLetters[i])
                blanks += freq[i] - scrabbleAvailableLetters[i];
        }
        if (blanks > 2) return 0;

        int score = 0;
        for (int i = 0; i < word.length(); i++) {
            char c = word.charAt(i);
            if (c >= 'A' && c <= 'Z') score += letterScores[c - 'A'];
        }
        int bonus = 0;
        int len = word.length();
        int end = len < 3 ? len : 3;
        for (int i = 0; i < end; i++) {
            char c = word.charAt(i);
            if (c >= 'A' && c <= 'Z') {
                int s = letterScores[c - 'A'];
                if (s > bonus) bonus = s;
            }
        }
        int start = len - 3;
        if (start < 0) start = 0;
        for (int i = start; i < len; i++) {
            char c = word.charAt(i);
            if (c >= 'A' && c <= 'Z') {
                int s = letterScores[c - 'A'];
                if (s > bonus) bonus = s;
            }
        }
        score = (score + bonus) * 2;
        if (len == 7) score += 50;
        return score;
    }

    static void run() {
        // Score valid Shakespeare/Scrabble words, group by score
        HashMap<Integer, ArrayList<String>> groups = new HashMap<>();
        for (String word : allWords) {
            if (scrabbleWords.contains(word)) {
                int score = scoreWord(word);
                if (score > 0) {
                    ArrayList<String> list = groups.get(Integer.valueOf(score));
                    if (list == null) {
                        list = new ArrayList<>();
                        groups.put(Integer.valueOf(score), list);
                    }
                    list.add(word);
                }
            }
        }
        // Find top 3 scores
        int best1 = 0, best2 = 0, best3 = 0;
        for (Integer key : groups.keySet()) {
            int k = key.intValue();
            if (k > best1) { best3 = best2; best2 = best1; best1 = k; }
            else if (k > best2) { best3 = best2; best2 = k; }
            else if (k > best3) { best3 = k; }
        }
        // Verify
        if (best1 != 120) {
            System.out.println("ERROR: expected best=120, got " + best1);
        }
    }

    public static void main(String[] args) {
        System.out.println("Initializing...");
        scrabbleWords = new HashSet<>();
        String[] sw = {"AA","AB","AD","AE","AG","AH","AI","AL","AM","AN","AR","AS","AT","AW","AX","AY",
            "BA","BE","BI","BO","BY","DA","DE","DO","ED","EF","EH","EL","EM","EN","ER","ES",
            "ET","EX","FA","FE","GO","HA","HE","HI","HM","HO","ID","IF","IN","IS","IT","JO",
            "KA","LA","LI","LO","MA","ME","MI","MM","MO","MU","MY","NA","NE","NO","NU","OD",
            "OE","OF","OH","OI","OM","ON","OP","OR","OS","OW","OX","OY","PA","PE","PI","QI",
            "RE","SH","SI","SO","TA","TI","TO","UH","UM","UN","UP","US","UT","WE","WO","XI",
            "XU","YA","YE","ZA","QUICKLY","ZEPHYRS","QUALIFY","QUICKEN","QUICKER"};
        for (int i = 0; i < sw.length; i++) scrabbleWords.add(sw[i]);

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

        System.out.println("Warming up...");
        for (int i = 0; i < 5; i++) run();

        System.out.println("Benchmarking...");
        int reps = 1000;
        long t0 = System.currentTimeMillis();
        for (int i = 0; i < reps; i++) run();
        long elapsed = System.currentTimeMillis() - t0;
        System.out.println("=== Scrabble Collections (" + reps + " reps) ===");
        System.out.println("Time: " + elapsed + " ms");
    }
}
