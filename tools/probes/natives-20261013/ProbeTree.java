import javax.tools.*;
import java.net.URI;
import java.util.*;
import java.lang.reflect.*;
import com.sun.source.tree.*;
import com.sun.source.util.*;

public class ProbeTree {
    public static void main(String[] a) throws Exception {
        JavaCompiler c = ToolProvider.getSystemJavaCompiler();
        String src = "package p; public class A { private String name = \"test\"; private boolean flag = true; private int n = 5; static final String K = \"k\"; }";
        JavaFileObject f = new SimpleJavaFileObject(URI.create("string:///p/A.java"), JavaFileObject.Kind.SOURCE) {
            public CharSequence getCharContent(boolean b) { return src; }
        };
        JavacTask task = (JavacTask) c.getTask(null, null, null, List.of("-proc:none"), null, List.of(f));
        for (CompilationUnitTree cu : task.parse()) {
            new TreeScanner<Void, Void>() {
                public Void visitVariable(VariableTree v, Void p) {
                    ExpressionTree init = v.getInitializer();
                    System.out.println("var " + v.getName() + " type=" + v.getType() + " mods=" + v.getModifiers().getFlags() + " init=" + init + " kind=" + (init == null ? null : init.getKind()));
                    try {
                        if (init != null) {
                            Class<?> lt = Class.forName("com.sun.source.tree.LiteralTree", false, init.getClass().getClassLoader());
                            System.out.println("  isLiteral=" + lt.isInstance(init));
                            if (lt.isInstance(init)) {
                                Method m = lt.getMethod("getValue");
                                System.out.println("  literal=" + m.invoke(init));
                            }
                        }
                        Class<?> vt = Class.forName("com.sun.source.tree.VariableTree", false, v.getClass().getClassLoader());
                        Method gm = vt.getMethod("getModifiers");
                        Object mods = gm.invoke(v);
                        Class<?> mt = Class.forName("com.sun.source.tree.ModifiersTree", false, v.getClass().getClassLoader());
                        System.out.println("  flags=" + mt.getMethod("getFlags").invoke(mods));
                    } catch (Throwable t) { System.out.println("  THROW " + t); }
                    return null;
                }
            }.scan(cu, null);
        }
    }
}
