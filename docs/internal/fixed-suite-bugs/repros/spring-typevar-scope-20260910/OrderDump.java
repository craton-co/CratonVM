import java.lang.reflect.*;
public class OrderDump {
  public static void main(String[] a) throws Exception {
    for (String n : a) {
      Class<?> k = Class.forName(n);
      System.out.println("== " + n);
      int i = 0;
      for (Method m : k.getDeclaredMethods())
        System.out.println("  " + (i++) + " " + m.getName() + m.toString().substring(m.toString().indexOf('(')) + " bridge=" + m.isBridge());
    }
  }
}
