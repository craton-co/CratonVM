import java.io.*; import java.lang.reflect.*;
public class F3 {
  public static void main(String[] a) throws Exception {
    File f = new File("C:/craton/CratonVM/apps/apache-tomcat-10.1.31/conf/catalina.properties");
    FileInputStream in = new FileInputStream(f);
    Class<?> c = in.getClass();
    for (Field fld : c.getDeclaredFields()) {
      if (Modifier.isStatic(fld.getModifiers())) continue;
      fld.setAccessible(true);
      System.out.println("  field "+fld.getName()+" type="+fld.getType().getName()+" val="+fld.get(in));
    }
  }
}
