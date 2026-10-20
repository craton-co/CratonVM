import java.lang.reflect.*;
public class Sbr2W9Small2Arr {
    interface I { void m(); }
    static void p(String k, Object v) { System.out.println(k + " = " + v); }
    public static void main(String[] a) throws Throwable {
        Runnable r = () -> {};
        Class<?> lc = r.getClass();
        Object arr = Array.newInstance(lc, 0);
        String ln = lc.getName().replaceAll("0x[0-9a-f]+", "0xN");
        p("lambda newInstance name", arr.getClass().getName().replaceAll("0x[0-9a-f]+", "0xN"));
        p("lambda newInstance comp==c", arr.getClass().getComponentType() == lc);
        Class<?> at = lc.arrayType();
        p("lambda arrayType name", at.getName().replaceAll("0x[0-9a-f]+", "0xN"));
        p("lambda arrayType comp==c", at.getComponentType() == lc);
        try { p("lambda arrayType simple", at.getSimpleName().replaceAll("0x[0-9a-f]+", "0xN")); } catch (Throwable t) { p("lambda arrayType simple", t.getClass().getName()); }
        p("lambda arrayType same", at == arr.getClass());
        Object[] la = (Object[]) Array.newInstance(lc, 2);
        la[0] = r;
        p("lambda arr store", la[0] == r);
        Class<?> pc = Proxy.newProxyInstance(Sbr2W9Small2Arr.class.getClassLoader(), new Class<?>[]{I.class}, (x, m, ar) -> null).getClass();
        Class<?> pat = pc.arrayType();
        p("proxy arrayType name", pat.getName().replaceAll("\\$Proxy\\d+", "\\$ProxyN"));
        p("proxy arrayType comp==c", pat.getComponentType() == pc);
        p("proxy newInstance comp==c", Array.newInstance(pc, 1).getClass().getComponentType() == pc); p("proxy newInstance name", Array.newInstance(pc, 1).getClass().getName().replaceAll("[$]Proxy[0-9]+", "PX") + " comp=" + Array.newInstance(pc, 1).getClass().getComponentType().getName().replaceAll("[$]Proxy[0-9]+", "PX") + " loaderSame=" + (pc.getClassLoader() == Sbr2W9Small2Arr.class.getClassLoader()));
        p("proxy arrayType same", pat == Array.newInstance(pc, 0).getClass());
        p("string arrayType", String.class.arrayType() == String[].class);
        p("int arrayType", int.class.arrayType() == int[].class);
        p("arr arrayType", String[].class.arrayType() == String[][].class);
        long t = System.nanoTime();
        Class<?> x = null;
        for (int i = 0; i < 200000; i++) x = String.class.arrayType();
        System.out.println("arrayType(String) ns/op ~ " + (System.nanoTime() - t) / 200000);
        t = System.nanoTime();
        for (int i = 0; i < 200000; i++) x = lc.arrayType();
        System.out.println("arrayType(lambda) ns/op ~ " + (System.nanoTime() - t) / 200000);
    }
}
