import java.lang.reflect.*;
public class BProbe {
  public static void main(String[] a) throws Exception {
    System.setProperty("catalina.home","C:/craton/CratonVM/apps/apache-tomcat-10.1.31");
    System.setProperty("catalina.base","C:/craton/CratonVM/apps/apache-tomcat-10.1.31");
    Class<?> bc = Class.forName("org.apache.catalina.startup.Bootstrap");
    Object b = bc.getConstructor().newInstance();
    Method m = bc.getDeclaredMethod("initClassLoaders");
    m.setAccessible(true);
    try { m.invoke(b); } catch (Throwable t) {
      Throwable r = t; while(r.getCause()!=null) r=r.getCause();
      System.out.println("initClassLoaders threw:"); r.printStackTrace();
      return;
    }
    Field cf = bc.getDeclaredField("commonLoader"); cf.setAccessible(true);
    ClassLoader common = (ClassLoader) cf.get(b);
    System.out.println("commonLoader="+common);
    try {
      Class<?> cat = common.loadClass("org.apache.catalina.startup.Catalina");
      System.out.println("loaded="+cat.getName());
    } catch (Throwable t) {
      System.out.println("loadClass threw:"); t.printStackTrace();
    }
  }
}
