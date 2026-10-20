import java.lang.reflect.*;
public class Sbr2W9Small2Prox {
    interface I { void m(); }
    static void p(String k, Object v) { System.out.println(k + " = " + v); }
    static String n(Object s) { return String.valueOf(s).replaceAll("[$]Proxy[0-9]+", "PX"); }
    public static void main(String[] a) throws Throwable {
        Class<?> pc = Proxy.newProxyInstance(Sbr2W9Small2Prox.class.getClassLoader(), new Class<?>[]{I.class}, (x, m, ar) -> null).getClass();
        p("pc", n(pc));
        p("pc loader", pc.getClassLoader() == Sbr2W9Small2Prox.class.getClassLoader());
        p("pc module", n(pc.getModule()));
        Object arr = Array.newInstance(pc, 1);
        p("arr", n(arr.getClass()));
        p("arr class name", n(arr.getClass().getName()));
        try { p("arr comp", n(arr.getClass().getComponentType())); } catch (Throwable t) { p("arr comp", t); }
        try { p("arr comp==pc", arr.getClass().getComponentType() == pc); } catch (Throwable t) { p("arr comp==pc", t); }
        Object[] oa = (Object[]) arr;
        try { oa[0] = Proxy.newProxyInstance(Sbr2W9Small2Prox.class.getClassLoader(), new Class<?>[]{I.class}, (x, m, ar) -> null); p("store", "ok"); } catch (Throwable t) { p("store", t.getClass().getName()); }
        p("arrayType", n(pc.arrayType()));
        p("arrayType == arr.getClass", pc.arrayType() == arr.getClass());
        p("anewarray equal", n(new I[0].getClass()));
    }
}
