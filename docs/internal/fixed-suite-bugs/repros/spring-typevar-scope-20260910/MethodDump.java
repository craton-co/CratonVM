import java.lang.reflect.*;
import java.util.*;

public class MethodDump {
  public static void main(String[] a) throws Exception {
    List<String> names = new ArrayList<>();
    Class<?> c = Class.forName(a[0]);
    while (c != null && !c.getName().equals("java.lang.Object")) { names.add(c.getName()); c = c.getSuperclass(); }
    for (String n : names) {
      Class<?> k = Class.forName(n);
      System.out.println("== " + n + " mods=" + Modifier.toString(k.getModifiers())
          + " generic=" + k.toGenericString());
      List<String> rows = new ArrayList<>();
      for (Method m : k.getDeclaredMethods()) {
        rows.add(String.format("  %s | bridge=%b synth=%b varargs=%b | mods=%s | ret=%s | params=%s | tvars=%s | decl=%s",
            m.getName(), m.isBridge(), m.isSynthetic(), m.isVarArgs(), Modifier.toString(m.getModifiers()),
            m.getGenericReturnType(), Arrays.toString(m.getGenericParameterTypes()),
            Arrays.toString(m.getTypeParameters()), m.getDeclaringClass().getName()));
      }
      Collections.sort(rows);
      rows.forEach(System.out::println);
    }
  }
}
