// SBR2 wave 9, lane MODLOAD: the rows whose JDK bytecode needs the boot modules DEFINED TO the built-in
// loaders (BuiltinClassLoader.loadModule), a real ClassLoader.packages map, a real boot Configuration
// and a real system module finder / jrt: reader:
//   Class.forName(Module,String), ClassLoader.loadClass (both overloads), Class.getPackage,
//   ModuleLayer.boot, ModuleLayer.configuration, SystemModuleFinder.find, Class.getResource.
// One line per fact; diff against HotSpot 25 verbatim.
//   javac -d out Sbr2W9ModloadRows.java
//   java|cratonvm [--jdk-only|--compatible] -cp out Sbr2W9ModloadRows
//   (optional module-path module: --module-path <dir with cratonvm.jdkonly.svc> --add-modules cratonvm.jdkonly.svc)
import java.lang.module.ModuleFinder;
import java.lang.module.ModuleReference;
import java.lang.module.ResolvedModule;
import java.net.URL;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.Optional;
import java.util.Set;
import java.util.TreeSet;

public class Sbr2W9ModloadRows {
    static void p(String k, Object v) { System.out.println(k + ": " + v); }

    static String ex(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    interface Th { Object get() throws Throwable; }

    static void t(String k, Th th) {
        try { p(k, th.get()); } catch (Throwable x) { p(k, "THROWS " + ex(x)); }
    }

    static String proto(URL u) {
        if (u == null) return "null";
        String s = u.toString();
        int i = s.indexOf(':');
        return s.substring(0, i + 1) + (s.startsWith("jrt:") ? s.substring(i + 1) : "");
    }

    static class Peek extends ClassLoader {
        Peek(ClassLoader parent) { super(parent); }
        Class<?> lc(String n, boolean r) throws ClassNotFoundException { return loadClass(n, r); }
    }

    public static void main(String[] a) throws Exception {
        Module base = Object.class.getModule();
        Module sql = java.sql.Connection.class.getModule();
        Module logm = java.util.logging.Logger.class.getModule();
        Module self = Sbr2W9ModloadRows.class.getModule();
        ClassLoader sys = ClassLoader.getSystemClassLoader();
        ClassLoader plat = ClassLoader.getPlatformClassLoader();

        // ---- Class.forName(Module, String)
        t("forName(base,String)", () -> Class.forName(base, "java.lang.String"));
        t("forName(base,app)", () -> Class.forName(base, "Sbr2W9ModloadRows"));
        t("forName(sql,Connection)", () -> Class.forName(sql, "java.sql.Connection"));
        t("forName(sql,Driver)", () -> Class.forName(sql, "java.sql.Driver"));
        t("forName(base,sql)", () -> Class.forName(base, "java.sql.Connection"));
        t("forName(sql,base)", () -> Class.forName(sql, "java.lang.String"));
        t("forName(base,missing)", () -> Class.forName(base, "no.Such"));
        t("forName(base,default pkg)", () -> Class.forName(base, "NoPackage"));
        t("forName(base,array)", () -> Class.forName(base, "[I"));
        t("forName(base,slash)", () -> Class.forName(base, "java/lang/String"));
        t("forName(unnamed,app)", () -> Class.forName(self, "Sbr2W9ModloadRows"));
        t("forName(unnamed,String)", () -> Class.forName(self, "java.lang.String"));
        t("forName(null,String)", () -> Class.forName((Module) null, "java.lang.String"));
        t("forName(base,null)", () -> Class.forName(base, null));
        t("forName(logging,Logger)", () -> Class.forName(logm, "java.util.logging.Logger"));
        t("forName(logging,Level)", () -> Class.forName(logm, "java.util.logging.Level"));
        t("forName(logging,lazy handler)", () -> Class.forName(logm, "java.util.logging.ConsoleHandler"));
        t("forName(sql,lazy)", () -> Class.forName(sql, "java.sql.SQLPermission"));
        Optional<Module> loc = ModuleLayer.boot().findModule("jdk.localedata");
        p("localedata present", loc.isPresent());
        if (loc.isPresent()) {
            t("forName(localedata,adapter)", () -> Class.forName(loc.get(),
                    "sun.util.resources.provider.LocaleDataProvider") != null);
        }
        Optional<Module> svc = ModuleLayer.boot().findModule("cratonvm.jdkonly.svc");
        p("svc present", svc.isPresent());
        if (svc.isPresent()) {
            t("forName(svc,Greeter)", () -> Class.forName(svc.get(), "com.cratonvm.jdkonly.svc.Greeter").getName());
            t("forName(svc,internal)", () -> Class.forName(svc.get(), "com.cratonvm.jdkonly.svc.internal.EnGreeter").getName());
            t("forName(svc,app)", () -> Class.forName(svc.get(), "Sbr2W9ModloadRows"));
            t("forName(base,svc)", () -> Class.forName(base, "com.cratonvm.jdkonly.svc.Greeter"));
            t("sys.loadClass(svc internal)", () -> sys.loadClass("com.cratonvm.jdkonly.svc.internal.EnGreeter").getName());
            t("plat.loadClass(svc)", () -> plat.loadClass("com.cratonvm.jdkonly.svc.Greeter"));
            t("svc loader is sys", () -> svc.get().getClassLoader() == sys);
            t("svc class module", () -> sys.loadClass("com.cratonvm.jdkonly.svc.Greeter").getModule() == svc.get());
            t("Class.forName(svc,name,false,sys)", () -> Class.forName("com.cratonvm.jdkonly.svc.Greeter", false, sys).getName());
        }

        // ---- ClassLoader.loadClass (both overloads), delegation, errors
        t("sys.loadClass(String)", () -> sys.loadClass("java.lang.String"));
        t("sys.loadClass(sql)", () -> sys.loadClass("java.sql.Connection"));
        t("sys.loadClass(self)", () -> sys.loadClass("Sbr2W9ModloadRows") == Sbr2W9ModloadRows.class);
        t("sys.loadClass(missing)", () -> sys.loadClass("no.Such"));
        t("sys.loadClass([I)", () -> sys.loadClass("[I"));
        t("sys.loadClass(null)", () -> sys.loadClass(null));
        t("sys.loadClass(slash)", () -> sys.loadClass("java/lang/String"));
        t("sys.loadClass(empty)", () -> sys.loadClass(""));
        t("plat.loadClass(String)", () -> plat.loadClass("java.lang.String"));
        t("plat.loadClass(sql)", () -> plat.loadClass("java.sql.Connection"));
        t("plat.loadClass(app)", () -> plat.loadClass("Sbr2W9ModloadRows"));
        t("plat.loadClass(missing)", () -> plat.loadClass("no.Such"));
        t("plat.loadClass(logging)", () -> plat.loadClass("java.util.logging.Logger"));
        t("sys.loadClass(logging)", () -> sys.loadClass("java.util.logging.Handler"));
        Peek pk = new Peek(sys);
        t("peek.loadClass(String,false)", () -> pk.lc("java.lang.String", false));
        t("peek.loadClass(sql,true)", () -> pk.lc("java.sql.Driver", true));
        t("peek.loadClass(self)", () -> pk.lc("Sbr2W9ModloadRows", false) == Sbr2W9ModloadRows.class);
        t("peek.loadClass(missing)", () -> pk.lc("no.Such", false));
        t("peek.loadClass(null)", () -> pk.lc(null, false));
        t("sys.loadClass twice same", () -> sys.loadClass("java.sql.Savepoint") == sys.loadClass("java.sql.Savepoint"));
        t("sys.loadClass ctx ClassNotFound msg", () -> {
            try { sys.loadClass("a.b.C"); return "loaded"; } catch (ClassNotFoundException e) { return e.getMessage(); }
        });

        // ---- Class.getPackage and the package map
        t("String pkg", () -> String.class.getPackage());
        t("String pkg name", () -> String.class.getPackage().getName());
        t("String pkg ==", () -> String.class.getPackage() == Object.class.getPackage());
        t("String pkg == BootLoader", () -> String.class.getPackage() == Package.getPackage("java.lang"));
        t("sql pkg", () -> java.sql.Connection.class.getPackage());
        t("sql pkg ==", () -> java.sql.Connection.class.getPackage() == java.sql.Driver.class.getPackage());
        t("sql pkg == plat defined", () -> java.sql.Connection.class.getPackage() == plat.getDefinedPackage("java.sql"));
        t("sql pkg == Package.getPackage", () -> java.sql.Connection.class.getPackage() == Package.getPackage("java.sql"));
        t("self pkg", () -> Sbr2W9ModloadRows.class.getPackage());
        t("self pkg name empty", () -> Sbr2W9ModloadRows.class.getPackage().getName().isEmpty());
        t("self pkg ==", () -> Sbr2W9ModloadRows.class.getPackage() == Sbr2W9ModloadRows.class.getPackage());
        t("self pkg == sys defined", () -> Sbr2W9ModloadRows.class.getPackage() == sys.getDefinedPackage(""));
        t("self nested pkg ==", () -> Sbr2W9ModloadRows.class.getPackage() == Peek.class.getPackage());
        t("int pkg", () -> int.class.getPackage());
        t("array pkg", () -> String[].class.getPackage());
        t("obj array pkg", () -> Sbr2W9ModloadRows[].class.getPackage());
        t("lambda pkg", () -> ((Runnable) () -> { }).getClass().getPackage());
        t("lambda pkg ==", () -> ((Runnable) () -> { }).getClass().getPackage() == Sbr2W9ModloadRows.class.getPackage());
        t("proxy pkg", () -> java.lang.reflect.Proxy.newProxyInstance(sys, new Class<?>[] {Runnable.class},
                (x, m, y) -> null).getClass().getPackage().getName().startsWith("jdk.proxy"));
        t("String pkg isSealed", () -> String.class.getPackage().isSealed());
        t("String pkg impl title", () -> String.class.getPackage().getImplementationTitle());
        t("String pkg spec title", () -> String.class.getPackage().getSpecificationTitle());
        t("String pkg annotations", () -> String.class.getPackage().getAnnotations().length);
        t("String pkg toString", () -> String.class.getPackage().toString());
        t("String pkg hash eq", () -> String.class.getPackage().hashCode() == Object.class.getPackage().hashCode());
        t("String pkg module", () -> String.class.getPackage().getName() + " " + base.getName());
        t("pkg of Map.Entry", () -> java.util.Map.Entry.class.getPackage().getName());
        t("plat getDefinedPackage java.sql", () -> plat.getDefinedPackage("java.sql"));
        t("plat getDefinedPackage java.lang", () -> plat.getDefinedPackage("java.lang"));
        t("sys getDefinedPackage java.sql", () -> sys.getDefinedPackage("java.sql"));
        t("sys getDefinedPackage self", () -> sys.getDefinedPackage("") != null);
        t("plat definedPackages has java.sql", () -> {
            for (Package q : plat.getDefinedPackages()) if (q.getName().equals("java.sql")) return true;
            return false;
        });
        t("sys definedPackages has self", () -> {
            for (Package q : sys.getDefinedPackages()) if (q.getName().isEmpty()) return true;
            return false;
        });
        t("Package.getPackages has java.lang", () -> {
            for (Package q : Package.getPackages()) if (q.getName().equals("java.lang")) return true;
            return false;
        });
        t("Package.getPackage(java.util)", () -> Package.getPackage("java.util"));
        t("Package.getPackage(no.such)", () -> Package.getPackage("no.such"));
        ClassLoader fresh = new ClassLoader("fresh", sys) { };
        t("custom getDefinedPackage before", () -> fresh.getDefinedPackage("x.y"));
        t("custom Class.getPackage (self via fresh)", () -> fresh.loadClass("Sbr2W9ModloadRows").getPackage().getName().isEmpty());

        // ---- ModuleLayer.boot / configuration
        ModuleLayer boot = ModuleLayer.boot();
        t("boot ==", () -> boot == ModuleLayer.boot());
        t("boot findModule base", () -> boot.findModule("java.base").isPresent());
        t("boot findModule ==", () -> boot.findModule("java.base").get() == base);
        t("boot base layer", () -> base.getLayer() == boot);
        t("boot parents", () -> boot.parents().size());
        t("boot parent empty", () -> boot.parents().get(0) == ModuleLayer.empty());
        t("boot modules>=60", () -> boot.modules().size() >= 60);
        t("boot toString has base", () -> boot.toString().contains("java.base"));
        t("boot cfg !=null", () -> boot.configuration() != null);
        t("boot cfg ==", () -> boot.configuration() == boot.configuration());
        t("boot cfg parents", () -> boot.configuration().parents().size());
        t("boot cfg parent empty", () -> boot.configuration().parents().get(0) == java.lang.module.Configuration.empty());
        t("boot cfg modules>20", () -> boot.configuration().modules().size() > 20);
        t("boot cfg size==layer size", () -> boot.configuration().modules().size() == boot.modules().size());
        t("boot cfg findModule base", () -> boot.configuration().findModule("java.base").isPresent());
        t("boot cfg findModule none", () -> boot.configuration().findModule("no.such").isPresent());
        t("boot cfg base ref name", () -> boot.configuration().findModule("java.base").get().reference().descriptor().name());
        t("boot cfg base cfg ==", () -> boot.configuration().findModule("java.base").get().configuration() == boot.configuration());
        t("boot cfg base name", () -> boot.configuration().findModule("java.base").get().name());
        t("boot cfg base reads size 0", () -> boot.configuration().findModule("java.base").get().reads().size());
        t("boot cfg sql reads", () -> {
            Set<String> s = new TreeSet<>();
            for (ResolvedModule r : boot.configuration().findModule("java.sql").get().reads()) s.add(r.name());
            return s;
        });
        t("boot cfg sql ref descriptor requires", () -> {
            Set<String> s = new TreeSet<>();
            for (var r : boot.configuration().findModule("java.sql").get().reference().descriptor().requires()) s.add(r.name());
            return s;
        });
        t("boot cfg toString has base", () -> boot.configuration().toString().contains("java.base"));
        t("boot cfg layer modules agree", () -> {
            Set<String> a1 = new TreeSet<>(), b1 = new TreeSet<>();
            for (ResolvedModule r : boot.configuration().modules()) a1.add(r.name());
            for (Module m : boot.modules()) b1.add(m.getName());
            return a1.equals(b1);
        });
        t("empty cfg", () -> ModuleLayer.empty().configuration().modules().size());
        t("boot cfg stream count>20", () -> boot.configuration().modules().stream().count() > 20);
        t("child layer defineModules", () -> {
            var cf = boot.configuration().resolve(ModuleFinder.of(), ModuleFinder.of(), Set.of());
            return cf.modules().size() + " " + cf.parents().size();
        });

        // ---- ModuleFinder.ofSystem
        ModuleFinder sf = ModuleFinder.ofSystem();
        t("ofSystem find java.base", () -> sf.find("java.base").isPresent());
        t("ofSystem find name", () -> sf.find("java.base").get().descriptor().name());
        t("ofSystem find sql name", () -> sf.find("java.sql").get().descriptor().name());
        t("ofSystem find nope", () -> sf.find("cratonvm.absent").isPresent());
        t("ofSystem find empty", () -> sf.find("").isPresent());
        t("ofSystem find null", () -> sf.find(null).isPresent());
        t("ofSystem find ==", () -> sf.find("java.base").get() == sf.find("java.base").get());
        t("ofSystem find location scheme", () -> sf.find("java.base").get().location().get().getScheme());
        t("ofSystem find location", () -> sf.find("java.base").get().location().get());
        t("ofSystem find sql location", () -> sf.find("java.sql").get().location().get());
        t("ofSystem find desc packages has java.lang", () -> sf.find("java.base").get().descriptor().packages().contains("java.lang"));
        t("ofSystem find desc exports has java.lang", () -> {
            for (var e : sf.find("java.base").get().descriptor().exports()) if (e.source().equals("java.lang")) return true;
            return false;
        });
        t("ofSystem find sql requires logging", () -> {
            for (var r : sf.find("java.sql").get().descriptor().requires()) if (r.name().equals("java.logging")) return true;
            return false;
        });
        t("ofSystem findAll>=60", () -> sf.findAll().size() >= 60);
        t("ofSystem findAll has base", () -> {
            for (ModuleReference r : sf.findAll()) if (r.descriptor().name().equals("java.base")) return true;
            return false;
        });
        t("ofSystem findAll agrees with boot layer", () -> {
            Set<String> a1 = new TreeSet<>(), b1 = new TreeSet<>();
            for (ModuleReference r : sf.findAll()) a1.add(r.descriptor().name());
            for (Module m : boot.modules()) b1.add(m.getName());
            return a1.containsAll(b1);
        });
        t("ofSystem reader find class", () -> {
            try (var rd = sf.find("java.base").get().open()) {
                return rd.find("java/lang/Object.class").isPresent();
            }
        });
        t("ofSystem reader find class uri", () -> {
            try (var rd = sf.find("java.base").get().open()) {
                return rd.find("java/lang/Object.class").get();
            }
        });
        t("ofSystem reader find missing", () -> {
            try (var rd = sf.find("java.base").get().open()) {
                return rd.find("java/lang/NoSuch.class").isPresent();
            }
        });
        t("ofSystem reader read class magic", () -> {
            try (var rd = sf.find("java.base").get().open()) {
                var bb = rd.read("java/lang/Object.class").get();
                return Integer.toHexString(bb.getInt());
            }
        });
        t("ofSystem reader list has Object", () -> {
            try (var rd = sf.find("java.sql").get().open()) {
                return rd.list().anyMatch("java/sql/Connection.class"::equals);
            }
        });

        // ---- Class.getResource
        t("String.getResource(String.class)", () -> proto(String.class.getResource("String.class")));
        t("String.getResource(String.class) uri", () -> String.class.getResource("String.class"));
        t("Object.getResource(abs)", () -> proto(Object.class.getResource("/java/lang/Object.class")));
        t("Object.getResource(module-info)", () -> proto(Object.class.getResource("/module-info.class")));
        t("Object.getResource(missing)", () -> proto(Object.class.getResource("/java/lang/Nope.class")));
        t("sql.getResource(Connection.class)", () -> proto(java.sql.Connection.class.getResource("Connection.class")));
        t("String[].getResource", () -> proto(String[].class.getResource("String.class")));
        t("int.getResource", () -> proto(int.class.getResource("String.class")));
        t("self.getResource(self)", () -> proto(Sbr2W9ModloadRows.class.getResource("Sbr2W9ModloadRows.class")));
        t("self.getResource(abs)", () -> proto(Sbr2W9ModloadRows.class.getResource("/Sbr2W9ModloadRows.class")));
        t("self.getResource(missing)", () -> proto(Sbr2W9ModloadRows.class.getResource("/nope.txt")));
        t("self.getResource(null)", () -> Sbr2W9ModloadRows.class.getResource(null));
        t("self.getResource(empty)", () -> proto(Sbr2W9ModloadRows.class.getResource("")) );
        t("sys.getResource(Object)", () -> proto(sys.getResource("java/lang/Object.class")));
        t("plat.getResource(Object)", () -> proto(plat.getResource("java/lang/Object.class")));
        t("plat.getResource(Connection)", () -> proto(plat.getResource("java/sql/Connection.class")));
        t("ClassLoader.getSystemResource(String)", () -> proto(ClassLoader.getSystemResource("java/lang/String.class")));
        t("getResources(Object)", () -> {
            var e = sys.getResources("java/lang/Object.class");
            List<String> l = new ArrayList<>();
            while (e.hasMoreElements()) l.add(proto(e.nextElement()));
            Collections.sort(l);
            return l;
        });
        t("String.getResourceAsStream", () -> { try (var in = String.class.getResourceAsStream("String.class")) { return in != null; } });
        t("jrt stream read", () -> {
            URL u = String.class.getResource("String.class");
            if (u == null) return "null";
            try (var in = u.openStream()) { return Integer.toHexString(in.readNBytes(4).length); }
        });
        t("Module.getResourceAsStream", () -> { try (var in = base.getResourceAsStream("java/lang/Object.class")) { return in != null; } });
        t("sql Module.getResourceAsStream", () -> { try (var in = sql.getResourceAsStream("java/sql/Connection.class")) { return in != null; } });
    }
}
