import net.bytebuddy.ByteBuddy;
import net.bytebuddy.description.type.TypeDescription;
import net.bytebuddy.description.method.MethodDescription;
import net.bytebuddy.dynamic.DynamicType;
import java.util.*;

public class BbProbe {
  public static void main(String[] a) throws Exception {
    Class<?> k = Class.forName(a.length > 0 ? a[0] : "org.assertj.core.api.IntegerAssert");
    TypeDescription td = TypeDescription.ForLoadedType.of(k);
    System.out.println("TD " + td.getName() + " visibility=" + td.getVisibility()
        + " tvars=" + td.getTypeVariables() + " super=" + td.getSuperClass());
    List<String> rows = new ArrayList<>();
    for (MethodDescription.InDefinedShape m : td.getDeclaredMethods()) {
      if (!m.getName().equals("returns")) continue;
      rows.add("  DM " + m + " | ret=" + m.getReturnType() + " | tvars=" + m.getTypeVariables()
          + " | declVis=" + m.getDeclaringType().getVisibility() + " | bridge=" + m.isBridge());
    }
    Collections.sort(rows); rows.forEach(System.out::println);
    // walk the super chain describing 'returns'
    TypeDescription.Generic g = td.getSuperClass();
    while (g != null && !g.asErasure().getName().equals("java.lang.Object")) {
      TypeDescription e = g.asErasure();
      System.out.println("SUPER " + g + " | erasure=" + e.getName() + " vis=" + e.getVisibility()
          + " tvars=" + e.getTypeVariables());
      for (MethodDescription.InDefinedShape m : e.getDeclaredMethods()) {
        if (!m.getName().equals("returns")) continue;
        System.out.println("   R " + m + " ret=" + m.getReturnType() + " tvars=" + m.getTypeVariables());
      }
      g = e.getSuperClass();
    }
    System.out.println("=== make subclass ===");
    try {
      DynamicType.Unloaded<?> u = new ByteBuddy().subclass(k).make();
      System.out.println("MAKE_OK bytes=" + u.getBytes().length);
    } catch (Throwable t) {
      System.out.println("MAKE_FAIL " + t);
      Throwable c = t; while (c != null) { System.out.println("  cause: " + c); c = c.getCause(); }
    }
  }
}
