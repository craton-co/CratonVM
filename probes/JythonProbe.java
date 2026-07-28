import javax.script.*;

public class JythonProbe {
    public static void main(String[] args) throws Exception {
        long t0 = System.currentTimeMillis();
        ScriptEngine e = new ScriptEngineManager().getEngineByName("jython");
        System.out.println("engine=" + (e == null ? "NULL" : e.getClass().getName()) + " t=" + (System.currentTimeMillis()-t0) + "ms");
        if (e == null) return;
        long t1 = System.currentTimeMillis();
        e.eval("import string");
        System.out.println("import string t=" + (System.currentTimeMillis()-t1) + "ms");
        long t2 = System.currentTimeMillis();
        e.eval("import re");
        System.out.println("import re t=" + (System.currentTimeMillis()-t2) + "ms");
        long t3 = System.currentTimeMillis();
        e.eval("def render(t, m): return t\nx = render(\"a\", None)");
        System.out.println("def+call t=" + (System.currentTimeMillis()-t3) + "ms total=" + (System.currentTimeMillis()-t0) + "ms");
    }
}
