import java.nio.charset.StandardCharsets;

/** Separates "the built document is corrupt" from "the parser is miscompiled". */
public class YamlSplit {
    static final String LINE = "- some list entry\n";

    /**
     * A `Yaml` with the code-point limit raised, mirroring what
     * `OriginTrackedYamlLoader` configures. snakeyaml 2.x defaults to
     * 3 MiB and this document is deliberately larger, so the plain no-arg
     * constructor makes a HEALTHY host report
     * `parse=THREW ... exceeds the limit: 3145728 code points` — a red line
     * that has nothing to do with the corruption this probe looks for.
     */
    static Object newYaml(Class<?> yamlClass) throws Exception {
        try {
            Class<?> optsClass = Class.forName("org.yaml.snakeyaml.LoaderOptions");
            Object opts = optsClass.getDeclaredConstructor().newInstance();
            optsClass.getMethod("setCodePointLimit", int.class).invoke(opts, Integer.MAX_VALUE);
            return yamlClass.getDeclaredConstructor(optsClass).newInstance(opts);
        } catch (ReflectiveOperationException pre2x) {
            // snakeyaml 1.x has no LoaderOptions constructor and no limit.
            return yamlClass.getDeclaredConstructor().newInstance();
        }
    }
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
        // Resolve the parser BEFORE the parse, and report a missing snakeyaml
        // as its own verdict. Folded into the `catch` below it reads as
        // `parse=THREW java.lang.ClassNotFoundException`, i.e. a classpath
        // mistake scores as a reproduction of the very corruption this probe
        // exists to detect.
        Class<?> yamlClass;
        try {
            yamlClass = Class.forName("org.yaml.snakeyaml.Yaml");
        } catch (Throwable t) {
            System.out.println("SPLIT-RESULT build=OK parse=UNAVAILABLE "
                    + "(snakeyaml is not on the classpath: " + t + ") - this row is ABSENT, not a pass");
            return;
        }
        try {
            Object yamlObj = newYaml(yamlClass);
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
