import kotlin.Metadata;

/** Is mv() a real int[] or a boxed Integer[]? And what are the raw element values? */
public class MetaProbe2 {
    public static void main(String[] args) throws Exception {
        Class<?> c = Class.forName("org.springframework.core.MethodParameterKotlinTests");
        Metadata md = c.getAnnotation(Metadata.class);
        Object mv = md.mv();
        System.out.println("mv runtime class = " + mv.getClass().getName());
        System.out.println("mv array length  = " + java.lang.reflect.Array.getLength(mv));
        for (int i = 0; i < java.lang.reflect.Array.getLength(mv); i++) {
            Object e = java.lang.reflect.Array.get(mv, i);
            System.out.println("  mv[" + i + "] = " + e + " (class " + (e == null ? "null" : e.getClass().getName()) + ")");
        }
        // Also a known int[] annotation-free baseline: k() scalar
        System.out.println("k = " + md.k() + " ; xi = " + md.xi());
    }
}
