import java.util.*;
import java.util.function.*;
public class Sbr2W9Small2Ste {
    static void p(String k, Object v) { System.out.println(k + " = " + v); }
    static String f(StackTraceElement e) {
        return e.getClassName() + "." + e.getMethodName() + " mod=" + e.getModuleName() + " ver=" + e.getModuleVersion() + " ldr=" + e.getClassLoaderName() + " str=" + e.toString().replaceAll(":[0-9]+\\)", ":N)").replaceAll("0x[0-9a-f]+", "0xN");
    }
    static void trace(String k, Throwable t) {
        StackTraceElement[] st = t.getStackTrace();
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < Math.min(st.length, 4); i++) sb.append("\n    ").append(f(st[i]));
        p(k, sb);
    }
    public static void main(String[] a) throws Throwable {
        try { List.of(1).get(3); } catch (Exception e) { trace("list.of oob", e); }
        try { Object o = null; o.hashCode(); } catch (Exception e) { trace("npe app", e); }
        try { Integer.parseInt("zz"); } catch (Exception e) { trace("parseInt", e); }
        try { java.util.logging.Level.parse("zzz"); } catch (Exception e) { trace("logging", e); }
        try { java.sql.Timestamp.valueOf("zz"); } catch (Exception e) { trace("sql", e); }
        try { javax.xml.parsers.DocumentBuilderFactory.newInstance().newDocumentBuilder().parse(new java.io.ByteArrayInputStream(new byte[]{1})); } catch (Throwable e) { trace("xml", e); }
        try { Supplier<Object> s = () -> { throw new IllegalStateException("x"); }; s.get(); } catch (Exception e) { trace("lambda", e); }
        try { java.lang.reflect.Proxy.newProxyInstance(Sbr2W9Small2Ste.class.getClassLoader(), new Class<?>[]{Runnable.class}, (x, m, ar) -> { throw new RuntimeException("p"); }); Runnable r = (Runnable) java.lang.reflect.Proxy.newProxyInstance(Sbr2W9Small2Ste.class.getClassLoader(), new Class<?>[]{Runnable.class}, (x, m, ar) -> { throw new RuntimeException("p"); }); r.run(); } catch (Exception e) { trace("proxy", e); }
        try { Class.forName("no.Such"); } catch (Exception e) { trace("forName", e); }
        try { new java.util.ArrayList<>(-1); } catch (Exception e) { trace("ctor", e); }
        Thread t = new Thread(() -> { try { throw new RuntimeException("t"); } catch (Exception e) { trace("thread", e); } }, "w");
        t.start(); t.join();
        p("stackwalker", StackWalker.getInstance().walk(s -> s.limit(2).map(fr -> f(fr.toStackTraceElement())).toList()));
        p("getStackTrace", f(Thread.currentThread().getStackTrace()[1]));
        p("ctor", new StackTraceElement("com.x.Y", "m", "Y.java", 12));
        p("ctor full", new StackTraceElement("app", "mod", "1.2", "com.x.Y", "m", "Y.java", 12));
        p("ctor full jdk", new StackTraceElement(null, "java.base", "25", "java.util.X", "m", "X.java", -2));
        try { StackTraceElement.class.getDeclaredMethod("computeFormat"); p("has computeFormat", "yes"); } catch (Throwable e) { p("has computeFormat", e); }
        // exception text as printed
        java.io.StringWriter sw = new java.io.StringWriter();
        try { Collections.emptyList().iterator().next(); } catch (Exception e) { e.printStackTrace(new java.io.PrintWriter(sw)); }
        p("printStackTrace", sw.toString().lines().limit(4).map(x -> x.replaceAll(":[0-9]+\\)", ":N)")).toList());
        // many frames: format byte stable on repeat
        long h = 0;
        for (int i = 0; i < 2000; i++) { try { Integer.parseInt("x" + i); } catch (Exception e) { h += e.getStackTrace()[0].toString().length(); } }
        p("repeat", h);
    }
}
