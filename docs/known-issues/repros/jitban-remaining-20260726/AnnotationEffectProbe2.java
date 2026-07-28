import javax.tools.JavaCompiler;
import javax.tools.ToolProvider;
import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.PrintWriter;

// Same workload as DeprecationSuppressionProbe (which reproduces
// SPRING-TESTCOMPILER.3 symptom (a) at iteration ~21), but the moment the
// suppression oracle first flips, three extra probes run in the SAME process
// to narrow the mechanism:
//
//   FUNCIFACE @FunctionalInterface on a 2-abstract-method interface -> must FAIL
//   OVERRIDE  @Override on a method that overrides nothing          -> must FAIL
//   RETENTION a custom @interface + reading it back is not compile-visible, so
//             instead: @SafeVarargs on a non-varargs method         -> must FAIL
//
// If those three still behave, annotation ATTRIBUTION is intact and the defect
// is specific to how suppression is looked up. If they stop being reported,
// the whole Annotate flush/block pairing in ClassFinder.complete is the locus.
public class AnnotationEffectProbe2 {

    static JavaCompiler compiler;
    static File libOut, out;

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
    static String lastErr = "";

    static void secondaryOracles(String suffix) throws Exception {
        int rcF = compile("Fun" + suffix,
                "@FunctionalInterface\npublic interface Fun" + suffix + " { void a(); void b(); }\n");
        System.out.println("  ORACLE FUNCIFACE rc=" + rcF + " (expect non-zero)");
        int rcO = compile("Ovr" + suffix,
                "public class Ovr" + suffix + " extends DepLib.Base {\n"
              + "  @Override public void notAnOverride() {}\n}\n");
        System.out.println("  ORACLE OVERRIDE  rc=" + rcO + " (expect non-zero)");
        int rcV = compile("Var" + suffix,
                "public class Var" + suffix + " {\n"
              + "  @SafeVarargs public final void notVarargs(String s) {}\n}\n");
        System.out.println("  ORACLE SAFEVARARGS rc=" + rcV + " (expect non-zero)");
        int rcU = compile("Unrel" + suffix,
                "public class Unrel" + suffix + " { public int x() { return 1; } }\n");
        System.out.println("  ORACLE PLAIN     rc=" + rcU + " (expect zero)");
    }

    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 40;
        compiler = ToolProvider.getSystemJavaCompiler();
        File root = new File("/data/tmp/annotation-effect2-out");
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
                    + "  }\n"
                    + "  public static class Base { public void hello() {} }\n"
                    + "}\n");
        }
        ByteArrayOutputStream libErr = new ByteArrayOutputStream();
        if (compiler.run(null, null, libErr, "-d", libOut.getAbsolutePath(), depSrc.getAbsolutePath()) != 0) {
            System.out.println("RESULT: FAIL -- library compile failed: " + libErr);
            System.exit(1);
        }

        System.out.println("=== BEFORE (control) ===");
        secondaryOracles("Pre");

        boolean tripped = false;
        for (int i = 0; i < iterations; i++) {
            String cls = "User" + i;
            int rc = compile(cls,
                    "import java.lang.SuppressWarnings;\n"
                  + "@SuppressWarnings(\"deprecation\")\n"
                  + "public class " + cls + " {\n"
                  + "  @SuppressWarnings(\"deprecation\")\n"
                  + "  public DepLib.DeprecatedSample apply(DepLib.DeprecatedSample instance) {\n"
                  + "    instance.environment = instance.value();\n"
                  + "    return instance;\n"
                  + "  }\n"
                  + "}\n",
                    "-Xlint:deprecation", "-Werror");
            if (rc != 0 && !tripped) {
                tripped = true;
                System.out.println("=== SUPPRESSION FIRST FAILED at iteration " + i + " ===");
                System.out.println(lastErr);
                System.out.println("=== AFTER (same process, suppression now broken) ===");
                secondaryOracles("Post" + i);
                // Does a source with NO cross-unit reference still suppress?
                int rcSelf = compile("SelfSup" + i,
                        "public class SelfSup" + i + " {\n"
                      + "  @Deprecated public int old() { return 1; }\n"
                      + "  @SuppressWarnings(\"deprecation\") public int x() { return old(); }\n}\n",
                        "-Xlint:deprecation", "-Werror");
                System.out.println("  ORACLE SELF-CONTAINED SUPPRESS rc=" + rcSelf + " (expect zero)");
                break;
            }
        }
        System.out.println("RESULT: tripped=" + tripped);
    }
}
