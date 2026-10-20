import java.lang.module.*;
import java.lang.reflect.*;
public class Sbr2W9Small2Hash2 {
    public static void main(String[] a) throws Throwable {
        var jb = ModuleFinder.ofSystem().find("java.base").get();
        System.out.println("version=" + jb.descriptor().version());
        try {
            Class<?> mm = Class.forName("jdk.internal.module.SystemModulesMap");
            Method m = mm.getDeclaredMethod("allSystemModules"); m.setAccessible(true);
            Object sm = m.invoke(null);
            System.out.println("allSystemModules=" + (sm == null ? null : sm.getClass().getName()));
            Class<?> si = Class.forName("jdk.internal.module.SystemModules");
            for (Method x : si.getDeclaredMethods()) System.out.println("iface method " + x.getName());
            Method mh = si.getDeclaredMethod("moduleHashes"); mh.setAccessible(true);
            Object[] hs = (Object[]) mh.invoke(sm);
            System.out.println("moduleHashes len=" + hs.length);
            int nn = 0; for (Object o : hs) if (o != null) nn++;
            System.out.println("non-null hashes=" + nn);
            Method mn = si.getDeclaredMethod("moduleNames"); mn.setAccessible(true);
            String[] names = (String[]) mn.invoke(sm);
            for (int k = 0; k < names.length; k++) if (names[k].equals("java.base")) System.out.println("java.base idx " + k + " hashes=" + (hs[k] == null ? null : String.valueOf(hs[k]).substring(0, 60)));
        } catch (Throwable t) { System.out.println("ERR " + t); t.printStackTrace(System.out); }
    }
}
