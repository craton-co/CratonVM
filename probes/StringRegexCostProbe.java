/**
 * The cost side of the forced-native `String` policy: what the SBR-02 fast-regex
 * family (`CRATONVM_NATIVE_STRING_REGEX`) is actually worth.
 *
 * The record that introduced those entries
 * (docs/known-issues/jdk-only/forced-native-string-policy-two-lists-that-disagree.md
 * item 3) argues they are a *performance* optimisation rather than an RKC16N.6
 * correctness workaround, and that they therefore belong as reviewed
 * `NativeKind::Intrinsic` registrations. That argument is only worth acting on
 * if the number is real, so this probe reproduces the shape it was measured on
 * -- Spring Boot's `PluginXmlParser.format()`, a chain of four `replaceAll`
 * calls and eight literal `replace` calls over the same string -- and prints
 * elapsed milliseconds per stage.
 *
 * It prints the digest of every result as well as the timing, because a fast
 * path that is fast and wrong is not a fast path. Compare the digest across
 * binaries and modes; compare the timing only within one host at one load.
 */
public class StringRegexCostProbe {

    public static void main(String[] args) {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 300;

        StringBuilder seed = new StringBuilder();
        for (int i = 0; i < 60; i++) {
            seed.append("<plugin id=\"p").append(i).append("\">  <name>Plugin ")
                .append(i).append("</name>\r\n\t<version>1.").append(i)
                .append(".0</version> </plugin>\n");
        }
        String doc = seed.toString();

        long t0 = System.nanoTime();
        long digestReplaceAll = 0;
        for (int r = 0; r < reps; r++) {
            String s = doc;
            s = s.replaceAll("\\s+", " ");
            s = s.replaceAll("<([a-z]+) id=\"([^\"]*)\">", "[$1:$2]");
            s = s.replaceAll("</([a-z]+)>", "");
            s = s.replaceAll("(\\d+)\\.(\\d+)\\.(\\d+)", "v$1_$2_$3");
            digestReplaceAll += s.hashCode();
        }
        long t1 = System.nanoTime();

        long digestReplace = 0;
        for (int r = 0; r < reps; r++) {
            String s = doc;
            s = s.replace("<plugin", "{plugin");
            s = s.replace("</plugin>", "}");
            s = s.replace("<name>", "(");
            s = s.replace("</name>", ")");
            s = s.replace("<version>", "[");
            s = s.replace("</version>", "]");
            s = s.replace("\r\n", "\n");
            s = s.replace("\t", "    ");
            digestReplace += s.hashCode();
        }
        long t2 = System.nanoTime();

        long digestMatches = 0;
        for (int r = 0; r < reps; r++) {
            if (doc.matches("(?s).*<version>.*")) {
                digestMatches++;
            }
            if (doc.matches("^<plugin.*")) {
                digestMatches += 2;
            }
            digestMatches += doc.replaceFirst("<plugin[^>]*>", "#").length();
        }
        long t3 = System.nanoTime();

        System.out.println("REGEXCOST reps=" + reps
                + " replaceAllMs=" + ((t1 - t0) / 1_000_000)
                + " replaceMs=" + ((t2 - t1) / 1_000_000)
                + " matchesMs=" + ((t3 - t2) / 1_000_000));
        System.out.println("REGEXDIGEST replaceAll=" + digestReplaceAll
                + " replace=" + digestReplace
                + " matches=" + digestMatches);
    }
}
