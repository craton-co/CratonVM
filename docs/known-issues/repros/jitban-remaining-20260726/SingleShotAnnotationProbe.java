import javax.tools.JavaCompiler;
import javax.tools.ToolProvider;
import java.io.ByteArrayOutputStream;
import java.io.PrintWriter;

public class SingleShotAnnotationProbe {
    public static void main(String[] args) throws Exception {
        JavaCompiler compiler = ToolProvider.getSystemJavaCompiler();
        java.io.File tmpDir = new java.io.File("/data/tmp/single-shot-out");
        tmpDir.mkdirs();
        String source = "class Solo { @Deprecated public int old() { return 1; } @SuppressWarnings(\"deprecation\") public int x() { return old(); } }";
        java.io.File srcFile = new java.io.File(tmpDir, "Solo.java");
        try (PrintWriter pw = new PrintWriter(srcFile)) {
            pw.print(source);
        }
        ByteArrayOutputStream errOut = new ByteArrayOutputStream();
        int rc = compiler.run(null, null, errOut, "-d", tmpDir.getAbsolutePath(), srcFile.getAbsolutePath());
        System.out.println("RC=" + rc);
        System.out.println("STDERR:\n" + errOut);
    }
}
