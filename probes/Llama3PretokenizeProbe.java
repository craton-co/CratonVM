import java.util.ArrayList;
import java.util.List;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * The Llama-3 pretokenizer regex, applied to a prompt.
 *
 * GPULlama3 splits every input with this pattern before BPE merging, so
 * a regex engine that produces different pieces produces a different
 * prompt — and then a different answer, with nothing in the inference
 * path to blame for it. Comparing the piece list against a real JDK is
 * the whole test.
 */
public class Llama3PretokenizeProbe {
    private static final String LLAMA_3_PATTERN =
        "(?i:'s|'t|'re|'ve|'m|'ll|'d)|[^\\r\\n\\p{L}\\p{N}]?\\p{L}+|\\p{N}{1,3}"
        + "| ?[^\\s\\p{L}\\p{N}]+[\\r\\n]*|\\s*[\\r\\n]+|\\s+(?!\\S)|\\s+";

    public static void main(String[] args) {
        String[] inputs = {
            "Why is the sky blue?",
            "hello world",
            "a1 b22 c333 d4444",
            "don't  stop\n\nnow",
            "  leading and trailing  ",
        };
        Pattern p = Pattern.compile(LLAMA_3_PATTERN);
        for (String in : inputs) {
            Matcher m = p.matcher(in);
            List<String> pieces = new ArrayList<>();
            while (m.find()) {
                pieces.add(m.group());
            }
            StringBuilder sb = new StringBuilder();
            for (String s : pieces) {
                sb.append('[').append(s.replace("\n", "\\n")).append(']');
            }
            System.out.printf("IN  %-28s -> n=%d %s%n",
                    "\"" + in.replace("\n", "\\n") + "\"", pieces.size(), sb);
        }
    }
}
