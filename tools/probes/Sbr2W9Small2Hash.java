import java.lang.module.*;
import java.lang.reflect.*;
public class Sbr2W9Small2Hash {
    public static void main(String[] a) throws Throwable {
        var cfg = ModuleLayer.boot().configuration();
        var rm = cfg.findModule("java.base");
        System.out.println("rm=" + rm);
        ModuleReference mref = rm.get().reference();
        System.out.println("mref class=" + mref.getClass().getName());
        System.out.println("descriptor=" + mref.descriptor());
        Method m = mref.getClass().getMethod("recordedHashes");
        m.setAccessible(true);
        Object h = m.invoke(mref);
        System.out.println("hashes=" + h);
        System.out.println("modules in boot: " + ModuleLayer.boot().modules().size());
        for (String n : new String[]{"java.sql", "java.logging", "jdk.compiler"}) {
            var r = cfg.findModule(n);
            System.out.println(n + " -> " + r.map(x -> x.reference().getClass().getName() + " " + x.reference().descriptor().version()).orElse("absent"));
        }
        var jb = ModuleFinder.ofSystem().find("java.base");
        System.out.println("ofSystem java.base: " + jb.map(x -> x.getClass().getName()).orElse("none"));
        if (jb.isPresent()) { Object h2 = m.invoke(jb.get()); System.out.println("ofSystem hashes=" + h2); }
    }
}
