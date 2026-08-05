import java.nio.charset.StandardCharsets;

/** Separates "the built document is corrupt" from "the parser is miscompiled". */
public class YamlSplit {
    static final String LINE = "- some list entry\n";
    public static void main(String[] args) throws Exception {
        int target = args.length > 0 ? Integer.parseInt(args[0]) : 4_194_304;
        StringBuilder yaml = new StringBuilder();
        while (yaml.length() < target) yaml.append(LINE);
        String s = yaml.toString();

        int n = LINE.length(), bad = -1;
        for (int i = 0; i < s.length() && bad < 0; i++)
            if (s.charAt(i) != LINE.charAt(i % n)) bad = i;
        if (bad >= 0) {
            System.out.println("SPLIT-RESULT build=CORRUPT firstBad=" + bad
                    + " line=" + (bad / n) + " len=" + s.length());
            return;
        }
        byte[] b = s.getBytes(StandardCharsets.UTF_8);
        int badb = -1;
        for (int i = 0; i < b.length && badb < 0; i++)
            if (b[i] != (byte) LINE.charAt(i % n)) badb = i;
        if (badb >= 0) {
            System.out.println("SPLIT-RESULT build=OK utf8=CORRUPT firstBad=" + badb
                    + " line=" + (badb / n));
            return;
        }
        System.out.println("SPLIT-INFO build=OK utf8=OK len=" + s.length()
                + " lines=" + (s.length() / n) + " - handing to snakeyaml");
        try {
            Object yamlObj = Class.forName("org.yaml.snakeyaml.Yaml")
                    .getDeclaredConstructor().newInstance();
            Object loaded = yamlObj.getClass()
                    .getMethod("load", String.class).invoke(yamlObj, s);
            int size = (loaded instanceof java.util.List) ? ((java.util.List<?>) loaded).size() : -1;
            System.out.println("SPLIT-RESULT build=OK parse=OK entries=" + size);
        } catch (Throwable t) {
            Throwable r = t; while (r.getCause() != null) r = r.getCause();
            System.out.println("SPLIT-RESULT build=OK parse=THREW " + r);
        }
    }
}
