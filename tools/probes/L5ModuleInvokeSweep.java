// L5 residual sweep: the two items HANDOFF-20260828-L5-reflection.md leaves open --
// `Module.getPackages()` for java.base, and `invokeExact`'s exact-signature rule.
//
// Hygiene: stdout only, no identity hashes, no timing, no set ORDER printed
// (`getPackages()` returns an unordered Set and both VMs may order it freely --
// so every row is a size bucket, a membership test, or a shape).
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.Set;

public class L5ModuleInvokeSweep {
    interface Body { Object run() throws Throwable; }

    public static class Box {
        public int v;
        public static int S = 5;
        public Box(int v) { this.v = v; }
    }

    static void t(String tag, Body b) {
        String v;
        try {
            Object o = b.run();
            v = String.valueOf(o);
        } catch (Throwable e) {
            v = "throws " + e.getClass().getName();
        }
        System.out.println(tag + " = " + v);
    }

    public static void main(String[] a) throws Throwable {
        modulePackages();
        invokeExact();
        invokeCoercing();
        System.out.println("DONE");
    }

    // ---------------- Module.getPackages ----------------

    static void modulePackages() {
        Module base = Object.class.getModule();
        t("base.getName", () -> base.getName());
        t("base.isNamed", () -> base.isNamed());
        t("base.pkgs.nonNull", () -> base.getPackages() != null);
        t("base.pkgs.size>100", () -> base.getPackages().size() > 100);
        t("base.pkgs.size>200", () -> base.getPackages().size() > 200);
        // Public API packages every java.base ships.
        for (String p : new String[] {
            "java.lang", "java.util", "java.io", "java.net", "java.nio",
            "java.lang.invoke", "java.lang.reflect", "java.lang.annotation",
            "java.util.concurrent", "java.util.concurrent.atomic",
            "java.util.function", "java.util.stream", "java.util.regex",
            "java.text", "java.time", "java.time.format", "java.math",
            "java.security", "java.security.cert", "java.nio.charset",
            "java.nio.file", "java.nio.channels", "java.lang.module",
            "java.lang.ref", "java.util.jar", "java.util.zip",
        }) {
            t("base.pkgs.has." + p, () -> base.getPackages().contains(p));
        }
        // java.base internals -- exported to nobody, but still ITS packages.
        for (String p : new String[] {
            "jdk.internal.misc", "jdk.internal.loader", "jdk.internal.reflect",
            "sun.nio.ch", "sun.security.util",
        }) {
            t("base.pkgs.has." + p, () -> base.getPackages().contains(p));
        }
        // Packages that belong to OTHER modules, or to nothing.
        for (String p : new String[] {
            "java.sql", "java.awt", "javax.xml.parsers", "org.w3c.dom",
            "java.util.logging", "no.such.package", "",
        }) {
            t("base.pkgs.hasNot." + p, () -> !base.getPackages().contains(p));
        }
        // Shape of the returned set.
        t("base.pkgs.immutable", () -> {
            base.getPackages().add("x.y");
            return "MUTATED";
        });
        t("base.pkgs.noSlashes", () -> {
            for (String p : base.getPackages()) {
                if (p.indexOf('/') >= 0) return "SLASHED:" + p;
                if (p.isEmpty()) return "EMPTY-NAME";
            }
            return true;
        });
        t("base.pkgs.stable", () -> base.getPackages().size() == base.getPackages().size());
        t("base.pkgs.hasDescriptorPkgs", () -> {
            Set<String> fromDescriptor = base.getDescriptor().packages();
            return base.getPackages().containsAll(fromDescriptor);
        });
        t("base.desc.pkgs.size>100", () -> base.getDescriptor().packages().size() > 100);
        t("base.desc.pkgs.has.java.lang", () -> base.getDescriptor().packages().contains("java.lang"));
        t("base.pkgs.matchesDescriptor",
            () -> base.getPackages().equals(base.getDescriptor().packages()));

        // java.base's exports are a SUBSET of its packages, and a package the
        // module does not contain cannot be exported.
        t("base.exports.subsetOfPkgs", () -> {
            Set<String> pkgs = base.getPackages();
            int missing = 0;
            for (var e : base.getDescriptor().exports()) {
                if (!pkgs.contains(e.source())) missing++;
            }
            return "missingExports=" + missing;
        });
        t("base.pkgs.vs.desc.onlyInDesc", () -> {
            Set<String> p = base.getPackages();
            int n = 0;
            for (String d : base.getDescriptor().packages()) if (!p.contains(d)) n++;
            return n == 0 ? "0" : (n > 0 ? "nonzero" : "?");
        });
        t("base.pkgs.vs.desc.onlyInPkgs", () -> {
            Set<String> d = base.getDescriptor().packages();
            int n = 0;
            for (String q : base.getPackages()) if (!d.contains(q)) n++;
            return n == 0 ? "0" : "nonzero";
        });
        t("base.desc.pkgs.has.jdk.internal.loader",
            () -> base.getDescriptor().packages().contains("jdk.internal.loader"));
        t("base.desc.pkgs.immutable", () -> {
            base.getDescriptor().packages().add("x.y");
            return "MUTATED";
        });
        // Every ModuleDescriptor collection accessor is immutable on HotSpot.
        t("base.desc.uses.immutable", () -> { base.getDescriptor().uses().add("x"); return "MUTATED"; });
        t("base.desc.exports.immutable", () -> { base.getDescriptor().exports().clear(); return "MUTATED"; });
        t("base.desc.opens.immutable", () -> { base.getDescriptor().opens().clear(); return "MUTATED"; });
        t("base.desc.requires.immutable", () -> { base.getDescriptor().requires().clear(); return "MUTATED"; });
        t("base.desc.provides.immutable", () -> { base.getDescriptor().provides().clear(); return "MUTATED"; });
        t("base.pkgs.iterRemove", () -> {
            var it = base.getPackages().iterator();
            it.next();
            it.remove();
            return "REMOVED";
        });
        t("base.isExported.java.lang", () -> base.isExported("java.lang"));
        t("base.isExported.jdk.internal.misc", () -> base.isExported("jdk.internal.misc"));

        // The UNNAMED module: getPackages() is the loader's package set, which
        // both VMs populate lazily -- so only the shape is asserted.
        Module unnamed = L5ModuleInvokeSweep.class.getModule();
        t("unnamed.isNamed", () -> unnamed.isNamed());
        t("unnamed.pkgs.nonNull", () -> unnamed.getPackages() != null);
        t("unnamed.pkgs.noNulls", () -> !unnamed.getPackages().contains(null));

        // Another named module reached through the boot layer.
        t("java.logging.pkgs.has.java.util.logging", () -> {
            var m = ModuleLayer.boot().findModule("java.logging");
            if (m.isEmpty()) return "ABSENT";
            return m.get().getPackages().contains("java.util.logging");
        });
        t("java.logging.pkgs.hasNot.java.lang", () -> {
            var m = ModuleLayer.boot().findModule("java.logging");
            if (m.isEmpty()) return "ABSENT";
            return !m.get().getPackages().contains("java.lang");
        });
    }

    // ---------------- invokeExact: NO conversion ----------------

    static void invokeExact() throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodHandle max = l.findStatic(Math.class, "max",
            MethodType.methodType(int.class, int.class, int.class));
        MethodHandle len = l.findVirtual(String.class, "length",
            MethodType.methodType(int.class));
        MethodHandle cat = l.findVirtual(String.class, "concat",
            MethodType.methodType(String.class, String.class));

        t("ie.max.exact", () -> (int) max.invokeExact(1, 2));
        t("ie.max.longArgs", () -> (int) max.invokeExact(1L, 2L));
        t("ie.max.shortArgs", () -> (int) max.invokeExact((short) 1, (short) 2));
        t("ie.max.byteArgs", () -> (int) max.invokeExact((byte) 1, (byte) 2));
        t("ie.max.charArgs", () -> (int) max.invokeExact('a', 'b'));
        t("ie.max.boxedArgs", () -> (int) max.invokeExact(Integer.valueOf(1), Integer.valueOf(2)));
        t("ie.max.mixedArgs", () -> (int) max.invokeExact(1, 2L));
        t("ie.max.retLong", () -> (long) max.invokeExact(1, 2));
        t("ie.max.retObject", () -> (Object) max.invokeExact(1, 2));
        t("ie.max.retInteger", () -> (Integer) max.invokeExact(1, 2));
        t("ie.max.retVoid", () -> { max.invokeExact(1, 2); return "no-throw"; });
        t("ie.max.tooFewArgs", () -> (int) max.invokeExact(1));
        t("ie.max.tooManyArgs", () -> (int) max.invokeExact(1, 2, 3));

        t("ie.len.exact", () -> (int) len.invokeExact("abcd"));
        t("ie.len.objRecv", () -> (int) len.invokeExact((Object) "abcd"));
        t("ie.len.csRecv", () -> (int) len.invokeExact((CharSequence) "abcd"));
        t("ie.len.nullRecv", () -> (int) len.invokeExact((String) null));

        t("ie.cat.exact", () -> (String) cat.invokeExact("ab", "cd"));
        t("ie.cat.retCharSequence", () -> (CharSequence) cat.invokeExact("ab", "cd"));
        t("ie.cat.retObject", () -> (Object) cat.invokeExact("ab", "cd"));
        t("ie.cat.objArg", () -> (String) cat.invokeExact("ab", (Object) "cd"));
        t("ie.cat.nullArg", () -> (String) cat.invokeExact("ab", (String) null));

        // asType() moves the handle's own type; then the SAME call site is exact.
        t("ie.asType.narrowArgs", () ->
            max.asType(MethodType.methodType(long.class, long.class, long.class)).type().toString());
        MethodHandle maxL = max.asType(MethodType.methodType(long.class, int.class, int.class));
        t("ie.maxAsLong.exact", () -> (long) maxL.invokeExact(1, 2));
        t("ie.maxAsLong.intRet", () -> (int) maxL.invokeExact(1, 2));
        t("ie.maxAsLong.longArgs", () -> (long) maxL.invokeExact(1L, 2L));

        MethodHandle bound = cat.bindTo("ab");
        t("ie.bound.exact", () -> (String) bound.invokeExact("cd"));
        t("ie.bound.extraRecv", () -> (String) bound.invokeExact("ab", "cd"));

        // Field accessors and a constructor: `type()` on an INSTANCE accessor
        // names the receiver on HotSpot, and the exact rule is checked against
        // whatever `type()` says, so these two rows pin both at once.
        MethodHandle getter = l.findGetter(Box.class, "v", int.class);
        MethodHandle setter = l.findSetter(Box.class, "v", int.class);
        MethodHandle sget = l.findStaticGetter(Box.class, "S", int.class);
        MethodHandle ctor = l.findConstructor(Box.class, MethodType.methodType(void.class, int.class));
        Box b = new Box(3);
        t("ie.getter.exact", () -> (int) getter.invokeExact(b));
        t("ie.getter.noRecv", () -> (int) getter.invokeExact());
        t("ie.getter.longRet", () -> (long) getter.invokeExact(b));
        t("ie.setter.exact", () -> { setter.invokeExact(b, 9); return b.v; });
        t("ie.sget.exact", () -> (int) sget.invokeExact());
        t("ie.sget.extraArg", () -> (int) sget.invokeExact(b));
        t("ie.ctor.exact", () -> ((Box) ctor.invokeExact(4)).v);
        t("ie.ctor.objRet", () -> ((Object) ctor.invokeExact(4)).getClass().getSimpleName());
        t("ie.type.getter", () -> getter.type().toString());
        t("ie.type.setter", () -> setter.type().toString());
        t("ie.type.sget", () -> sget.type().toString());
        t("ie.type.ctor", () -> ctor.type().toString());
        t("ie.type.len", () -> len.type().toString());
        t("ie.type.max", () -> max.type().toString());
        t("ie.type.maxAsLong", () -> maxL.type().toString());
        t("ie.type.bound", () -> bound.type().toString());
    }

    // ---------------- invoke: conversion IS allowed (the control) ----------------

    static void invokeCoercing() throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodHandle max = l.findStatic(Math.class, "max",
            MethodType.methodType(int.class, int.class, int.class));
        MethodHandle cat = l.findVirtual(String.class, "concat",
            MethodType.methodType(String.class, String.class));

        t("iv.max.exact", () -> (int) max.invoke(1, 2));
        t("iv.max.byteArgs", () -> (int) max.invoke((byte) 1, (byte) 2));
        t("iv.max.shortArgs", () -> (int) max.invoke((short) 1, (short) 2));
        t("iv.max.boxedArgs", () -> (int) max.invoke(Integer.valueOf(1), Integer.valueOf(2)));
        t("iv.max.longArgs", () -> (int) max.invoke(1L, 2L));
        t("iv.max.retLong", () -> (long) max.invoke(1, 2));
        t("iv.max.retObject", () -> (Object) max.invoke(1, 2));
        t("iv.max.retVoid", () -> { max.invoke(1, 2); return "no-throw"; });
        t("iv.max.tooFewArgs", () -> (int) max.invoke(1));
        t("iv.cat.objArg", () -> (String) cat.invoke("ab", (Object) "cd"));
        t("iv.cat.badObjArg", () -> (String) cat.invoke("ab", (Object) Integer.valueOf(3)));
        t("iv.cat.retCharSequence", () -> (CharSequence) cat.invoke("ab", "cd"));
    }
}
