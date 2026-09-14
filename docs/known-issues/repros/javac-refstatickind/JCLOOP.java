import java.io.StringWriter;
import java.net.URI;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import javax.tools.DiagnosticCollector;
import javax.tools.JavaCompiler;
import javax.tools.JavaFileObject;
import javax.tools.SimpleJavaFileObject;
import javax.tools.StandardJavaFileManager;
import javax.tools.ToolProvider;

/**
 * javac-under-load reproducer for the StaticKind ClassCastException.
 *
 * JC.java showed that the method-reference resolution path itself is fine on
 * CratonVM: all six shapes compile. The Spring AOT test still dies with
 *
 *   ClassCastException: Symbol$MethodSymbol cannot be cast to
 *     Resolve$ReferenceLookupResult$StaticKind
 *
 * A MethodSymbol found where a StaticKind belongs is what a STALE REFERENCE
 * looks like -- the slot was collected or relocated and something else now
 * occupies the cell. That needs allocation pressure, not a special source
 * shape, which is why the 55-minute Spring run reproduces it and a
 * six-case compile does not.
 *
 * So: generate a lot of source containing the exact construct, compile it
 * repeatedly in ONE VM, and let the collector run underneath.
 *
 *   -Djcl.classes=N   classes per compilation unit batch (default 60)
 *   -Djcl.rounds=N    compile rounds (default 40)
 *   -Djcl.refs=N      method references per generated class (default 12)
 *
 * Prints a line per round; any CRASHED line is the bug.
 */
public class JCLOOP {

    static final class Src extends SimpleJavaFileObject {
        private final String code;

        Src(String cls, String code) {
            super(URI.create("string:///" + cls + ".java"), Kind.SOURCE);
            this.code = code;
        }

        @Override
        public CharSequence getCharContent(boolean ignoreEncodingErrors) {
            return code;
        }
    }

    /**
     * Every generated class puts method references in DEFERRED positions --
     * arguments to overloaded methods -- which is what routes resolution
     * through DeferredAttr -> resolveMemberReference -> staticKind's stream.
     */
    static String gen(String cls, int refs) {
        StringBuilder b = new StringBuilder(4096);
        b.append("import java.util.function.*;\n");
        b.append("public class ").append(cls).append(" {\n");
        b.append("  interface F1 { String f(Integer i); }\n");
        b.append("  interface F2 { String f(String s); }\n");
        b.append("  static void take(F1 f) { }\n");
        b.append("  static void take(F2 f) { }\n");
        b.append("  static <T> void gtake(Supplier<T> s) { }\n");
        b.append("  static <T> void gtake(Function<T, T> f) { }\n");
        for (int i = 0; i < refs; i++) {
            b.append("  static String conv").append(i).append("(Integer v) { return String.valueOf(v); }\n");
            b.append("  String inst").append(i).append("() { return \"i\"; }\n");
            b.append("  static String mk").append(i).append("() { return \"m\"; }\n");
            b.append("  static String id").append(i).append("(String s) { return s; }\n");
        }
        b.append("  static void go() {\n");
        for (int i = 0; i < refs; i++) {
            b.append("    take(").append(cls).append("::conv").append(i).append(");\n");
            b.append("    gtake(").append(cls).append("::mk").append(i).append(");\n");
            b.append("    gtake(").append(cls).append("::id").append(i).append(");\n");
        }
        b.append("  }\n}\n");
        return b.toString();
    }

    public static void main(String[] args) {
        int classes = Integer.getInteger("jcl.classes", 60);
        int rounds = Integer.getInteger("jcl.rounds", 40);
        int refs = Integer.getInteger("jcl.refs", 12);
        String out = System.getProperty("jcl.out", ".");
        System.out.println("JCLOOP classes=" + classes + " rounds=" + rounds + " refs=" + refs);

        JavaCompiler compiler = ToolProvider.getSystemJavaCompiler();
        if (compiler == null) {
            System.out.println("NOCOMPILER");
            return;
        }
        int crashed = 0;
        for (int round = 0; round < rounds; round++) {
            List<JavaFileObject> units = new ArrayList<>();
            for (int c = 0; c < classes; c++) {
                String cls = "G" + round + "_" + c;
                units.add(new Src(cls, gen(cls, refs)));
            }
            DiagnosticCollector<JavaFileObject> diags = new DiagnosticCollector<>();
            StringWriter w = new StringWriter();
            try {
                StandardJavaFileManager fm = compiler.getStandardFileManager(diags, null, null);
                List<String> opts = Arrays.asList("-proc:none", "-d", out);
                Boolean ok = compiler.getTask(w, fm, diags, opts, null, units).call();
                if (!Boolean.TRUE.equals(ok)) {
                    System.out.println("round " + round + " FAILED diags=" + diags.getDiagnostics().size());
                    for (Object d : diags.getDiagnostics()) {
                        System.out.println("    " + d);
                        break;
                    }
                } else {
                    System.out.println("round " + round + " ok");
                }
            } catch (Throwable t) {
                crashed++;
                System.out.println("round " + round + " CRASHED " + t);
                StackTraceElement[] st = t.getStackTrace();
                for (int i = 0; i < st.length && i < 16; i++) {
                    System.out.println("    at " + st[i]);
                }
                Throwable c = t.getCause();
                while (c != null) {
                    System.out.println("  caused by " + c);
                    StackTraceElement[] cs = c.getStackTrace();
                    for (int i = 0; i < cs.length && i < 16; i++) {
                        System.out.println("    at " + cs[i]);
                    }
                    c = c.getCause();
                }
            }
            System.out.flush();
        }
        System.out.println("JCLOOPDONE crashed=" + crashed);
    }
}
