import java.lang.reflect.*;

/** Probe java.lang.reflect generic signature reconstruction (independent of kotlin-reflect):
 *  does getGenericParameterTypes()/getGenericReturnType() carry type arguments? */
public class GenProbe {
    public static void main(String[] args) throws Exception {
        Class<?> c = Class.forName("org.springframework.core.MethodParameterKotlinTests");
        for (String name : new String[]{"suspendFun2", "nullable"}) {
            Method m = null;
            for (Method x : c.getDeclaredMethods()) if (x.getName().equals(name)) m = x;
            System.out.println("==== " + name);
            System.out.println("  toGenericString = " + m.toGenericString());
            System.out.println("  modifiers       = 0x" + Integer.toHexString(m.getModifiers()));
            System.out.println("  paramCount      = " + m.getParameterCount());
            Type[] gpt = m.getGenericParameterTypes();
            System.out.println("  genericParamTypes.len = " + gpt.length);
            for (int i = 0; i < gpt.length; i++) {
                Type t = gpt[i];
                System.out.println("    [" + i + "] " + t + "  (" + t.getClass().getSimpleName()
                        + (t instanceof ParameterizedType ? " args=" + java.util.Arrays.toString(((ParameterizedType)t).getActualTypeArguments()) : "") + ")");
            }
            System.out.println("  genericReturnType = " + m.getGenericReturnType());
        }
    }
}
