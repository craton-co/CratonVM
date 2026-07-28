import javax.tools.JavaCompiler;
import javax.tools.ToolProvider;
import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.PrintWriter;

// Spring-free standalone reproducer for SPRING-TESTCOMPILER.3 symptom (a):
// "@SuppressWarnings("deprecation") present in the source but not honored",
// i.e. -Werror still fails the compile with "warnings found and -Werror
// specified".
//
// The shape matters, and is why the simpler SuppressWerrorProbe (deprecated
// member and its suppressed user in ONE source file) does not reproduce it:
// in the real Spring AOT case the deprecated symbol lives in a SEPARATE,
// already-compiled .class file pulled in off the classpath, so resolving it
// goes through ClassFinder.complete -> ClassReader, and so does resolving
// java.lang.SuppressWarnings' own `value` element. This probe reproduces that
// exactly: phase 1 compiles a @Deprecated class to a directory, phase 2 then
// repeatedly compiles a fresh user class that references it under
// -Xlint:deprecation -Werror with that directory on the classpath.
public class DeprecationSuppressionProbe {

    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 40;
        JavaCompiler compiler = ToolProvider.getSystemJavaCompiler();
        if (compiler == null) {
            System.out.println("RESULT: FAIL -- no system Java compiler available");
            System.exit(1);
        }
        File root = new File("/data/tmp/deprecation-suppression-out");
        File libSrc = new File(root, "libsrc");
        File libOut = new File(root, "libout");
        File out = new File(root, "out");
        libSrc.mkdirs(); libOut.mkdirs(); out.mkdirs();

        // Phase 1: a @Deprecated type in its own compilation unit, compiled to
        // .class ahead of the loop so every later reference to it is resolved
        // from a class file rather than from source.
        File depSrc = new File(libSrc, "DepLib.java");
        try (PrintWriter pw = new PrintWriter(depSrc)) {
            pw.print("public class DepLib {\n"
                    + "  @Deprecated public static class DeprecatedSample {\n"
                    + "    @Deprecated public String environment;\n"
                    + "    @Deprecated public String value() { return \"v\"; }\n"
                    + "  }\n"
                    + "}\n");
        }
        ByteArrayOutputStream libErr = new ByteArrayOutputStream();
        int libRc = compiler.run(null, null, libErr,
                "-d", libOut.getAbsolutePath(), depSrc.getAbsolutePath());
        if (libRc != 0) {
            System.out.println("RESULT: FAIL -- could not compile the @Deprecated library: " + libErr);
            System.exit(1);
        }

        // Phase 2: repeatedly compile a user of that type. Suppression is
        // declared BOTH on the class and on the method, exactly as Spring's
        // CodeWarnings.detectDeprecation emits it.
        int failures = 0;
        for (int i = 0; i < iterations; i++) {
            String cls = "User" + i;
            String source = "import java.lang.SuppressWarnings;\n"
                    + "@SuppressWarnings(\"deprecation\")\n"
                    + "public class " + cls + " {\n"
                    + "  @SuppressWarnings(\"deprecation\")\n"
                    + "  public DepLib.DeprecatedSample apply(DepLib.DeprecatedSample instance) {\n"
                    + "    instance.environment = instance.value();\n"
                    + "    return instance;\n"
                    + "  }\n"
                    + "}\n";
            File srcFile = new File(out, cls + ".java");
            try (PrintWriter pw = new PrintWriter(srcFile)) {
                pw.print(source);
            }
            ByteArrayOutputStream errOut = new ByteArrayOutputStream();
            int rc = compiler.run(null, null, errOut,
                    "-d", out.getAbsolutePath(),
                    "-cp", libOut.getAbsolutePath(),
                    "-Xlint:deprecation", "-Werror",
                    srcFile.getAbsolutePath());
            if (rc != 0) {
                failures++;
                System.out.println("RESULT: FAIL at iteration " + i
                        + " -- javac exit " + rc + ", stderr:\n" + errOut);
                if (failures > 2) {
                    System.out.println("RESULT: too many failures, stopping early");
                    System.exit(1);
                }
            }
        }
        if (failures > 0) {
            System.out.println("RESULT: FAIL -- " + failures + "/" + iterations + " compilations failed");
            System.exit(1);
        }
        System.out.println("RESULT: OK -- " + iterations
                + " cross-compilation-unit @SuppressWarnings(\"deprecation\") + -Werror compilations, 0 failures");
    }
}
