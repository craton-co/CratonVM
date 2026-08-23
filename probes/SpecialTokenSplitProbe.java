import java.util.ArrayList;
import java.util.List;
import java.util.regex.Pattern;
import java.util.stream.Collectors;

/**
 * `String.split` on an alternation of {@code Pattern.quote}d literals.
 *
 * This is how GPULlama3 separates special tokens from ordinary text
 * before BPE: every special token is quoted, joined with `|`, wrapped
 * in a capturing group, and handed to {@code String.split}. The
 * literals are full of regex metacharacters — {@code <|begin_of_text|>}
 * alone carries two alternation bars — so the whole construction rests
 * on {@code \Q...\E} being honoured. If it is not, the pattern stops
 * being a list of literals and becomes an alternation of fragments,
 * and ordinary prose gets shredded on the fragments that happen to
 * appear in it.
 */
public class SpecialTokenSplitProbe {
    public static void main(String[] args) {
        List<String> specials = List.of(
            "<|begin_of_text|>", "<|end_of_text|>", "<|start_header_id|>",
            "<|end_header_id|>", "<|eot_id|>", "<|finetune_right_pad_id|>");

        String pattern = specials.stream().map(Pattern::quote)
                .collect(Collectors.joining("|", "(", ")"));
        System.out.printf("PATTERN_LEN %d%n", pattern.length());

        String[] cases = {
            "Why is the sky blue?",
            "<|begin_of_text|>hello<|eot_id|>",
            "no specials here at all",
        };
        for (String text : cases) {
            String[] parts = text.split(pattern);
            List<String> shown = new ArrayList<>();
            for (String p : parts) {
                shown.add("[" + p + "]");
            }
            System.out.printf("SPLIT %-34s -> n=%d %s%n",
                    "\"" + text + "\"", parts.length, String.join("", shown));
        }

        // The narrowest form of the same question.
        String q = Pattern.quote("<|eot_id|>");
        System.out.printf("QUOTE %s%n", q.replace("\\", "\\\\"));
        System.out.printf("MATCHES_LITERAL %b%n", Pattern.matches(q, "<|eot_id|>"));
        System.out.printf("MATCHES_FRAGMENT %b%n", Pattern.matches(q, "eot_id"));
        System.out.printf("FINDS_IN_PROSE %b%n",
                Pattern.compile(q).matcher("Why is the sky blue?").find());
    }
}
