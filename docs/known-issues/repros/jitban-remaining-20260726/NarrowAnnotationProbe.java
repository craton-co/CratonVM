import javax.tools.JavaCompiler;
import javax.tools.ToolProvider;
import java.io.ByteArrayOutputStream;
import java.io.PrintWriter;

public class NarrowAnnotationProbe {
    static int run(JavaCompiler compiler, String label, String source) throws Exception {
        java.io.File tmpDir = new java.io.File("/data/tmp/narrow-out");
        tmpDir.mkdirs();
        java.io.File srcFile = new java.io.File(tmpDir, label + ".java");
        try (PrintWriter pw = new PrintWriter(srcFile)) {
            pw.print(source);
        }
        ByteArrayOutputStream errOut = new ByteArrayOutputStream();
        int rc = compiler.run(null, null, errOut, "-d", tmpDir.getAbsolutePath(), srcFile.getAbsolutePath());
        System.out.println(label + ": RC=" + rc + (rc != 0 ? " STDERR=" + errOut : ""));
        return rc;
    }

    public static void main(String[] args) throws Exception {
        JavaCompiler compiler = ToolProvider.getSystemJavaCompiler();
        run(compiler, "OnlySuppress", "class OnlySuppress { @SuppressWarnings(\"deprecation\") public int x() { return 1; } }");
        run(compiler, "OnlyDeprecated", "class OnlyDeprecated { @Deprecated public int old() { return 1; } }");
        run(compiler, "BothSeparateMethods", "class BothSeparateMethods { @Deprecated public int old() { return 1; } @SuppressWarnings(\"deprecation\") public int x() { return 2; } }");
        run(compiler, "BothSameMethodCall", "class BothSameMethodCall { @Deprecated public int old() { return 1; } @SuppressWarnings(\"deprecation\") public int x() { return old(); } }");
        run(compiler, "SuppressOnClass", "@SuppressWarnings(\"deprecation\") class SuppressOnClass { @Deprecated public int old() { return 1; } public int x() { return old(); } }");
    }
}
