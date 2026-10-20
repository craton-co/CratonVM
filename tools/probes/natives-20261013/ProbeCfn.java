import com.sun.source.tree.*;
import javax.tools.*;
import java.util.*;

public class ProbeCfn {
    static void t(String label, String n, ClassLoader cl) {
        try { Class<?> c = Class.forName(n, false, cl); System.out.println(label + " " + n + " -> OK " + c.getName() + " loader=" + (c.getClassLoader() == null ? "null" : c.getClassLoader().getName())); }
        catch (Throwable e) { System.out.println(label + " " + n + " -> " + e); }
    }
    public static void main(String[] a) throws Exception {
        ClassLoader cl = ToolProvider.getSystemJavaCompiler().getClass().getClassLoader();
        System.out.println("compiler loader=" + (cl == null ? "null" : cl.getName()));
        for (String n : List.of("com.sun.source.tree.VariableTree", "com.sun.source.tree.ModifiersTree", "com.sun.source.tree.LiteralTree", "com.sun.source.tree.ExpressionTree", "com.sun.source.tree.MethodInvocationTree", "com.sun.source.tree.NewClassTree", "com.sun.source.tree.Tree")) {
            t("jc-loader", n, cl);
            t("sys-loader", n, ClassLoader.getSystemClassLoader());
            t("null-loader", n, null);
        }
        Class<?> c = VariableTree.class;
        System.out.println("direct " + c + " " + c.getClassLoader());
    }
}
