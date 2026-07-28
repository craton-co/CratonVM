import javax.tools.JavaCompiler;
import javax.tools.ToolProvider;
import java.io.ByteArrayOutputStream;
import java.io.PrintWriter;

// Consolidation test for the "does TYPES-ERASURE.1 alone (Types.erasure)
// subsume SPRING-TESTCOMPILER.1-4 / HIB-STOREDPROC-JIT.1 / the two unnamed
// ClassFinder.fillIn + ClassReader.readInnerClasses/readAttrs bans" open
// hypothesis documented in skip_list.rs next to TYPES-ERASURE.1 and in
// memory types-erasure-javac-jit-family-fix-20260726. Run against a binary
// where all 7 OTHER javac-family bans are temporarily disabled and only
// TypesErasure remains active.
//
// Deliberately varies source content across iterations to exercise the
// specific code paths each of the 7 individual bans targeted (not just the
// original minimal @Deprecated-class repro that motivated TYPES-ERASURE.1
// itself): inner classes (readInnerClasses), annotations (readAttrs /
// SPRING-TESTCOMPILER.3's suppression-annotation symptom), and generics /
// deprecation (Lower.boxIfNeeded's original trigger).
//
// 2026-07-27: kind 4 (@SuppressWarnings("deprecation") compiled under
// -Xlint:deprecation -Werror) was added once the unrelated
// "duplicate element 'value' in annotation @SuppressWarnings" defect this
// probe originally had to route around was fixed: LinkedHashSet.remove
// deleted the element but returned false, and javac's
// Annotate.attributeAnnotation reports exactly that error when
// members.remove(method) answers false (see the retired
// suppresswarnings-annotation-duplicate-value-bug-20260726 write-up).
// That kind is also the standalone form of SPRING-TESTCOMPILER.3's
// documented symptom (a).
public class JavacConsolidationProbe {

    private static String sourceFor(int i) {
        int kind = i % 5;
        switch (kind) {
            case 0:
                return "@Deprecated class Trivial" + i + " { public int x() { return " + i + "; } }";
            case 1:
                // Inner class -- exercises readInnerClasses.
                return "class Trivial" + i + " { "
                        + "static class Inner { int v = " + i + "; } "
                        + "public int x() { return new Inner().v; } }";
            case 2:
                // Annotation on a member -- exercises readAttrs. Bare
                // @Deprecated, no explicit annotation value.
                return "class Trivial" + i + " { "
                        + "@Deprecated public int old() { return " + i + "; } "
                        + "public int x() { return old(); } }";
            case 4:
                // Single-element annotation with the `value = ` shorthand,
                // whose suppression must actually be honored under -Werror
                // (see the header note): SPRING-TESTCOMPILER.3 symptom (a).
                return "class Trivial" + i + " { "
                        + "@Deprecated public int old() { return " + i + "; } "
                        + "@SuppressWarnings(\"deprecation\") public int x() { return old(); } }";
            default:
                // Generics -- exercises erasure-heavy symbol completion.
                return "import java.util.*; class Trivial" + i + " { "
                        + "public List<String> x() { List<String> l = new ArrayList<>(); l.add(\"" + i + "\"); return l; } }";
        }
    }

    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 100;
        JavaCompiler compiler = ToolProvider.getSystemJavaCompiler();
        if (compiler == null) {
            System.out.println("RESULT: FAIL -- no system Java compiler available");
            System.exit(1);
        }
        java.io.File tmpDir = new java.io.File("/data/tmp/javac-consolidation-out");
        tmpDir.mkdirs();
        int failures = 0;
        for (int i = 0; i < iterations; i++) {
            String className = "Trivial" + i;
            String source = sourceFor(i);
            java.io.File srcFile = new java.io.File(tmpDir, className + ".java");
            try (PrintWriter pw = new PrintWriter(srcFile)) {
                pw.print(source);
            }
            ByteArrayOutputStream errOut = new ByteArrayOutputStream();
            int rc = (i % 5 == 4)
                    ? compiler.run(null, null, errOut,
                            "-d", tmpDir.getAbsolutePath(),
                            "-Xlint:deprecation", "-Werror",
                            srcFile.getAbsolutePath())
                    : compiler.run(null, null, errOut,
                            "-d", tmpDir.getAbsolutePath(),
                            "-Xlint:-deprecation",
                            srcFile.getAbsolutePath());
            if (rc != 0) {
                failures++;
                System.out.println("RESULT: FAIL at iteration " + i + " (kind " + (i % 5)
                        + ") -- javac exit " + rc + ", stderr:\n" + errOut.toString());
                if (failures > 3) {
                    System.out.println("RESULT: too many failures (" + failures + "), stopping early");
                    System.exit(1);
                }
            }
        }
        if (failures > 0) {
            System.out.println("RESULT: FAIL -- " + failures + "/" + iterations + " compilations failed");
            System.exit(1);
        }
        System.out.println("RESULT: OK -- " + iterations + " varied in-process javac compilations, 0 failures");
    }
}
