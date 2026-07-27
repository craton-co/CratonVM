import javax.tools.*;
import java.io.*;
import java.net.URI;
import java.util.*;

/** Minimal in-process javac probe: compile a source that uses @SuppressWarnings. */
public class JavacProbe {

    static class Src extends SimpleJavaFileObject {
        final String code;
        Src(String name, String code) {
            super(URI.create("string:///" + name.replace('.', '/') + ".java"), Kind.SOURCE);
            this.code = code;
        }
        @Override public CharSequence getCharContent(boolean ignore) { return code; }
    }

    static void compile(String label, String name, String code) {
        JavaCompiler c = ToolProvider.getSystemJavaCompiler();
        if (c == null) { System.out.println(label + ": NO COMPILER"); return; }
        DiagnosticCollector<JavaFileObject> diags = new DiagnosticCollector<>();
        StandardJavaFileManager fm = c.getStandardFileManager(diags, null, null);
        File out = new File(System.getProperty("java.io.tmpdir"), "javacprobe-out");
        out.mkdirs();
        List<String> opts = Arrays.asList("-d", out.getAbsolutePath());
        boolean ok = c.getTask(null, fm, diags, opts, null,
                Collections.singletonList(new Src(name, code))).call();
        System.out.println(label + ": ok=" + ok);
        for (Diagnostic<? extends JavaFileObject> d : diags.getDiagnostics()) {
            System.out.println("   " + d.getKind() + " " + d.getMessage(null));
        }
    }

    public static void main(String[] args) throws Exception {
        compile("A plain", "P1", "class P1 { int x; }");
        compile("B suppress-single", "P2",
                "@SuppressWarnings(\"deprecation\")\nclass P2 { int x; }");
        compile("C suppress-array", "P3",
                "@SuppressWarnings({\"deprecation\", \"removal\"})\nclass P3 { int x; }");
        compile("D suppress-explicit-value", "P4",
                "@SuppressWarnings(value = \"deprecation\")\nclass P4 { int x; }");
        compile("E custom-annotation", "P5",
                "@interface MyAnn { String value(); }\n@MyAnn(\"hi\")\nclass P5 { int x; }");
        compile("F target-annotation", "P6",
                "import java.lang.annotation.*;\n@Retention(RetentionPolicy.RUNTIME)\nclass P6 { int x; }");
    }
}
