public class ProbeLdr {
    static void p(String n) {
        try { Class<?> c = Class.forName(n, false, ClassLoader.getSystemClassLoader()); ClassLoader l = c.getClassLoader(); System.out.println(n + " -> " + (l == null ? "null" : l.getName()) + " module=" + c.getModule().getName()); }
        catch (Throwable t) { System.out.println(n + " -> " + t); }
    }
    public static void main(String[] a) {
        for (String n : new String[] {"com.sun.tools.javac.api.JavacTool", "com.sun.tools.javac.tree.JCTree", "com.sun.source.tree.Tree", "com.sun.source.tree.VariableTree", "com.sun.source.util.JavacTask", "com.sun.source.util.Trees", "javax.tools.ToolProvider", "javax.lang.model.element.Element", "java.sql.Connection", "jdk.jshell.JShell", "com.sun.net.httpserver.HttpServer"})
            p(n);
    }
}
