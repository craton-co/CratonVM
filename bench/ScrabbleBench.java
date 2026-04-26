import java.util.ArrayList;
import java.util.HashMap;
import java.util.HashSet;

/**
 * Standalone Scrabble benchmark — simplified for RustJVM compatibility.
 * Scores Shakespeare words using Scrabble letter values.
 */
public class ScrabbleBench {
    static final int[] letterScores = {
        1, 3, 3, 2, 1, 4, 2, 4, 1, 8, 5, 1, 3, 1, 1, 3, 10, 1, 1, 1, 1, 4, 4, 8, 4, 10
    };

    static final int[] scrabbleAvailableLetters = {
        9, 2, 2, 1, 12, 2, 3, 2, 9, 1, 1, 4, 2, 6, 8, 2, 1, 6, 4, 6, 4, 2, 2, 1, 2, 1
    };

    static final String[] scrabbleWords = {
        "QUICKLY","ZEPHYRS","QUALIFY","QUICKEN","QUICKER","BLAZING","PRIZING","WHIZZING",
        "QUIZZED","BUZZING","JAZZY","FIZZY","FUZZY","PUZZLE","PIZZAZZ","QUIZZES","SQUEEZE",
        "SQUEEZY","FOXLIKE","COMPLEX","JINXING","MAXIMIZE","AA","AB","AD","AE","AG","AH",
        "AI","AL","AM","AN","AR","AS","AT","AW","AX","AY","BA","BE","BI","BO","BY","DA",
        "DE","DO","ED","EF","EH","EL","EM","EN","ER","ES","ET","EX","FA","FE","GO","HA",
        "HE","HI","HM","HO","ID","IF","IN","IS","IT","JO","KA","LA","LI","LO","MA","ME",
        "MI","MO","MU","MY","NA","NE","NO","NU","OD","OE","OF","OH","OI","OM","ON","OP",
        "OR","OS","OW","OX","OY","PA","PE","PI","QI","RE","SH","SI","SO","TA","TI","TO",
        "UH","UM","UN","UP","US","UT","WE","WO","XI","XU","YA","YE","ZA"
    };

    static final String[] shakespeareWords = {
        "THE","AND","TO","OF","I","A","IN","THAT","IS","YOU","MY","IT","FOR","NOT","WITH",
        "HIS","ME","BUT","BE","HE","HAVE","THIS","WILL","YOUR","HER","WAS","ALL","AS",
        "WHAT","DO","SO","ARE","NO","WOULD","IF","SHALL","WHICH","FROM","THEIR","THAN",
        "HAD","HIM","ONE","OUR","ON","THEY","COME","BEEN","OR","WE","BY","WHEN","AN",
        "MORE","THOU","GOOD","THEM","THEN","NOW","COULD","LORD","SIR","KNOW","HOW","WELL",
        "UPON","MOST","MAN","KING","LOVE","MAKE","LIKE","TIME","VERY","HERE","DID","MADE",
        "SHOULD","THINK","MUST","THESE","SUCH","GREAT","YET","MAY","LET","SAY","MUCH",
        "GIVE","TAKE","BEFORE","SOME","LOOK","SEE","HAS","NEVER","OLD","STILL","OWN",
        "TELL","FIRST","GO","LONG","AFTER","HAND","WORD","TOO","MIGHT","JUST","WHERE",
        "DEATH","HEART","SPEAK","QUICKLY","FAIR","GRACE","BLOOD","TRUE","PRAY","WORLD",
        "FATHER","NIGHT","EVERY","PLACE","NOTHING","HEAD","WOMAN","LIFE","WAR","AGAINST",
        "WHILE","BEING","STATE","MINE","ONCE","REASON","SWEET","POWER","FEAR","POOR",
        "BETWEEN","WHOSE","MANY","KEEP","EYES","STAND","AGAIN","UNDER","DONE","HOPE",
        "HEAVEN","HOUSE","LIGHT","FACE","NATURE","MOTHER","YOUNG","THOUGH","FRIEND",
        "HOLD","LEAVE","SHOW","HONOR","ENOUGH","SPIRIT","FORTUNE","NOBLE","MASTER",
        "WRONG","HUSBAND","PEACE","STRANGE","PRINCE","WITHIN","KINGDOM","COUNTRY",
        "BELIEVE","BROTHER","FOLLOW","TROUBLE","DAUGHTER","JUSTICE","PRESENT",
        "SOLDIER","THOUGHT","PROMISE","MORNING","COUNSEL","QUESTION","CAPTAIN",
        "WELCOME","SERVANT","CERTAIN","GENERAL","HIMSELF","COMMAND","VENTURE","SILENCE",
        "ALREADY","PERHAPS","RESOLVE","TREACHERY","ASSEMBLY","PLEASURE","INNOCENT",
        "VIOLENCE","TOGETHER","APPROACH","REMEMBER","CONSIDER","JUDGMENT","GRACIOUS",
        "CREATURE","PATIENCE","INTEREST","PRACTICE","TOMORROW","THOUSAND","CEREMONY",
        "BUSINESS","AMBITION","VALIANT","COWARDLY","ORDINARY","DARKNESS","CONQUEST",
        "ABSOLUTE","DIRECTLY","CONTEMPT","WITHDRAW","GLORIOUS","DISCOVER","FAMILIAR",
        "THINKING","ARGUMENT","WHATEVER","POWERFUL","MOVEMENT","DELICATE","DIVISION",
        "FAITHFUL","TERRIBLE","BARBAROUS","ADVANTAGE","BEGINNING","MESSENGER","WILLINGLY",
        "DANGEROUS","EXCELLENT","IMPORTANT","KNOWLEDGE","SOMETIMES","OTHERWISE","FOLLOWING",
        "CHARACTER","BEAUTIFUL","CONDITION","DESPERATE","DETERMINE","ENCOUNTER","EXECUTION",
        "GENTLEMAN","GRATITUDE","HAPPINESS","HONORABLE","INNOCENCE","LAUGHTER","MAJESTY",
        "MISCHIEF","NECESSARY","OBEDIENCE","OVERTHROW","PERFECTION","PRINCIPAL","RECOMMEND",
        "REPENTANT","SACRIFICE","TREASON","UNIVERSAL","VILLAINOUS","WONDERFUL"
    };

    static int letterScore(int letter) {
        if (letter >= 65 && letter <= 90) return letterScores[letter - 65];
        return 0;
    }

    static int scrabbleScore(String word) {
        // Check blanks needed
        int[] needed = new int[26];
        for (int i = 0; i < word.length(); i++) {
            int ch = word.charAt(i);
            if (ch >= 65 && ch <= 90) needed[ch - 65]++;
        }
        int blanks = 0;
        for (int i = 0; i < 26; i++) {
            if (needed[i] > scrabbleAvailableLetters[i]) {
                blanks += needed[i] - scrabbleAvailableLetters[i];
            }
        }
        if (blanks > 2) return 0;

        // Base score
        int score = 0;
        for (int i = 0; i < word.length(); i++) {
            score += letterScore(word.charAt(i));
        }

        // Double letter bonus (best of first/last 3 chars)
        int bonus = 0;
        int len = word.length();
        int end = len < 3 ? len : 3;
        for (int i = 0; i < end; i++) {
            int s = letterScore(word.charAt(i));
            if (s > bonus) bonus = s;
        }
        int start = len - 3;
        if (start < 0) start = 0;
        for (int i = start; i < len; i++) {
            int s = letterScore(word.charAt(i));
            if (s > bonus) bonus = s;
        }
        score = (score + bonus) * 2;
        if (len == 7) score += 50;
        return score;
    }

    static int[] run(boolean[] isScrabbleWord) {
        // Score all Shakespeare words that are valid Scrabble words
        // Track top 3 scores and their words
        int best1 = 0, best2 = 0, best3 = 0;
        int count1 = 0, count2 = 0, count3 = 0;

        for (int w = 0; w < shakespeareWords.length; w++) {
            String word = shakespeareWords[w];
            if (!isScrabbleWord[w]) continue;
            int score = scrabbleScore(word);
            if (score <= 0) continue;

            if (score > best1) {
                best3 = best2; count3 = count2;
                best2 = best1; count2 = count1;
                best1 = score; count1 = 1;
            } else if (score == best1) {
                count1++;
            } else if (score > best2) {
                best3 = best2; count3 = count2;
                best2 = score; count2 = 1;
            } else if (score == best2) {
                count2++;
            } else if (score > best3) {
                best3 = score; count3 = 1;
            } else if (score == best3) {
                count3++;
            }
        }
        return new int[]{best1, count1, best2, count2, best3, count3};
    }

    public static void main(String[] args) {
        // Build scrabble word lookup
        boolean[] isScrabbleWord = new boolean[shakespeareWords.length];
        for (int i = 0; i < shakespeareWords.length; i++) {
            for (int j = 0; j < scrabbleWords.length; j++) {
                if (shakespeareWords[i].equals(scrabbleWords[j])) {
                    isScrabbleWord[i] = true;
                    break;
                }
            }
        }

        // Warmup
        for (int i = 0; i < 100; i++) {
            run(isScrabbleWord);
        }

        // Benchmark
        int reps = 10000;
        long t0 = System.currentTimeMillis();
        int[] result = null;
        for (int i = 0; i < reps; i++) {
            result = run(isScrabbleWord);
        }
        long elapsed = System.currentTimeMillis() - t0;

        System.out.println("=== Scrabble Benchmark (" + reps + " reps) ===");
        System.out.println("Time: " + elapsed + " ms");
        if (result != null) {
            System.out.println("Top scores: " + result[0] + " (" + result[1] + " words), "
                + result[2] + " (" + result[3] + "), "
                + result[4] + " (" + result[5] + ")");
        }
    }
}
