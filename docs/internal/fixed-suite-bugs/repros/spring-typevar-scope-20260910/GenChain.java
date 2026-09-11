import java.lang.reflect.*;
public class GenChain {
  static void show(String indent, Type t) {
    System.out.println(indent + "type=" + t + " class=" + t.getClass().getName()
      + " isPT=" + (t instanceof ParameterizedType) + " isTV=" + (t instanceof TypeVariable)
      + " isGAT=" + (t instanceof GenericArrayType) + " isWC=" + (t instanceof WildcardType)
      + " isClass=" + (t instanceof Class));
    if (t instanceof ParameterizedType) {
      ParameterizedType p = (ParameterizedType) t;
      System.out.println(indent + "  raw=" + p.getRawType() + " owner=" + p.getOwnerType());
      for (Type x : p.getActualTypeArguments()) show(indent + "    ", x);
    } else if (t instanceof TypeVariable) {
      TypeVariable<?> v = (TypeVariable<?>) t;
      Object gd = v.getGenericDeclaration();
      System.out.println(indent + "  name=" + v.getName() + " genericDeclaration=" + gd
        + " gdClass=" + (gd == null ? "null" : gd.getClass().getName()));
      Type[] bs = v.getBounds();
      for (Type b : bs) System.out.println(indent + "    bound=" + b + " (" + b.getClass().getName() + ")");
    }
  }
  public static void main(String[] a) throws Exception {
    Class<?> c = Class.forName(a.length>0?a[0]:"org.assertj.core.api.IntegerAssert");
    while (c != null && c != Object.class) {
      System.out.println("### " + c.getName());
      System.out.println("  typeParams:");
      for (TypeVariable<?> v : c.getTypeParameters()) show("    ", v);
      System.out.println("  genericSuperclass:");
      Type gs = c.getGenericSuperclass();
      if (gs != null) show("    ", gs);
      c = c.getSuperclass();
    }
  }
}
