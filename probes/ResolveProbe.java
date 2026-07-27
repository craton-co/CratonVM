import java.lang.reflect.*;
import java.util.*;

public class ResolveProbe {
  static Type resolve(Map<Type, Type> vars, Type t, String indent) {
    System.out.println(indent + "in " + t + " class=" + (t == null ? "null" : t.getClass()));
    if (t == null) return null;
    if (t instanceof Class<?> c) {
      Type s = resolve(vars, c.getGenericSuperclass(), indent + "  ");
      if (s != null) return s;
      for (Type i : c.getGenericInterfaces()) {
        s = resolve(vars, i, indent + "  ");
        if (s != null) return s;
      }
      return null;
    }
    if (t instanceof ParameterizedType p && p.getRawType() instanceof Class<?> raw) {
      TypeVariable<?>[] ps = raw.getTypeParameters();
      Type[] as = p.getActualTypeArguments();
      for (int i = 0; i < ps.length; i++) {
        vars.put(ps[i], as[i]);
        System.out.println(indent + "put " + ps[i] + "#" + ps[i].hashCode() + " -> " + as[i]);
      }
      if (raw.equals(jakarta.validation.ConstraintValidator.class)) return p;
      return resolve(vars, raw, indent + "  ");
    }
    return null;
  }
  public static void main(String[] args) throws Exception {
    Map<Type, Type> vars = new HashMap<>();
    Type result = resolve(vars, Class.forName(args[0]), "");
    System.out.println("RESULT=" + result);
    if (result instanceof ParameterizedType p)
      for (Type a : p.getActualTypeArguments()) {
        System.out.println("arg " + a + " hash=" + a.hashCode() + " maps=" + vars.get(a));
      }
  }
}
