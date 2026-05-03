import java.lang.reflect.*;
public class ReflectProbe {
    private static final long sf1 = 1L;
    public static int sf2 = 42;
    private final String if1;
    public int if2;
    public ReflectProbe() { this.if1 = "default"; }
    public ReflectProbe(String s, int n) { this.if1 = s; this.if2 = n; }
    private ReflectProbe(int n, String s) { this.if1 = s; this.if2 = n; }
    public ReflectProbe(int... varargs) { this.if1 = "v"; this.if2 = varargs.length; }
    public int intMethod(int x, int y) { return x + y; }
    public String strMethod(String s) { return s.toUpperCase(); }
    private static void privStaticVoid() {}
    public Object[] varargsMethod(String fmt, Object... args) { return args; }
    public static void main(String[] a) {
        Class<?> c = ReflectProbe.class;
        // Fields
        Field[] fs = c.getDeclaredFields();
        System.out.println("fields=" + fs.length);
        for (Field f : fs) System.out.println("  " + Modifier.toString(f.getModifiers()) + " " + f.getType().getSimpleName() + " " + f.getName());
        // Methods
        Method[] ms = c.getDeclaredMethods();
        int methodCount = 0;
        for (Method m : ms) { if (m.getDeclaringClass() == c && !m.isSynthetic()) methodCount++; }
        System.out.println("methods=" + methodCount);
        for (Method m : ms) {
            if (m.getDeclaringClass() != c || m.isSynthetic()) continue;
            StringBuilder sig = new StringBuilder();
            sig.append(Modifier.toString(m.getModifiers())).append(" ").append(m.getReturnType().getSimpleName()).append(" ").append(m.getName()).append("(");
            Class<?>[] pts = m.getParameterTypes();
            for (int i = 0; i < pts.length; i++) { if (i > 0) sig.append(","); sig.append(pts[i].getSimpleName()); }
            sig.append(")");
            System.out.println("  " + sig);
        }
        // Constructors
        Constructor<?>[] cs = c.getDeclaredConstructors();
        System.out.println("constructors=" + cs.length);
        for (Constructor<?> ct : cs) {
            StringBuilder sig = new StringBuilder();
            sig.append(Modifier.toString(ct.getModifiers())).append(" <init>(");
            Class<?>[] pts = ct.getParameterTypes();
            for (int i = 0; i < pts.length; i++) { if (i > 0) sig.append(","); sig.append(pts[i].getSimpleName()); }
            sig.append(")").append(ct.isVarArgs() ? " varargs" : "");
            System.out.println("  " + sig);
        }
        System.out.println("OK");
    }
}
