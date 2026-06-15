import java.lang.reflect.*;

/** Deeply unwrap suspendFun2's Continuation param to see exactly where the
 *  parameterized wildcard bound is lost (no reliance on toString). */
public class GenProbe2 {
    public static void main(String[] args) throws Exception {
        Class<?> c = Class.forName("org.springframework.core.MethodParameterKotlinTests");
        Method m = null;
        for (Method x : c.getDeclaredMethods()) if (x.getName().equals("suspendFun2")) m = x;
        Type cont = m.getGenericParameterTypes()[1];
        System.out.println("param[1] class=" + cont.getClass().getName());
        if (cont instanceof ParameterizedType pt) {
            System.out.println("  raw=" + pt.getRawType());
            Type[] ata = pt.getActualTypeArguments();
            System.out.println("  actualTypeArguments.len=" + ata.length);
            for (Type a : ata) {
                System.out.println("  arg class=" + a.getClass().getName() + " value=" + a);
                if (a instanceof WildcardType w) {
                    System.out.println("    upperBounds=" + java.util.Arrays.toString(w.getUpperBounds())
                            + " (len " + w.getUpperBounds().length + ")");
                    Type[] lb = w.getLowerBounds();
                    System.out.println("    lowerBounds.len=" + lb.length);
                    for (Type b : lb) {
                        System.out.println("    lower class=" + (b == null ? "NULL" : b.getClass().getName()) + " value=" + b);
                        if (b instanceof ParameterizedType bp) {
                            System.out.println("      bound.raw=" + bp.getRawType()
                                    + " bound.args=" + java.util.Arrays.toString(bp.getActualTypeArguments()));
                        }
                    }
                }
            }
        }
    }
}
