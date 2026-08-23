import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Comparator;
import java.util.List;
import java.util.Map;
import java.util.regex.Matcher;
import java.util.regex.Pattern;
import java.util.stream.Collectors;
import java.util.stream.IntStream;

/**
 * Everything GPULlama3's `LlamaTokenizer.encodeAsList` does to a prompt
 * before the vocabulary is consulted, reproduced with JDK primitives
 * only so it runs without a model.
 *
 * The observable defect is that CratonVM builds 11 prompt ids where
 * HotSpot builds 16 — it keeps `Why` and drops ` is the sky blue?`.
 * Every step below is a place that could lose the text, and each prints
 * enough to be compared against a real JDK rather than eyeballed:
 *
 *   1. `getBytes(UTF_8)`            — 20 bytes, or fewer?
 *   2. `bytesToUnicode()`           — 256 entries, built with
 *                                     Collectors.toMap over boxed keys
 *   3. the byte-encoded string      — same length as the byte array?
 *   4. the pretokenizer regex over THAT string (not over the prompt:
 *      the encoded space is U+0120, which is a LETTER, so the whole
 *      run is one `\p{L}+` piece)
 *   5. `IntStream.boxed().toList()` — the last thing encodeAsList does
 */
public class ByteEncoderPipelineProbe {

    private static final String LLAMA_3_PATTERN =
        "(?i:'s|'t|'re|'ve|'m|'ll|'d)|[^\\r\\n\\p{L}\\p{N}]?\\p{L}+|\\p{N}{1,3}"
        + "| ?[^\\s\\p{L}\\p{N}]+[\\r\\n]*|\\s*[\\r\\n]+|\\s+(?!\\S)|\\s+";

    /** Verbatim from LlamaTokenizer. */
    private static Map<Integer, Integer> bytesToUnicode() {
        List<Integer> bs = new ArrayList<>();
        IntStream.rangeClosed('!', '~').forEach(bs::add);
        IntStream.rangeClosed('¡', '¬').forEach(bs::add);
        IntStream.rangeClosed('®', 'ÿ').forEach(bs::add);

        List<Integer> cs = new ArrayList<>(bs);
        int n = 0;
        for (int b = 0; b < 256; ++b) {
            if (!bs.contains(b)) {
                bs.add(b);
                cs.add(256 + n);
                n += 1;
            }
        }
        return IntStream.range(0, bs.size()).boxed()
                .collect(Collectors.toMap(bs::get, cs::get));
    }

    private static List<String> findAll(Pattern pattern, String text) {
        List<String> all = new ArrayList<>();
        Matcher m = pattern.matcher(text);
        while (m.find()) {
            all.add(m.group());
        }
        return all;
    }

    public static void main(String[] args) {
        String text = args.length > 0 ? args[0] : "Why is the sky blue?";

        byte[] bytes = text.getBytes(StandardCharsets.UTF_8);
        System.out.printf("STEP1_BYTES len=%d first=%d last=%d%n",
                bytes.length, bytes[0] & 0xFF, bytes[bytes.length - 1] & 0xFF);

        Map<Integer, Integer> enc = bytesToUnicode();
        System.out.printf("STEP2_ENCODER size=%d space=%s W=%s tilde=%s hi255=%s%n",
                enc.size(), enc.get(32), enc.get((int) 'W'), enc.get(126), enc.get(255));
        long missing = IntStream.range(0, 256).filter(i -> enc.get(i) == null).count();
        System.out.printf("STEP2_MISSING %d of 256%n", missing);

        StringBuilder sb = new StringBuilder();
        for (byte b : bytes) {
            Integer cp = enc.get(Byte.toUnsignedInt(b));
            sb.appendCodePoint(cp == null ? '?' : cp);
        }
        String encoded = sb.toString();
        System.out.printf("STEP3_ENCODED len=%d cps=%d%n",
                encoded.length(), encoded.codePointCount(0, encoded.length()));
        StringBuilder cps = new StringBuilder();
        encoded.codePoints().forEach(c -> cps.append(c).append(' '));
        System.out.printf("STEP3_CODEPOINTS %s%n", cps.toString().trim());

        List<String> pieces = findAll(Pattern.compile(LLAMA_3_PATTERN), encoded);
        StringBuilder ps = new StringBuilder();
        for (String p : pieces) {
            ps.append('[').append(p.length()).append(':');
            p.codePoints().forEach(c -> ps.append(c).append(','));
            ps.append(']');
        }
        System.out.printf("STEP4_PIECES n=%d %s%n", pieces.size(), ps);

        int[] ints = {11, 22, 33, 44, 55, 66};
        List<Integer> boxed = Arrays.stream(ints).boxed().toList();
        System.out.printf("STEP5_BOXED n=%d %s%n", boxed.size(), boxed);
        List<Integer> viaCollect = Arrays.stream(ints).boxed()
                .collect(Collectors.toList());
        System.out.printf("STEP5_COLLECT n=%d %s%n", viaCollect.size(), viaCollect);
        int[] back = boxed.stream().mapToInt(i -> i).toArray();
        System.out.printf("STEP5_ROUNDTRIP n=%d%n", back.length);

        // The `min by merge rank` step encodeChunk runs every iteration.
        List<Integer> ids = List.of(5, 9, 2, 7);
        Integer min = ids.stream().min(Comparator.comparingInt(k -> k)).orElseThrow();
        System.out.printf("STEP6_MIN %d%n", min);
    }
}
