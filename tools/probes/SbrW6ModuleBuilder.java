// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.module.ModuleDescriptor;
import java.lang.reflect.Constructor;
import java.lang.reflect.Method;
import java.util.List;
import java.util.Set;
import java.util.TreeSet;

/** SBR w6 module: drives jdk.internal.module.Builder (reflective; needs --add-opens java.base/jdk.internal.module=ALL-UNNAMED). */
public class SbrW6ModuleBuilder {
    static Class<?> B;

    static Object st(String m, Class<?>[] t, Object... a) throws Exception {
        Method x = B.getDeclaredMethod(m, t);
        x.setAccessible(true);
        return x.invoke(null, a);
    }

    static Object run() throws Exception {
        B = Class.forName("jdk.internal.module.Builder");
        Object req0 = st("newRequires", new Class<?>[] {Set.class, String.class}, Set.of(ModuleDescriptor.Requires.Modifier.MANDATED), "java.base");
        Object req1 = st("newRequires", new Class<?>[] {Set.class, String.class, String.class}, Set.of(ModuleDescriptor.Requires.Modifier.TRANSITIVE), "m.z", "2.1");
        Object ex0 = st("newExports", new Class<?>[] {Set.class, String.class}, Set.of(), "p.a");
        Object ex1 = st("newExports", new Class<?>[] {Set.class, String.class, Set.class}, Set.of(), "p.b", Set.of("m.c", "m.d"));
        Object op0 = st("newOpens", new Class<?>[] {Set.class, String.class}, Set.of(), "p.o");
        Object op1 = st("newOpens", new Class<?>[] {Set.class, String.class, Set.class}, Set.of(), "p.q", Set.of("m.e"));
        Object pr = st("newProvides", new Class<?>[] {String.class, List.class}, "s.S", List.of("p.a.Impl"));
        Constructor<?> c = B.getDeclaredConstructor(String.class);
        c.setAccessible(true);
        Object b = c.newInstance("m.built");
        call(b, "requires", ModuleDescriptor.Requires[].class, new ModuleDescriptor.Requires[] {(ModuleDescriptor.Requires) req0, (ModuleDescriptor.Requires) req1});
        call(b, "exports", ModuleDescriptor.Exports[].class, new ModuleDescriptor.Exports[] {(ModuleDescriptor.Exports) ex0, (ModuleDescriptor.Exports) ex1});
        call(b, "opens", ModuleDescriptor.Opens[].class, new ModuleDescriptor.Opens[] {(ModuleDescriptor.Opens) op0, (ModuleDescriptor.Opens) op1});
        call(b, "provides", ModuleDescriptor.Provides[].class, new ModuleDescriptor.Provides[] {(ModuleDescriptor.Provides) pr});
        call(b, "packages", Set.class, Set.of("p.a", "p.b", "p.o", "p.q"));
        call(b, "uses", Set.class, Set.of("s.U"));
        call(b, "version", String.class, "3.4-ea");
        call(b, "mainClass", String.class, "p.a.Main");
        Method build = B.getDeclaredMethod("build", int.class);
        build.setAccessible(true);
        return build.invoke(b, 0);
    }

    static void call(Object b, String m, Class<?> t, Object a) throws Exception {
        Method x = B.getDeclaredMethod(m, t);
        x.setAccessible(true);
        x.invoke(b, a);
    }

    public static void main(String[] args) throws Exception {
        ModuleDescriptor d = (ModuleDescriptor) run();
        ModuleDescriptor d2 = (ModuleDescriptor) run();
        System.out.println("name=" + d.name() + " version=" + d.rawVersion() + " main=" + d.mainClass());
        System.out.println("requires=" + new TreeSet<>(d.requires().stream().map(Object::toString).toList()));
        System.out.println("exports=" + new TreeSet<>(d.exports().stream().map(Object::toString).toList()));
        System.out.println("opens=" + new TreeSet<>(d.opens().stream().map(Object::toString).toList()));
        System.out.println("provides=" + d.provides());
        System.out.println("uses=" + d.uses() + " packages=" + new TreeSet<>(d.packages()));
        System.out.println("equals=" + d.equals(d2) + " hash=" + (d.hashCode() == d2.hashCode()) + " cmp=" + d.compareTo(d2));
        System.out.println("exports.equals=" + d.exports().equals(d2.exports()) + " requires.equals=" + d.requires().equals(d2.requires()));
        System.out.println("q=" + d.exports().stream().filter(ModuleDescriptor.Exports::isQualified).count() + " toString=" + d);
        System.out.println("versionCmp=" + ModuleDescriptor.Version.parse("3.4-ea").compareTo(ModuleDescriptor.Version.parse("3.4")));
    }
}
