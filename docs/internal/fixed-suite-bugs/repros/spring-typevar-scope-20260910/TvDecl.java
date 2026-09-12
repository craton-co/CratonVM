import java.lang.reflect.*;
import java.util.function.Function;

public class TvDecl {
  static void p(String what, Type t) {
    System.out.println(what + " => " + t + " [" + t.getClass().getName() + "]");
    if (t instanceof TypeVariable) {
      TypeVariable<?> v = (TypeVariable<?>) t;
      System.out.println("    name=" + v.getName() + "  genericDeclaration=" + v.getGenericDeclaration());
    } else if (t instanceof ParameterizedType) {
      for (Type x : ((ParameterizedType) t).getActualTypeArguments()) p(what + "  arg", x);
    }
  }
  public static void main(String[] a) throws Exception {
    Class<?> ao = Class.forName("org.assertj.core.api.AbstractObjectAssert");
    Method m = ao.getDeclaredMethod("returns", Object.class, Function.class);
    System.out.println("METHOD " + m);
    p("  genericReturnType", m.getGenericReturnType());
    Type[] ps = m.getGenericParameterTypes();
    for (int i = 0; i < ps.length; i++) p("  param" + i, ps[i]);
    System.out.println("  methodTypeParams:");
    for (TypeVariable<?> v : m.getTypeParameters()) p("    mtv", v);
    // and the class's own type params for reference
    System.out.println("CLASS typeParams of AbstractObjectAssert:");
    for (TypeVariable<?> v : ao.getTypeParameters()) p("    ctv", v);
  }
}
