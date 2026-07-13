import java.lang.reflect.*;

public class IdentityProbe {
  static void show(String prefix, Type t) {
    System.out.println(prefix + " value=" + t + " class=" + (t == null ? "null" : t.getClass()));
    if (t instanceof TypeVariable<?> v) {
      GenericDeclaration d = v.getGenericDeclaration();
      System.out.println(prefix + " tv.name=" + v.getName() + " decl=" + d
          + " decl.class=" + (d == null ? "null" : d.getClass())
          + " decl.id=" + System.identityHashCode(d) + " tv.id=" + System.identityHashCode(v)
          + " hash=" + v.hashCode());
      if (d instanceof Class<?> c) {
        for (TypeVariable<?> candidate : c.getTypeParameters()) {
          System.out.println(prefix + " candidate=" + candidate + " eq=" + v.equals(candidate)
              + " same=" + (v == candidate) + " candidate.id=" + System.identityHashCode(candidate)
              + " candidate.hash=" + candidate.hashCode());
        }
      }
    }
    if (t instanceof ParameterizedType p) {
      System.out.println(prefix + " raw=" + p.getRawType());
      int i = 0;
      for (Type a : p.getActualTypeArguments()) show(prefix + ".arg" + i++, a);
    }
  }
  public static void main(String[] args) throws Exception {
    Class<?> c = Class.forName(args[0]);
    System.out.println("CLASS=" + c + " id=" + System.identityHashCode(c));
    show("super", c.getGenericSuperclass());
    int i = 0;
    for (Type t : c.getGenericInterfaces()) show("iface" + i++, t);
  }
}
