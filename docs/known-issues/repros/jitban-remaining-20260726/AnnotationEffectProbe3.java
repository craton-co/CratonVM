import javax.tools.JavaCompiler;
import javax.tools.ToolProvider;
import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.PrintWriter;

// Third narrowing pass for SPRING-TESTCOMPILER.3 symptom (a). Same trigger
// workload as DeprecationSuppressionProbe; once suppression flips, run oracles
// that separate "the warning machinery is broken" from "annotation-derived
// Lint is lost" from "deprecation specifically is lost".
//
// The oracle set is run twice, the second time in reverse order. Measured 3/3
// runs, the two -Werror oracles fail post-trip while the non-Werror one still
// suppresses correctly -- but that could equally be an ordering artifact (the
// corruption recurring per-compile rather than being sticky), so running both
// orders in one process separates the two readings.
public class AnnotationEffectProbe3 {

    static JavaCompiler compiler;
    static File libOut, out;
    static String lastErr = "";

    static int compile(String cls, String source, String... extra) throws Exception {
        File srcFile = new File(out, cls + ".java");
        try (PrintWriter pw = new PrintWriter(srcFile)) { pw.print(source); }
        ByteArrayOutputStream errOut = new ByteArrayOutputStream();
        String[] base = {"-d", out.getAbsolutePath(), "-cp", libOut.getAbsolutePath()};
        String[] argv = new String[base.length + extra.length + 1];
        System.arraycopy(base, 0, argv, 0, base.length);
        System.arraycopy(extra, 0, argv, base.length, extra.length);
        argv[argv.length - 1] = srcFile.getAbsolutePath();
        int rc = compiler.run(null, null, errOut, argv);
        lastErr = errOut.toString();
        return rc;
    }

    static String depUser(String cls, boolean suppress) {
        return (suppress ? "@SuppressWarnings(\"deprecation\")\n" : "")
             + "public class " + cls + " {\n"
             + (suppress ? "  @SuppressWarnings(\"deprecation\")\n" : "")
             + "  public DepLib.DeprecatedSample apply(DepLib.DeprecatedSample instance) {\n"
             + "    instance.environment = instance.value();\n"
             + "    return instance;\n"
             + "  }\n}\n";
    }

    // 1. Command-line suppression of the same category, same source shape.
    //    Isolates "annotation-derived Lint" from "the lint machinery".
    static void oracleCmdlineFlag(String tag, String suffix) throws Exception {
        int rc = compile("FlagSup" + suffix, depUser("FlagSup" + suffix, false),
                "-Xlint:-deprecation", "-Werror");
        System.out.println(tag + "ORACLE cmdline -Xlint:-deprecation rc=" + rc + " (expect 0)");
    }

    // 2. Same source, annotation suppression, but WITHOUT -Werror: the warning
    //    should not even be printed if suppression works.
    static void oracleNoWerror(String tag, String suffix) throws Exception {
        int rc = compile("NoWerror" + suffix, depUser("NoWerror" + suffix, true),
                "-Xlint:deprecation");
        boolean warned = lastErr.contains("has been deprecated");
        System.out.println(tag + "ORACLE annotation suppression, no -Werror rc=" + rc
                + " warningPrinted=" + warned + " (expect rc 0, warningPrinted false)");
    }

    // 3. A DIFFERENT lint category, also annotation-suppressed, also -Werror.
    static void oracleRawtypes(String tag, String suffix) throws Exception {
        int rc = compile("Raw" + suffix,
                "import java.util.List;\n"
              + "@SuppressWarnings(\"rawtypes\")\n"
              + "public class Raw" + suffix + " {\n"
              + "  public int size(List l) { return l.size(); }\n}\n",
                "-Xlint:rawtypes", "-Werror");
        System.out.println(tag + "ORACLE @SuppressWarnings(\"rawtypes\") rc=" + rc + " (expect 0)");
    }

    // 4. Control: unsuppressed cross-unit deprecation with -Werror must fail.
    static void oracleUnsuppressed(String tag, String suffix) throws Exception {
        int rc = compile("Unsup" + suffix, depUser("Unsup" + suffix, false),
                "-Xlint:deprecation", "-Werror");
        System.out.println(tag + "ORACLE unsuppressed control rc=" + rc + " (expect non-zero)");
    }

    static void oracles(String suffix) throws Exception {
        oracleCmdlineFlag("  ", suffix);
        oracleNoWerror("  ", suffix);
        oracleRawtypes("  ", suffix);
        oracleUnsuppressed("  ", suffix);
    }

    static void oraclesReversed(String suffix) throws Exception {
        oracleUnsuppressed("  [rev] ", suffix);
        oracleRawtypes("  [rev] ", suffix);
        oracleNoWerror("  [rev] ", suffix);
        oracleCmdlineFlag("  [rev] ", suffix);
    }

    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 45;
        compiler = ToolProvider.getSystemJavaCompiler();
        File root = new File("/data/tmp/annotation-effect3-out");
        File libSrc = new File(root, "libsrc");
        libOut = new File(root, "libout");
        out = new File(root, "out");
        libSrc.mkdirs(); libOut.mkdirs(); out.mkdirs();
        File depSrc = new File(libSrc, "DepLib.java");
        try (PrintWriter pw = new PrintWriter(depSrc)) {
            pw.print("public class DepLib {\n"
                    + "  @Deprecated public static class DeprecatedSample {\n"
                    + "    @Deprecated public String environment;\n"
                    + "    @Deprecated public String value() { return \"v\"; }\n"
                    + "  }\n}\n");
        }
        ByteArrayOutputStream libErr = new ByteArrayOutputStream();
        if (compiler.run(null, null, libErr, "-d", libOut.getAbsolutePath(), depSrc.getAbsolutePath()) != 0) {
            System.out.println("RESULT: FAIL -- library compile failed: " + libErr);
            System.exit(1);
        }

        System.out.println("=== BEFORE (control) ===");
        oracles("Pre");

        boolean tripped = false;
        for (int i = 0; i < iterations; i++) {
            String cls = "User" + i;
            int rc = compile(cls,
                    "import java.lang.SuppressWarnings;\n" + depUser(cls, true),
                    "-Xlint:deprecation", "-Werror");
            if (rc != 0) {
                tripped = true;
                System.out.println("=== SUPPRESSION FIRST FAILED at iteration " + i + " ===");
                System.out.println("=== AFTER ===");
                oracles("Post" + i);
                System.out.println("=== AFTER, oracle order reversed ===");
                oraclesReversed("Rev" + i);
                break;
            }
        }
        System.out.println("RESULT: tripped=" + tripped);
    }
}
