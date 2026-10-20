import java.lang.classfile.*;
import java.lang.classfile.constantpool.*;
import java.lang.constant.*;
import java.lang.invoke.*;
import static java.lang.invoke.MethodType.methodType;

public class ProbeIndy {
    public static CallSite SITE;
    public static CallSite SITE_V;
    public static String a() { return "A"; }
    public static String b() { return "B"; }
    public static String a1(String s) { return "A" + s; }
    public static String b1(String s) { return "B" + s; }
    public static CallSite bsm(MethodHandles.Lookup l, String name, MethodType t) throws Throwable {
        MethodHandle h = l.findStatic(ProbeIndy.class, name.equals("v") ? "a1" : "a", t);
        CallSite cs = name.equals("v") ? new VolatileCallSite(h) : new MutableCallSite(h);
        if (name.equals("v")) SITE_V = cs; else SITE = cs;
        return cs;
    }
    public static void main(String[] args) throws Throwable {
        ClassDesc me = ClassDesc.of("ProbeIndy");
        ClassDesc gen = ClassDesc.of("GenIndy");
        DirectMethodHandleDesc bsmD = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, me, "bsm",
            MethodTypeDesc.ofDescriptor("(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/CallSite;"));
        byte[] bytes = ClassFile.of().build(gen, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC);
            cb.withMethodBody("m", MethodTypeDesc.ofDescriptor("()Ljava/lang/String;"), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code ->
                code.invokedynamic(DynamicCallSiteDesc.of(bsmD, "m", MethodTypeDesc.ofDescriptor("()Ljava/lang/String;"))).areturn());
            cb.withMethodBody("v", MethodTypeDesc.ofDescriptor("(Ljava/lang/String;)Ljava/lang/String;"), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code ->
                code.aload(0).invokedynamic(DynamicCallSiteDesc.of(bsmD, "v", MethodTypeDesc.ofDescriptor("(Ljava/lang/String;)Ljava/lang/String;"))).areturn());
        });
        MethodHandles.Lookup hl = MethodHandles.lookup().defineHiddenClass(bytes, true);
        Class<?> g = hl.lookupClass();
        MethodHandle m = hl.findStatic(g, "m", methodType(String.class));
        MethodHandle v = hl.findStatic(g, "v", methodType(String.class, String.class));
        MethodHandles.Lookup L = MethodHandles.lookup();
        MethodHandle ha = L.findStatic(ProbeIndy.class, "a", methodType(String.class));
        MethodHandle hb = L.findStatic(ProbeIndy.class, "b", methodType(String.class));
        MethodHandle ha1 = L.findStatic(ProbeIndy.class, "a1", methodType(String.class, String.class));
        MethodHandle hb1 = L.findStatic(ProbeIndy.class, "b1", methodType(String.class, String.class));
        String s0 = (String) m.invokeExact(); String t0 = (String) v.invokeExact("q"); System.out.println("initial " + s0 + " " + t0);
        int bad = 0;
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 40000; i++) {
            boolean even = (i % 7) < 4;
            if (i == 0 || i % 7 == 0 || i % 7 == 4) {
                SITE.setTarget(even ? ha : hb);
                SITE_V.setTarget(even ? ha1 : hb1);
            }
            String s = (String) m.invokeExact();
            String t = (String) v.invokeExact("z");
            if (!s.equals(even ? "A" : "B")) { bad++; if (bad < 4) sb.append("m mismatch at ").append(i).append(' ').append(s).append('\n'); }
            if (!t.equals(even ? "Az" : "Bz")) { bad++; if (bad < 4) sb.append("v mismatch at ").append(i).append(' ').append(t).append('\n'); }
        }
        System.out.print(sb);
        System.out.println("indy retarget mismatches=" + bad);
    }
}
