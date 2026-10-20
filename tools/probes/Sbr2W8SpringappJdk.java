import java.beans.Introspector;
import java.lang.annotation.*;
import java.lang.invoke.MethodHandles;
import java.lang.module.ModuleFinder;
import java.lang.reflect.*;
import java.util.*;
import java.util.concurrent.CopyOnWriteArraySet;

/**
 * SBR2 W8 SPRINGAPP: the JDK rows the Spring Boot sample reaches and the regression suite does not.
 * Compile with -parameters so Parameter names are observable; run --jdk-only, --compatible and HotSpot and diff.
 */
public class Sbr2W8SpringappJdk {
    static void row(String n, Object v) { System.out.println(n + " = " + v); }
    static String ex(Throwable t) { return t.getClass().getName() + ": " + t.getMessage(); }
    interface T { Object run() throws Throwable; }
    static void t(String n, T r) { try { row(n, r.run()); } catch (Throwable e) { row(n, "EX " + ex(e)); } }

    @Retention(RetentionPolicy.RUNTIME) @Target(ElementType.TYPE_USE) @interface TU { String value() default ""; }
    @Retention(RetentionPolicy.RUNTIME) @interface PA { int v(); }
    interface Gi<A, B> {}
    interface Plain {}
    static class Base<X> {}
    static class Sub extends Base<List<String>> implements Gi<String, Map<String, Integer>>, Plain {}
    static class Raw extends ArrayList {}
    static class Wild<T extends Number> extends Base<T[]> implements Comparable<Wild<T>> { public int compareTo(Wild<T> o) { return 0; } }
    static class Ctors {
        public Ctors() {}
        public Ctors(int alpha, String beta) {}
        public Ctors(@PA(v = 3) long gamma, final List<String> delta) {}
        public void m0() {}
        public void m2(int alpha, String beta) {}
        public static <Q> void mg(@PA(v = 1) Q q, int... rest) {}
        public void annotated(@TU("p") String s) {}
    }
    enum E { A, B; E() {} }
    record R(int x, String y) {}
    class Inner { Inner(int a) {} }
    static class Fld {
        public @TU("outer") Map<@TU("k") String, @TU("v") List<@TU("e") Integer>> f;
        public List<String>[] arr;
        public Map<String, ? extends Number> wild;
    }

    public static void main(String[] a) throws Throwable {
        // Introspector.decapitalize
        for (String s : new String[] { "FooBar", "URL", "", "a", "A", "X1", "aB", "Ab", "ABc", "École", "_X", "FOo" })
            t("decap[" + s + "]", () -> Introspector.decapitalize(s));
        t("decap null", () -> Introspector.decapitalize(null));

        // Class.arrayType
        t("arrayType int", () -> int.class.arrayType().getName());
        t("arrayType String", () -> String.class.arrayType().getName());
        t("arrayType String[]", () -> String[].class.arrayType().getName());
        t("arrayType void", () -> void.class.arrayType());
        t("arrayType identity", () -> String.class.arrayType() == String[].class);
        t("arrayType comp", () -> Sub.class.arrayType().getComponentType() == Sub.class);
        t("arrayType iface", () -> Plain.class.arrayType().getName());
        t("arrayType lambda name", () -> { Runnable r = () -> {}; return r.getClass().arrayType().getName().replaceAll("\\$\\$Lambda.*", "\\$\\$Lambda"); });
        t("arrayType lambda comp", () -> { Runnable r = () -> {}; return r.getClass().arrayType().getComponentType() == r.getClass(); });
        t("arrayType lambda simple", () -> { Runnable r = () -> {}; return r.getClass().arrayType().getSimpleName().replaceAll("\\$\\$Lambda.*", "\\$\\$Lambda"); });
        t("arrayType proxy", () -> { Object p = Proxy.newProxyInstance(Plain.class.getClassLoader(), new Class<?>[] { Plain.class }, (x, m, z) -> null); return p.getClass().arrayType().getComponentType() == p.getClass(); });
        t("arrayType hidden newInstance", () -> { Runnable r = () -> {}; return java.lang.reflect.Array.newInstance(r.getClass(), 2).getClass() == r.getClass().arrayType(); });
        t("arrayType 255", () -> { Class<?> c = int.class; for (int i = 0; i < 255; i++) c = c.arrayType(); return c.getName().length(); });
        t("arrayType 256", () -> { Class<?> c = int.class; for (int i = 0; i < 256; i++) c = c.arrayType(); return c; });

        // getGenericSuperclass / getGenericInterfaces
        for (Class<?> c : new Class<?>[] { Sub.class, Raw.class, Wild.class, Object.class, Plain.class, int.class, String[].class, Base.class, E.class, R.class,
                ArrayList.class, HashMap.class, Integer.class, Enum.class, Comparable.class, List.class, Fld.class, Thread.class, CopyOnWriteArraySet.class }) {
            t("gsuper " + c.getName(), () -> String.valueOf(c.getGenericSuperclass()));
            t("gifaces " + c.getName(), () -> Arrays.toString(c.getGenericInterfaces()));
        }
        t("gsuper sub cls", () -> ((ParameterizedType) Sub.class.getGenericSuperclass()).getActualTypeArguments()[0].getClass().getInterfaces().length);
        t("gsuper sub owner", () -> String.valueOf(((ParameterizedType) Sub.class.getGenericSuperclass()).getOwnerType()));
        t("gsuper sub raw", () -> ((ParameterizedType) Sub.class.getGenericSuperclass()).getRawType());
        t("gifaces same", () -> Sub.class.getGenericInterfaces()[0].equals(Sub.class.getGenericInterfaces()[0]));
        t("gifaces clone", () -> Sub.class.getGenericInterfaces() != Sub.class.getGenericInterfaces());
        t("lambda gifaces", () -> { Runnable r = () -> {}; return Arrays.toString(r.getClass().getGenericInterfaces()) + r.getClass().getGenericSuperclass(); });
        t("anon gsuper", () -> new ArrayList<String>() {}.getClass().getGenericSuperclass());
        t("anon giface", () -> new Comparable<Integer>() { public int compareTo(Integer o) { return 0; } }.getClass().getGenericInterfaces()[0]);
        t("proxy gifaces", () -> Arrays.toString(Proxy.newProxyInstance(Sbr2W8SpringappJdk.class.getClassLoader(), new Class<?>[] { Plain.class }, (p, m, x) -> null).getClass().getGenericInterfaces()));

        // Class.getResource
        t("res String.class", () -> String.class.getResource("String.class") != null);
        t("res abs Object", () -> String.valueOf(Object.class.getResource("/java/lang/Object.class")).replaceAll("^.*!", "!"));
        t("res missing", () -> Sbr2W8SpringappJdk.class.getResource("nosuchfile.xyz"));
        t("res null", () -> Sbr2W8SpringappJdk.class.getResource(null));
        t("res empty", () -> Sbr2W8SpringappJdk.class.getResource("") != null);
        t("res own class", () -> String.valueOf(Sbr2W8SpringappJdk.class.getResource("Sbr2W8SpringappJdk.class")).replaceAll("^.*/", "<>/"));
        t("res own abs", () -> String.valueOf(Sbr2W8SpringappJdk.class.getResource("/Sbr2W8SpringappJdk.class")).replaceAll("^.*/", "<>/"));
        t("res array", () -> String[].class.getResource("String.class") != null);
        t("res prim", () -> int.class.getResource("int.class"));
        t("res module-info", () -> String.valueOf(Object.class.getResource("/module-info.class")).replaceAll("^.*!", "!"));
        t("res pkg dir", () -> String.valueOf(Object.class.getResource("/java/lang")).replaceAll("^.*!", "!"));
        t("res stream", () -> { try (var in = Object.class.getResourceAsStream("/java/lang/Object.class")) { return in.readNBytes(4).length; } });
        t("res url proto", () -> Object.class.getResource("/java/lang/Object.class").getProtocol());
        t("res loader", () -> String.valueOf(Sbr2W8SpringappJdk.class.getClassLoader().getResource("Sbr2W8SpringappJdk.class") != null));
        t("res list", () -> { Enumeration<java.net.URL> e = Sbr2W8SpringappJdk.class.getClassLoader().getResources("Sbr2W8SpringappJdk.class"); int n = 0; while (e.hasMoreElements()) { e.nextElement(); n++; } return n; });

        // ModuleLayer.configuration
        t("boot layer", () -> ModuleLayer.boot() == ModuleLayer.boot());
        t("boot cfg", () -> ModuleLayer.boot().configuration() != null);
        t("boot cfg java.base", () -> ModuleLayer.boot().configuration().findModule("java.base").isPresent());
        t("boot cfg size>20", () -> ModuleLayer.boot().configuration().modules().size() > 20);
        t("boot cfg parents", () -> ModuleLayer.boot().configuration().parents().size());
        t("boot cfg module name", () -> ModuleLayer.boot().configuration().findModule("java.base").get().name());
        t("boot cfg layer parents", () -> ModuleLayer.boot().parents().size());
        t("boot cfg same", () -> ModuleLayer.boot().configuration() == ModuleLayer.boot().configuration());
        t("boot cfg toString", () -> ModuleLayer.boot().configuration().toString().length() > 0);
        t("boot cfg resolved", () -> ModuleLayer.boot().configuration().findModule("java.base").get().reads().size());
        t("boot cfg ref loc", () -> ModuleLayer.boot().configuration().findModule("java.base").get().reference().descriptor().name());
        t("layer empty", () -> ModuleLayer.empty().configuration().modules().size());
        t("layer modules java.base", () -> ModuleLayer.boot().findModule("java.base").isPresent());
        t("layer module missing", () -> ModuleLayer.boot().findModule("nosuch.mod").isPresent());
        t("layer modules size>20", () -> ModuleLayer.boot().modules().size() > 20);
        t("layer loader", () -> ModuleLayer.boot().findLoader("java.base"));
        t("layer loader missing", () -> ModuleLayer.boot().findLoader("nosuch.mod"));

        // System.console / getSecurityManager
        t("console class", () -> { var c = System.console(); return c == null ? "null" : c.getClass().getName(); });
        t("console terminal", () -> { var c = System.console(); return c == null ? "null" : c.isTerminal(); });
        t("console same", () -> System.console() == System.console());
        t("console charset", () -> { var c = System.console(); return c == null ? "null" : c.charset().name().length() > 0; });
        t("secmgr", () -> System.getSecurityManager());
        t("secmgr set", () -> { System.setSecurityManager(new SecurityManager()); return "set"; });
        t("secmgr after", () -> System.getSecurityManager());

        // Lookup.findClass
        var L = MethodHandles.lookup();
        t("findClass String", () -> L.findClass("java.lang.String"));
        t("findClass own", () -> L.findClass("Sbr2W8SpringappJdk"));
        t("findClass inner", () -> L.findClass("Sbr2W8SpringappJdk$Sub"));
        t("findClass missing", () -> L.findClass("no.such.Clazz"));
        t("findClass null", () -> L.findClass(null));
        t("findClass array", () -> L.findClass("[I"));
        t("findClass array obj", () -> L.findClass("[Ljava.lang.String;"));
        t("findClass prim", () -> L.findClass("int"));
        t("findClass empty", () -> L.findClass(""));
        t("findClass slash", () -> L.findClass("java/lang/String"));
        t("findClass inaccessible", () -> L.findClass("jdk.internal.misc.Unsafe"));
        t("findClass private", () -> L.findClass("java.lang.ClassLoaderHelper"));
        t("findClass public lookup", () -> MethodHandles.publicLookup().findClass("java.util.ArrayList"));
        t("findClass public lookup own", () -> MethodHandles.publicLookup().findClass("Sbr2W8SpringappJdk"));
        t("findClass init", () -> { Class<?> c = L.findClass("Sbr2W8SpringappJdk$Lazy"); return Lazy.inited + "," + c.getSimpleName(); });
        t("findClass loader", () -> L.findClass("java.lang.String").getClassLoader());
        t("findClass dotted inner", () -> L.findClass("Sbr2W8SpringappJdk.Sub"));

        // Constructor/Method.getParameters, Parameter.isNamePresent
        Constructor<?>[] cs = Ctors.class.getDeclaredConstructors(); Arrays.sort(cs, Comparator.comparing(Constructor::toString));
        for (Constructor<?> c : cs) {
            String s = c.toString();
            t("ctor params " + s, () -> paramStr(c.getParameters()));
        }
        for (Method m : sortedMethods(Ctors.class)) t("method params " + m.getName(), () -> paramStr(m.getParameters()));
        t("params enum ctor", () -> paramStr(E.class.getDeclaredConstructors()[0].getParameters()));
        t("params record ctor", () -> paramStr(R.class.getDeclaredConstructors()[0].getParameters()));
        t("params inner ctor", () -> paramStr(Inner.class.getDeclaredConstructors()[0].getParameters()));
        t("params lambda", () -> { Runnable r = () -> {}; return r.getClass().getDeclaredMethods().length; });
        t("params identity", () -> { Method m = Ctors.class.getMethod("m2", int.class, String.class); return m.getParameters() != m.getParameters() && m.getParameters()[0].equals(m.getParameters()[0]); });
        t("params String.valueOf", () -> paramStr(String.class.getMethod("valueOf", int.class).getParameters()));
        t("params ArrayList.add", () -> paramStr(ArrayList.class.getMethod("add", int.class, Object.class).getParameters()));
        t("params jdk ctor", () -> paramStr(ArrayList.class.getConstructor(int.class).getParameters()));
        t("params atypes", () -> { Parameter p = Ctors.class.getMethod("annotated", String.class).getParameters()[0]; return p.getAnnotatedType().getAnnotations().length + "," + p.getParameterizedType(); });
        t("params gen", () -> { Parameter p = Ctors.class.getDeclaredConstructors().length > 0 ? Arrays.stream(Ctors.class.getDeclaredConstructors()).filter(c -> c.getParameterCount() == 2 && c.getParameterTypes()[0] == long.class).findFirst().get().getParameters()[1] : null; return p.getParameterizedType() + "," + p.getModifiers() + "," + p.isImplicit() + "," + p.isSynthetic() + "," + p.isVarArgs(); });
        t("params vararg", () -> { Parameter p = Ctors.class.getMethod("mg", Object.class, int[].class).getParameters()[1]; return p.isVarArgs() + "," + p.getName() + "," + p.getType().getName(); });
        t("params pannot", () -> { Parameter p = Ctors.class.getMethod("mg", Object.class, int[].class).getParameters()[0]; return p.getAnnotations().length + "," + p.getAnnotation(PA.class); });
        t("params exec", () -> Ctors.class.getMethod("m2", int.class, String.class).getParameters()[0].getDeclaringExecutable().getName());
        t("params toString", () -> Ctors.class.getMethod("m2", int.class, String.class).getParameters()[1].toString());
        t("params hash eq", () -> { Method m = Ctors.class.getMethod("m2", int.class, String.class); return m.getParameters()[0].hashCode() == m.getParameters()[0].hashCode(); });
        t("params proxy", () -> paramStr(Plain.class.getDeclaredMethods().length > 0 ? Plain.class.getDeclaredMethods()[0].getParameters() : new Parameter[0]));
        t("params List.add", () -> paramStr(List.class.getMethod("add", Object.class).getParameters()));
        t("params main", () -> paramStr(Sbr2W8SpringappJdk.class.getMethod("main", String[].class).getParameters()));
        t("Parameter.isNamePresent fresh", () -> Sbr2W8SpringappJdk.class.getMethod("main", String[].class).getParameters()[0].isNamePresent());
        t("params count", () -> Ctors.class.getMethod("m2", int.class, String.class).getParameterCount());

        // AnnotatedParameterizedType.getAnnotatedActualTypeArguments
        t("apt outer", () -> { AnnotatedParameterizedType t = (AnnotatedParameterizedType) Fld.class.getField("f").getAnnotatedType(); return aStr(t) + " || " + aStr(t.getAnnotatedActualTypeArguments()[0]) + " || " + aStr(t.getAnnotatedActualTypeArguments()[1]); });
        t("apt nested", () -> { AnnotatedParameterizedType t = (AnnotatedParameterizedType) ((AnnotatedParameterizedType) Fld.class.getField("f").getAnnotatedType()).getAnnotatedActualTypeArguments()[1]; return aStr(t.getAnnotatedActualTypeArguments()[0]); });
        t("apt len", () -> ((AnnotatedParameterizedType) Fld.class.getField("f").getAnnotatedType()).getAnnotatedActualTypeArguments().length);
        t("apt clone", () -> { var t = (AnnotatedParameterizedType) Fld.class.getField("f").getAnnotatedType(); return t.getAnnotatedActualTypeArguments() != t.getAnnotatedActualTypeArguments(); });
        t("apt array", () -> { var t = Fld.class.getField("arr").getAnnotatedType(); return t.getClass().getInterfaces()[0].getSimpleName() + "," + aStr(((AnnotatedArrayType) t).getAnnotatedGenericComponentType()); });
        t("apt wild", () -> { var t = (AnnotatedParameterizedType) Fld.class.getField("wild").getAnnotatedType(); return t.getAnnotatedActualTypeArguments()[1].getType() + "," + t.getAnnotatedActualTypeArguments()[1].getClass().getInterfaces()[0].getSimpleName(); });
        t("apt super", () -> { var t = (AnnotatedParameterizedType) Sub.class.getAnnotatedSuperclass(); return aStr(t.getAnnotatedActualTypeArguments()[0]); });
        t("apt ifaces", () -> { var t = Sub.class.getAnnotatedInterfaces(); return t.length + "," + ((AnnotatedParameterizedType) t[0]).getAnnotatedActualTypeArguments().length; });
        t("apt retval", () -> { var t = (AnnotatedParameterizedType) Fld.class.getField("f").getAnnotatedType(); return t.getAnnotatedActualTypeArguments()[0].getType() + "," + t.getType(); });
        t("apt toString", () -> Fld.class.getField("f").getAnnotatedType().toString());
        t("apt equals", () -> Fld.class.getField("f").getAnnotatedType().equals(Fld.class.getField("f").getAnnotatedType()));

        // CopyOnWriteArraySet
        t("cowas new", () -> new CopyOnWriteArraySet<String>().isEmpty());
        t("cowas iterator empty", () -> new CopyOnWriteArraySet<String>().iterator().hasNext());
        t("cowas add", () -> { var s = new CopyOnWriteArraySet<String>(); return s.add("a") + "," + s.add("a") + "," + s.add("b") + "," + s; });
        t("cowas iter snapshot", () -> { var s = new CopyOnWriteArraySet<>(List.of("a", "b")); var it = s.iterator(); s.add("c"); StringBuilder sb = new StringBuilder(); while (it.hasNext()) sb.append(it.next()); return sb + "," + s.size(); });
        t("cowas iter remove", () -> { var it = new CopyOnWriteArraySet<>(List.of("a")).iterator(); it.next(); it.remove(); return "no"; });
        t("cowas isEmpty after", () -> { var s = new CopyOnWriteArraySet<String>(); s.add("x"); boolean e1 = s.isEmpty(); s.remove("x"); return e1 + "," + s.isEmpty(); });
        t("cowas class", () -> new CopyOnWriteArraySet<String>().iterator().getClass().getName());
        t("cowas sub", () -> { class S extends CopyOnWriteArraySet<String> { int n; @Override public boolean isEmpty() { n++; return super.isEmpty(); } @Override public Iterator<String> iterator() { n += 10; return super.iterator(); } } var s = new S(); s.isEmpty(); s.iterator(); s.addAll(List.of("a", "b")); return s.n + "," + s; });
        t("cowas null", () -> { var s = new CopyOnWriteArraySet<String>(); s.add(null); return s.contains(null) + "," + s.iterator().next(); });
        t("cowas threads", () -> { var s = new CopyOnWriteArraySet<Integer>(); Thread[] ts = new Thread[4]; for (int i = 0; i < 4; i++) { int b = i; ts[i] = new Thread(() -> { for (int k = 0; k < 500; k++) s.add(b * 1000 + k % 100); }); ts[i].start(); } for (Thread th : ts) th.join(); return s.size(); });
        t("cowas listeners", () -> { var s = new CopyOnWriteArraySet<Runnable>(); int[] n = { 0 }; for (int i = 0; i < 3; i++) s.add(() -> n[0]++); for (Runnable r : s) r.run(); return n[0]; });
        t("cowas spliterator", () -> new CopyOnWriteArraySet<>(List.of(1, 2, 3)).stream().mapToInt(i -> i).sum());
        t("cowas equals", () -> new CopyOnWriteArraySet<>(List.of(1, 2)).equals(Set.of(1, 2)));
        t("cowas removeIf", () -> { var s = new CopyOnWriteArraySet<>(List.of(1, 2, 3, 4)); s.removeIf(i -> i % 2 == 0); return s; });
        t("cowas copy ctor", () -> new CopyOnWriteArraySet<>(List.of(1, 1, 2)).size());
        t("cowas serial", () -> { var bo = new java.io.ByteArrayOutputStream(); try (var o = new java.io.ObjectOutputStream(bo)) { o.writeObject(new CopyOnWriteArraySet<>(List.of("q", "r"))); } try (var i = new java.io.ObjectInputStream(new java.io.ByteArrayInputStream(bo.toByteArray()))) { return i.readObject(); } });

        // SystemModuleFinder.findAll
        t("finder class", () -> ModuleFinder.ofSystem().getClass().getName());
        t("finder findAll", () -> ModuleFinder.ofSystem().findAll().size() > 20);
        t("finder findAll same", () -> ModuleFinder.ofSystem().findAll() == ModuleFinder.ofSystem().findAll());
        t("finder findAll names", () -> { var s = new TreeSet<String>(); ModuleFinder.ofSystem().findAll().forEach(r -> s.add(r.descriptor().name())); return s.contains("java.base") + "," + s.contains("java.logging") + "," + s.contains("jdk.compiler") + "," + s.first(); });
        t("finder find", () -> ModuleFinder.ofSystem().find("java.base").isPresent());
        t("finder find none", () -> ModuleFinder.ofSystem().find("nosuch").isPresent());
        t("finder unmod", () -> { try { ModuleFinder.ofSystem().findAll().clear(); return "mutable"; } catch (UnsupportedOperationException e) { return "unmod"; } });
        t("finder count", () -> ModuleFinder.ofSystem().findAll().size());
        t("finder location", () -> ModuleFinder.ofSystem().find("java.base").get().location().get().getScheme());
        t("finder ref descriptor", () -> ModuleFinder.ofSystem().find("java.logging").get().descriptor().exports().size() > 0);
        t("finder compose", () -> ModuleFinder.compose(ModuleFinder.ofSystem(), ModuleFinder.of()).findAll().size() > 20);
        t("finder open", () -> { try (var r = ModuleFinder.ofSystem().find("java.base").get().open()) { return r.find("java/lang/Object.class").isPresent(); } });
        t("finder configuration", () -> java.lang.module.Configuration.resolve(ModuleFinder.ofSystem(), List.of(java.lang.module.Configuration.empty()), ModuleFinder.of(), Set.of("java.logging")).modules().size() > 0);
    }

    static class Lazy { static int inited = 7; static { inited = 8; } }

    static List<Method> sortedMethods(Class<?> c) {
        List<Method> l = new ArrayList<>(Arrays.asList(c.getDeclaredMethods()));
        l.sort(Comparator.comparing(Method::getName));
        return l;
    }
    static String paramStr(Parameter[] ps) {
        StringBuilder sb = new StringBuilder("[");
        for (Parameter p : ps) sb.append(p.getName()).append(':').append(p.isNamePresent()).append(':').append(p.getModifiers()).append(':').append(p.getType().getSimpleName()).append(' ');
        return sb.append(']').toString();
    }
    static String aStr(AnnotatedType t) { return t.getType() + Arrays.toString(t.getAnnotations()); }
}
