import org.assertj.core.api.SoftAssertions;
import java.lang.reflect.*;

public class SoftProbe {
  static void dump(Class<?> c) {
    System.out.println("CLASS " + c.getName());
    TypeVariable<?>[] tv = c.getTypeParameters();
    System.out.println("  typeParameters n=" + tv.length);
    for (TypeVariable<?> t : tv) {
      System.out.println("    name=" + t.getName() + " genericDecl=" + t.getGenericDeclaration()
          + " bounds=" + java.util.Arrays.toString(t.getBounds()));
    }
    System.out.println("  genericSuperclass=" + c.getGenericSuperclass());
    for (Method m : c.getDeclaredMethods()) {
      if (!m.getName().equals("returns")) continue;
      System.out.println("  METHOD " + m);
      System.out.println("    genericReturn=" + m.getGenericReturnType());
      System.out.println("    genericParams=" + java.util.Arrays.toString(m.getGenericParameterTypes()));
      System.out.println("    methodTypeParams=" + java.util.Arrays.toString(m.getTypeParameters()));
      System.out.println("    isBridge=" + m.isBridge() + " isSynthetic=" + m.isSynthetic());
    }
  }
  public static void main(String[] a) throws Exception {
    for (String n : new String[]{
        "org.assertj.core.api.IntegerAssert",
        "org.assertj.core.api.AbstractIntegerAssert",
        "org.assertj.core.api.AbstractComparableAssert",
        "org.assertj.core.api.AbstractObjectAssert",
        "org.assertj.core.api.AbstractAssert"}) {
      try { dump(Class.forName(n)); } catch (Throwable t) { System.out.println("ERR " + n + " " + t); }
    }
    System.out.println("=== now the soft assertion ===");
    try {
      SoftAssertions.assertSoftly(softly -> softly.assertThat(1).isEqualTo(1));
      System.out.println("SOFT_OK");
    } catch (Throwable t) {
      System.out.println("SOFT_FAIL " + t);
      Throwable c = t; while (c != null) { System.out.println("  cause: " + c); c = c.getCause(); }
    }
  }
}
