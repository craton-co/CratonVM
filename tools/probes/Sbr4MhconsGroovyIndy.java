// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * SBR round 4, lane mhcons: Groovy 5 indy call sites (CallSite.setTarget over adapted
 * handles). Needs groovy-5.x.jar on the class path; prints MISSING without it. On the
 * carrier world (family not retired) the loop fails with WrongMethodTypeException
 * ("MethodHandle(int,int)int should be of type (int,int)Object"); on real handles it runs.
 */
public class Sbr4MhconsGroovyIndy {
    static void t(String tag, java.util.concurrent.Callable<Object> c) {
        String v;
        try { v = String.valueOf(c.call()); }
        catch (Throwable e) {
            Throwable r = e;
            while (r.getCause() != null && r.getCause() != r) r = r.getCause();
            v = "throws " + e.getClass().getName() + (r == e ? "" : " <- " + r.getClass().getName());
        }
        System.out.println(tag + " = " + v);
    }

    public static void main(String[] a) throws Exception {
        Class<?> shellClass;
        try { shellClass = Class.forName("groovy.lang.GroovyShell"); }
        catch (Throwable e) { System.out.println("groovy.present = MISSING"); return; }
        Object shell = shellClass.getConstructor().newInstance();
        java.lang.reflect.Method eval = shellClass.getMethod("evaluate", String.class);
        String[] scripts = {
            "def s = 0; for (int i = 0; i < 3000; i++) { s = s.plus(i) }; s",
            "class P { int v = 3; int twice() { v * 2 } }; def p = new P(); def t = 0; for (int i = 0; i < 3000; i++) { t += p.twice() + p.v }; t",
            "def c = { int x -> x + 1 }; def u = 0; for (int i = 0; i < 3000; i++) { u += c(i) }; u",
            "def l = []; for (int i = 0; i < 500; i++) { l << i * 2 }; l.sum()",
            "def m = [a: 1, b: 2]; def z = 0; for (int i = 0; i < 1000; i++) { z += m.a + m['b'] }; z",
            "'abc'.toUpperCase() + (1..5).collect { it * it }.join(',')",
        };
        for (int i = 0; i < scripts.length; i++) {
            final String src = scripts[i];
            t("groovy.indy" + i, () -> eval.invoke(shell, src));
        }
    }
}
