import java.lang.reflect.*;
public class Sbr2W9Small2Fmt {
    public static void main(String[] a) throws Throwable {
        Module m = Object.class.getModule();
        System.out.println("layer==boot " + (ModuleLayer.boot() == m.getLayer()));
        System.out.println("m=" + m + " name=" + m.getName());
        try {
            Class<?> hm = Class.forName("java.lang.StackTraceElement$HashedModules");
            Method c = hm.getDeclaredMethod("contains", Module.class);
            c.setAccessible(true);
            System.out.println("contains(java.base)=" + c.invoke(null, m));
            System.out.println("contains(java.sql)=" + c.invoke(null, java.sql.Connection.class.getModule()));
            System.out.println("contains(java.logging)=" + c.invoke(null, java.util.logging.Logger.class.getModule()));
            Field f = hm.getDeclaredField("HASHED_MODULES"); f.setAccessible(true);
            System.out.println("HASHED=" + f.get(null));
        } catch (Throwable t) { System.out.println("ERR " + t); }
        try { Object o = null; o.hashCode(); } catch (Exception e) { var s = e.getStackTrace()[0]; System.out.println(s + " | " + s.getModuleVersion()); }
        try { java.util.List.of(1).get(3); } catch (Exception e) { var s = e.getStackTrace()[0]; System.out.println(s + " | " + s.getModuleName()+ " | " + s.getModuleVersion()); }
    }
}
