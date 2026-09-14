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
 * Ground-truth reproducer for the javac crash that blocks
 * BeanRegistrationsAotContributionTests#applyToWithVeryLargeBeanDefinitions...
 *
 * Spring's AOT test compiles its generated sources in-process through
 * javax.tools.  On CratonVM that compile dies with
 *
 *   java.lang.ClassCastException: ...Symbol$MethodSymbol cannot be cast to
 *     ...Resolve$ReferenceLookupResult$StaticKind
 *
 * The stack goes DeferredAttr$DeferredAttrNode$StructuralStuckChecker.visitReference
 * -> Resolve.resolveMemberReference -> ReferenceLookupResult.<init> -> staticKind,
 * so what is needed to provoke it is a METHOD REFERENCE in a deferred-attribution
 * position -- i.e. an argument to an OVERLOADED method, with a type (not an
 * expression) as the selector, which is what makes staticKind take its stream
 * branch rather than the scalar one.
 *
 * Each case prints OK / FAILED / CRASHED.  "CRASHED" is the bug: an exception
 * escaping the compiler itself rather than a diagnostic.
 */
public class JC {

    static final class Src extends SimpleJavaFileObject {
        private final String code;

        Src(String cls, String code) {
            super(URI.create("string:///" + cls.replace('.', '/') + ".java"), Kind.SOURCE);
            this.code = code;
        }

        @Override
        public CharSequence getCharContent(boolean ignoreEncodingErrors) {
            return code;
        }
    }

    static void compile(String label, String cls, String code, boolean expectOk) {
        JavaCompiler compiler = ToolProvider.getSystemJavaCompiler();
        if (compiler == null) {
            System.out.println(label + " NOCOMPILER");
            return;
        }
        DiagnosticCollector<JavaFileObject> diags = new DiagnosticCollector<>();
        StringWriter out = new StringWriter();
        try {
            StandardJavaFileManager fm = compiler.getStandardFileManager(diags, null, null);
            List<JavaFileObject> units = new ArrayList<>();
            units.add(new Src(cls, code));
            // -d to a scratch dir is deliberately omitted: attribution is what
            // crashes, and -proc:none keeps annotation processing out of it.
            List<String> opts = Arrays.asList("-proc:none", "-d", System.getProperty("jc.out", "."));
            Boolean ok = compiler.getTask(out, fm, diags, opts, null, units).call();
            boolean good = Boolean.TRUE.equals(ok);
            if (good == expectOk) {
                System.out.println(label + " OK");
            } else {
                System.out.println(label + " FAILED ok=" + ok + " want=" + expectOk
                        + " diags=" + diags.getDiagnostics().size());
                for (Object d : diags.getDiagnostics()) {
                    System.out.println("    " + d);
                }
            }
        } catch (Throwable t) {
            System.out.println(label + " CRASHED " + t);
            StackTraceElement[] st = t.getStackTrace();
            for (int i = 0; i < st.length && i < 14; i++) {
                System.out.println("    at " + st[i]);
            }
            Throwable c = t.getCause();
            if (c != null) {
                System.out.println("  caused by " + c);
                StackTraceElement[] cs = c.getStackTrace();
                for (int i = 0; i < cs.length && i < 14; i++) {
                    System.out.println("    at " + cs[i]);
                }
            }
        }
    }

    public static void main(String[] args) {
        // 1. Static-selector method reference, NOT deferred: single candidate,
        //    takes the scalar branch of staticKind.
        compile("plain-ref", "P1",
                "public class P1 {\n"
                + "  interface F { String f(Integer i); }\n"
                + "  static String conv(Integer i) { return String.valueOf(i); }\n"
                + "  static F g() { return P1::conv; }\n"
                + "}\n", true);

        // 2. THE SHAPE FROM THE STACK: a method reference passed to an
        //    OVERLOADED method, so DeferredAttr defers it and
        //    StructuralStuckChecker.visitReference re-resolves it. Static
        //    selector -> staticKind takes the stream branch.
        compile("deferred-ref", "P2",
                "public class P2 {\n"
                + "  interface F1 { String f(Integer i); }\n"
                + "  interface F2 { String f(String s); }\n"
                + "  static void take(F1 f) { }\n"
                + "  static void take(F2 f) { }\n"
                + "  static String conv(Integer i) { return String.valueOf(i); }\n"
                + "  static void go() { take(P2::conv); }\n"
                + "}\n", true);

        // 3. Overloaded TARGET method too, so the candidate list carries more
        //    than one applicable entry and reduce() actually runs.
        compile("deferred-overloaded-target", "P3",
                "public class P3 {\n"
                + "  interface F1 { String f(Integer i); }\n"
                + "  interface F2 { String f(String s); }\n"
                + "  static void take(F1 f) { }\n"
                + "  static void take(F2 f) { }\n"
                + "  static String conv(Integer i) { return \"i\"; }\n"
                + "  static String conv(String s) { return \"s\"; }\n"
                + "  static String conv(Object o) { return \"o\"; }\n"
                // HotSpot reports "reference to take is ambiguous" here -- that
                // is the correct answer, and a diagnostic is not the bug. What
                // matters is that resolution completes instead of CRASHED.
                + "  static void go() { take(P3::conv); }\n"
                + "}\n", false);

        // 4. Mixed static / instance candidates under one name: staticKind's
        //    reduce() has to fold STATIC with NON_STATIC into BOTH.
        compile("static-and-instance", "P4",
                "public class P4 {\n"
                + "  interface F1 { String f(P4 p); }\n"
                + "  interface F2 { String f(P4 p, int i); }\n"
                + "  static void take(F1 f) { }\n"
                + "  static void take(F2 f) { }\n"
                + "  String conv() { return \"inst\"; }\n"
                + "  static String conv(P4 p) { return \"stat\"; }\n"
                // Also legitimately ambiguous on HotSpot; kept because it is the
                // one case that forces reduce() to fold STATIC with NON_STATIC.
                + "  static void go() { take(P4::conv); }\n"
                + "}\n", false);

        // 5. Generic + deferred, the shape Spring's generated registrar code
        //    actually emits (BeanDefinition suppliers as method references).
        compile("generic-deferred", "P5",
                "import java.util.function.*;\n"
                + "public class P5 {\n"
                + "  static <T> void take(Supplier<T> s) { }\n"
                + "  static <T> void take(Function<T, T> f) { }\n"
                + "  static String make() { return \"x\"; }\n"
                + "  static String id(String s) { return s; }\n"
                + "  static void go() { take(P5::make); take(P5::id); }\n"
                + "}\n", true);

        // 6. A genuinely ambiguous reference: javac must REPORT it, not crash.
        compile("ambiguous-ref", "P6",
                "public class P6 {\n"
                + "  interface F { String f(P6 p); }\n"
                + "  String conv() { return \"inst\"; }\n"
                + "  static String conv(P6 p) { return \"stat\"; }\n"
                + "  static F g() { return P6::conv; }\n"
                + "}\n", false);

        System.out.println("JCDONE");
    }
}
