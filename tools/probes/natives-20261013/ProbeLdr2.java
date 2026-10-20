public class ProbeLdr2 {
    public static void main(String[] a) throws Exception {
        Class<?> t = com.sun.source.tree.Tree.class;
        System.out.println(t.getClassLoader() + " " + t.getModule());
        Class<?> j = Class.forName("com.sun.tools.javac.tree.JCTree");
        System.out.println(j.getClassLoader());
        System.out.println(javax.tools.ToolProvider.getSystemJavaCompiler().getClass().getClassLoader());
        System.out.println(j.getClassLoader());
        System.out.println(t.getClassLoader());
    }
}
