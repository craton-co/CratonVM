import java.lang.invoke.*;
import static java.lang.invoke.MethodType.methodType;

public class ProbeCallSite {
    static String a() { return "A"; }
    static String b() { return "B"; }
    static int c() { return 3; }
    static String d(String s) { return "D" + s; }

    static String reportInvoker(CallSite cs, MethodHandle inv) throws Throwable {
        return (String) inv.invokeExact();
    }
    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodHandle ha = l.findStatic(ProbeCallSite.class, "a", methodType(String.class));
        MethodHandle hb = l.findStatic(ProbeCallSite.class, "b", methodType(String.class));
        MethodHandle hc = l.findStatic(ProbeCallSite.class, "c", methodType(int.class));
        MethodHandle hd = l.findStatic(ProbeCallSite.class, "d", methodType(String.class, String.class));

        for (int kind = 0; kind < 2; kind++) {
            CallSite cs = kind == 0 ? new MutableCallSite(ha) : new VolatileCallSite(ha);
            String n = cs.getClass().getSimpleName();
            MethodHandle inv = cs.dynamicInvoker();
            System.out.println(n + " type=" + cs.type());
            System.out.println(n + " before=" + (String) inv.invokeExact());
            cs.setTarget(hb);
            System.out.println(n + " after=" + (String) inv.invokeExact() + " getTarget==hb " + (cs.getTarget() == hb));
            for (int i = 0; i < 3000; i++) {
                cs.setTarget((i & 1) == 0 ? ha : hb);
                String s = (String) inv.invokeExact();
                if (!s.equals((i & 1) == 0 ? "A" : "B")) { System.out.println(n + " MISMATCH at " + i + " " + s); break; }
            }
            System.out.println(n + " loop done");
            try { cs.setTarget(hc); System.out.println(n + " wrong type accepted"); }
            catch (Throwable t) { System.out.println(n + " wrongtype: " + t.getClass().getName() + ": " + String.valueOf(t.getMessage()).replaceAll("@[0-9a-f]+", "@X")); }
            try { cs.setTarget(hd); System.out.println(n + " wrong type2 accepted"); }
            catch (Throwable t) { System.out.println(n + " wrongtype2: " + t.getClass().getName()); }
            try { cs.setTarget(null); System.out.println(n + " null accepted"); }
            catch (Throwable t) { System.out.println(n + " null: " + t.getClass().getName()); }
        }
        // uninitialized call site
        MutableCallSite u = new MutableCallSite(methodType(String.class));
        try { Object o = (String) u.dynamicInvoker().invokeExact(); System.out.println("uninit ran " + o); }
        catch (Throwable t) { System.out.println("uninit: " + t.getClass().getName() + ": " + t.getMessage()); }
        u.setTarget(ha);
        System.out.println("uninit then " + (String) u.dynamicInvoker().invokeExact());
        MutableCallSite.syncAll(new MutableCallSite[] { u });
        System.out.println("syncAll ok");
        // ConstantCallSite
        ConstantCallSite k = new ConstantCallSite(ha);
        try { k.setTarget(hb); } catch (Throwable t) { System.out.println("const: " + t.getClass().getName() + ": " + t.getMessage()); }
        System.out.println("const " + (String) k.dynamicInvoker().invokeExact());

        // indy-linked via bootstrap that returns a MutableCallSite, retargeted from outside
        Runnable r = () -> {};
        // simulate indy-heavy: many lambdas / string concats
        long sum = 0;
        for (int i = 0; i < 20000; i++) { Runnable rr = () -> {}; String s = "x" + i; sum += s.length(); }
        System.out.println("indy sum " + sum);
    }
}
