import java.lang.reflect.*;
public class FieldsProbe {
  public static void main(String[] a) throws Exception {
    Class<?> c = Class.forName("org.springframework.core.MethodParameterKotlinTests");
    Method m = null; for (Method x: c.getDeclaredMethods()) if (x.getName().equals("suspendFun2")) m = x;
    for (Class<?> k = Method.class; k != null; k = k.getSuperclass()) {
      for (Field f : k.getDeclaredFields()) {
        if (f.getType() == String.class) {
          try { f.setAccessible(true); System.out.println(k.getSimpleName()+"."+f.getName()+" = "+f.get(m)); }
          catch (Throwable t) { System.out.println(k.getSimpleName()+"."+f.getName()+" <"+t.getClass().getSimpleName()+">"); }
        }
      }
    }
  }
}
