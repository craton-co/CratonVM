import java.util.ArrayList;
import java.util.List;

import org.antlr.v4.runtime.BaseErrorListener;
import org.antlr.v4.runtime.CharStreams;
import org.antlr.v4.runtime.CommonTokenStream;
import org.antlr.v4.runtime.RecognitionException;
import org.antlr.v4.runtime.Recognizer;
import org.hibernate.grammars.hql.HqlLexer;
import org.hibernate.grammars.hql.HqlParser;

/**
 * Focused witness for the "native ANTLR intrinsics lose object roots under the
 * moving young collector" defect.
 *
 * <p>{@code ASTParserLoadingTest} takes ~5 minutes per run and reproduces only
 * intermittently, which makes it useless as an inner-loop gate. This drives the
 * exact grammar path the doc names — a comparison operator directly after a
 * function call or a parenthesized expression — through Hibernate's real HQL
 * ANTLR parser thousands of times in one process, so a single mistimed
 * collection inside {@code ParserATNSimulator}'s closure/reach walk shows up as
 * a syntax error on valid HQL.
 *
 * <p>The generated parser's DFA cache is static, so one poisoned edge keeps
 * failing for the rest of the process — exactly the clustering the doc
 * describes. Run under {@code CRATONVM_GC_STRESS=<bytes>} to multiply the
 * number of young collections the parse is exposed to.
 *
 * <p>usage: {@code HqlParseStress [iterations]} — prints
 * {@code @@HQLSTRESS iters=N errors=M} and exits non-zero on any misparse.
 */
public final class HqlParseStress {

    /** Valid HQL. Every one of these must parse cleanly, every time. */
    private static final String[] QUERIES = {
        "from Animal an where sqrt(an.bodyWeight)/2 > 10",
        "from Human h where -(h.intValue - 100)=74",
        "from Animal an where (an.bodyWeight > 10) and (an.bodyWeight < 100)",
        "select count(*) from Animal an where abs(an.bodyWeight) >= 3",
        "from Human h where (h.bodyWeight + 1) * 2 <> 4",
        "from Animal a where mod(a.id, 2) = 0",
        "select a from Animal a where upper(a.description) like 'X%'",
        "from Human h where (h.nickName is not null) and length(h.nickName) > 2",
        "from Animal a where a.bodyWeight between (1 + 1) and (10 * 2)",
        "select a.id from Animal a where coalesce(a.bodyWeight, 0) <= 100",
        // Ordinal / named parameters. A poisoned prediction can drop the
        // parameter production without raising a syntax error at all: the
        // parse "succeeds" and Hibernate then reports
        // `No parameter labelled '?1' in query with ordinal parameters []`.
        // The tree-text check below is what catches that shape.
        "from Human where ?1 is null",
        "from Human where cast(?1 as string) is null",
        "from Animal a where a.bodyWeight > ?1 and a.description like ?2",
        "from Human h where h.name.first = :first and h.intValue > :n",
    };

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 400;
        List<String> misparsed = new ArrayList<>();

        outer:
        for (int i = 0; i < iterations; i++) {
            for (String query : QUERIES) {
                String failure = parse(query);
                if (failure != null) {
                    misparsed.add("iter=" + i + " " + failure + " query=" + query);
                    if (misparsed.size() >= 20) {
                        break outer;
                    }
                }
            }
        }

        for (String line : misparsed) {
            System.out.println("MISPARSE " + line);
        }
        System.out.println(
            "@@HQLSTRESS iters=" + iterations
                + " queries=" + QUERIES.length
                + " misparsed=" + misparsed.size());
        System.exit(misparsed.isEmpty() ? 0 : 1);
    }

    /**
     * Parse one statement. Returns {@code null} when the parse is good, or a
     * short description of what went wrong.
     *
     * <p>Two independent checks, because the defect has two shapes: a loud one
     * (ANTLR reports "no viable alternative" on valid HQL) and a quiet one (the
     * parse succeeds but a production is missing from the tree, which only
     * surfaces much later as an "unknown parameter" from Hibernate).
     */
    private static String parse(String query) {
        HqlLexer lexer = new HqlLexer(CharStreams.fromString(query));
        CommonTokenStream tokens = new CommonTokenStream(lexer);
        HqlParser parser = new HqlParser(tokens);
        final int[] count = {0};
        parser.removeErrorListeners();
        parser.addErrorListener(new BaseErrorListener() {
            @Override
            public void syntaxError(
                    Recognizer<?, ?> recognizer,
                    Object offendingSymbol,
                    int line,
                    int charPositionInLine,
                    String msg,
                    RecognitionException e) {
                count[0]++;
            }
        });
        String text;
        try {
            text = parser.statement().getText();
        } catch (RuntimeException e) {
            return "exception=" + e.getClass().getSimpleName();
        }
        if (count[0] > 0) {
            return "syntaxErrors=" + count[0];
        }
        // Every parameter marker in the source must survive into the tree.
        for (String marker : new String[] {"?1", "?2", ":first", ":n"}) {
            if (query.contains(marker) && !text.contains(marker)) {
                return "droppedParameter=" + marker + " tree=" + text;
            }
        }
        return null;
    }
}
