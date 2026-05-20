public class CProbe {
  public static void main(String[] a) throws Exception {
    System.setProperty("catalina.home","C:/craton/CratonVM/apps/apache-tomcat-10.1.31");
    System.setProperty("catalina.base","C:/craton/CratonVM/apps/apache-tomcat-10.1.31");
    Class<?> cp = Class.forName("org.apache.catalina.startup.CatalinaProperties");
    java.lang.reflect.Method m = cp.getMethod("getProperty", String.class);
    System.out.println("common.loader=["+m.invoke(null,"common.loader")+"]");
    System.out.println("server.loader=["+m.invoke(null,"server.loader")+"]");
  }
}
