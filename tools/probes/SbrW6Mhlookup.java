// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandleInfo;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodHandles.Lookup;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.List;
import java.util.function.Supplier;

/**
 * SBR wave 6, lane {@code mhlookup}: the {@code java.lang.invoke} rows the
 * int12 census found native-won -- {@code MethodHandles.lookup/publicLookup/
 * privateLookupIn}, {@code Lookup.lookupClass/lookupModes/in/ensureInitialized/
 * find*}, {@code MethodType.methodType/parameterType}, {@code MemberName}
 * accessors (reached through {@code revealDirect}), {@code MethodHandle.type}.
 *
 * <p>Every row is evaluated ONCE and printed (the first-run value), then
 * re-evaluated in a hot loop and compared with that value; {@code drift}
 * counts mismatches, so cold == warm is asserted. Diff the whole output against
 * HotSpot 25. Nothing printed depends on identity hashes.
 *
 * <pre>
 *   java SbrW6Mhlookup [iters]            (HotSpot oracle)
 *   cratonvm -cp . SbrW6Mhlookup [iters]
 * </pre>
 */
public class SbrW6Mhlookup {
    static final List<String> LABELS = new ArrayList<>();
    static final List<Supplier<String>> ROWS = new ArrayList<>();

    interface X {
        Object get() throws Throwable;
    }

    /** Comma-separated labels (args[1]) to leave out: rows that take the VM down on a build under test. */
    static java.util.Set<String> SKIP = new java.util.HashSet<>();

    static void rowx(String label, X x) {
        if (SKIP.contains(label)) {
            return;
        }
        LABELS.add(label);
        ROWS.add(() -> {
            try {
                return String.valueOf(x.get());
            } catch (Throwable t) {
                return fmt(t);
            }
        });
    }

    /** Throwable -> "!Class: message", hash-free; implicit-exception messages (AIOOBE, NPE, CCE) vary with fast-throw so are dropped. */
    static String fmt(Throwable t) {
        if (t instanceof ArrayIndexOutOfBoundsException || t instanceof NullPointerException
                || t instanceof ClassCastException) {
            return "!" + t.getClass().getName();
        }
        return ("!" + t.getClass().getName() + ": " + t.getMessage()).replaceAll("@[0-9a-f]+", "@H");
    }

    static String safe(Supplier<String> s) {
        try {
            return s.get();
        } catch (Throwable t) {
            return fmt(t);
        }
    }

    public static int inc(int x) { return x + 1; }
    public int f = 5;
    private int priv = 6;
    public static int sf = 7;
    public SbrW6Mhlookup() {}
    public SbrW6Mhlookup(int v) { f = v; }
    public String hello(String s) { return "hi " + s; }
    private static String secret() { return "s"; }
    static String pkg() { return "p"; }

    static class Nested {
        static Lookup mine() { return MethodHandles.lookup(); }
        static int initCount;
        static { initCount++; }
    }

    static class LazyInit {
        static int n;
        static { n = 42; }
    }

    public static class Pub {}

    static Lookup viaReflection() throws Throwable {
        Method m = MethodHandles.class.getMethod("lookup");
        return (Lookup) m.invoke(null);
    }

    static Lookup viaLambda() {
        Supplier<Lookup> s = MethodHandles::lookup;
        return s.get();
    }

    static Lookup viaMh() throws Throwable {
        MethodHandle mh = MethodHandles.publicLookup().findStatic(MethodHandles.class, "lookup",
                MethodType.methodType(Lookup.class));
        return (Lookup) mh.invoke();
    }

    static String info(MethodHandleInfo i) {
        return i.getDeclaringClass().getSimpleName() + " " + i.getName() + " " + i.getMethodType() + " "
                + i.getReferenceKind() + " " + i.getModifiers() + " " + i;
    }

    public static void main(String[] args) throws Exception {
        if (args.length > 1) {
            SKIP.addAll(java.util.Arrays.asList(args[1].split(",")));
        }
        final Lookup L = MethodHandles.lookup();
        // ------------------------------------------------ MethodHandles.lookup family
        rowx("lookup class", () -> MethodHandles.lookup().lookupClass().getName());
        rowx("lookup modes", () -> MethodHandles.lookup().lookupModes());
        rowx("lookup prevLookupClass", () -> MethodHandles.lookup().previousLookupClass());
        rowx("lookup toString", () -> MethodHandles.lookup().toString());
        rowx("lookup hasFullPrivilegeAccess", () -> MethodHandles.lookup().hasFullPrivilegeAccess());
        rowx("lookup hasPrivateAccess", () -> MethodHandles.lookup().hasPrivateAccess());
        rowx("lookup in nested", () -> Nested.mine().lookupClass().getName() + " " + Nested.mine().lookupModes());
        rowx("lookup via reflection", () -> viaReflection().lookupClass().getName());
        rowx("lookup via reflection modes", () -> viaReflection().lookupModes());
        rowx("lookup via lambda", () -> viaLambda().lookupClass().getName().replaceAll("0x[0-9a-f]+", "0xH"));
        rowx("lookup via mh", () -> viaMh().lookupClass().getName());
        rowx("lookup via mh modes", () -> viaMh().lookupModes());
        rowx("lookup via thread", () -> {
            Lookup[] r = new Lookup[1];
            Thread t = new Thread(() -> r[0] = MethodHandles.lookup());
            t.start();
            t.join();
            return r[0].lookupClass().getName() + " " + r[0].lookupModes();
        });
        rowx("lookup distinct objs", () -> MethodHandles.lookup() != MethodHandles.lookup());
        rowx("lookup equals-class", () -> MethodHandles.lookup().lookupClass() == SbrW6Mhlookup.class);
        rowx("lookup loader", () -> MethodHandles.lookup().lookupClass().getClassLoader() == SbrW6Mhlookup.class.getClassLoader());
        rowx("publicLookup class", () -> MethodHandles.publicLookup().lookupClass().getName());
        rowx("publicLookup modes", () -> MethodHandles.publicLookup().lookupModes());
        rowx("publicLookup toString", () -> MethodHandles.publicLookup().toString());
        rowx("publicLookup same", () -> MethodHandles.publicLookup() == MethodHandles.publicLookup());
        rowx("publicLookup prev", () -> MethodHandles.publicLookup().previousLookupClass());
        rowx("publicLookup findStatic public", () -> MethodHandles.publicLookup()
                .findStatic(Math.class, "abs", MethodType.methodType(int.class, int.class)).invoke(-3));
        rowx("publicLookup findStatic nonpublic", () -> MethodHandles.publicLookup()
                .findStatic(SbrW6Mhlookup.class, "pkg", MethodType.methodType(String.class)));
        rowx("publicLookup findVirtual pkg class", () -> MethodHandles.publicLookup()
                .findVirtual(Nested.class, "toString", MethodType.methodType(String.class)));
        rowx("privateLookupIn self", () -> {
            Lookup p = MethodHandles.privateLookupIn(SbrW6Mhlookup.class, L);
            return p.lookupClass().getName() + " " + p.lookupModes();
        });
        rowx("privateLookupIn nested", () -> {
            Lookup p = MethodHandles.privateLookupIn(Nested.class, L);
            return p.lookupClass().getName() + " " + p.lookupModes() + " " + p.previousLookupClass();
        });
        rowx("privateLookupIn private field", () -> MethodHandles.privateLookupIn(SbrW6Mhlookup.class, L)
                .findGetter(SbrW6Mhlookup.class, "priv", int.class).invoke(new SbrW6Mhlookup()));
        rowx("privateLookupIn String (other module)", () -> MethodHandles.privateLookupIn(String.class, L));
        rowx("privateLookupIn null target", () -> MethodHandles.privateLookupIn(null, L));
        rowx("privateLookupIn null lookup", () -> MethodHandles.privateLookupIn(String.class, null));
        rowx("privateLookupIn primitive", () -> MethodHandles.privateLookupIn(int.class, L));
        rowx("privateLookupIn array", () -> MethodHandles.privateLookupIn(int[].class, L));
        rowx("privateLookupIn from publicLookup", () -> MethodHandles.privateLookupIn(SbrW6Mhlookup.class,
                MethodHandles.publicLookup()));
        rowx("privateLookupIn from dropped", () -> MethodHandles.privateLookupIn(SbrW6Mhlookup.class,
                L.dropLookupMode(Lookup.MODULE)));
        rowx("privateLookupIn result in", () -> MethodHandles.privateLookupIn(SbrW6Mhlookup.class, L)
                .in(Pub.class).lookupModes());

        // ------------------------------------------------ Lookup accessors / in / drop
        rowx("L.lookupClass", () -> L.lookupClass().getSimpleName());
        rowx("L.lookupModes", () -> L.lookupModes());
        rowx("L.in(self)", () -> L.in(SbrW6Mhlookup.class).lookupModes());
        rowx("L.in(nestmate)", () -> L.in(Nested.class).lookupModes());
        rowx("L.in(String)", () -> L.in(String.class).lookupModes());
        rowx("L.in(null)", () -> L.in(null));
        rowx("L.in(int)", () -> L.in(int.class));
        rowx("L.in(array)", () -> L.in(String[].class).lookupModes());
        rowx("L.in(String).in(self)", () -> L.in(String.class).in(SbrW6Mhlookup.class).lookupModes());
        rowx("publicLookup.in", () -> MethodHandles.publicLookup().in(SbrW6Mhlookup.class).lookupModes());
        rowx("L.dropLookupMode(PRIVATE)", () -> L.dropLookupMode(Lookup.PRIVATE).lookupModes());
        rowx("L.dropLookupMode(PUBLIC)", () -> L.dropLookupMode(Lookup.PUBLIC).lookupModes());
        rowx("L.dropLookupMode(PACKAGE)", () -> L.dropLookupMode(Lookup.PACKAGE).lookupModes());
        rowx("L.dropLookupMode(MODULE)", () -> L.dropLookupMode(Lookup.MODULE).lookupModes());
        rowx("L.dropLookupMode(UNCONDITIONAL)", () -> L.dropLookupMode(Lookup.UNCONDITIONAL).lookupModes());
        rowx("L.dropLookupMode(ORIGINAL)", () -> L.dropLookupMode(Lookup.ORIGINAL).lookupModes());
        rowx("L.dropLookupMode(bad)", () -> L.dropLookupMode(3));
        rowx("L.dropped toString", () -> L.dropLookupMode(Lookup.PRIVATE).toString());
        rowx("L.in(String) toString", () -> L.in(String.class).toString());
        rowx("L.in(nestmate) toString", () -> L.in(Nested.class).toString());
        rowx("L.dropped(PUBLIC) findStatic", () -> L.dropLookupMode(Lookup.PUBLIC).findStatic(Math.class, "abs",
                MethodType.methodType(int.class, int.class)));
        rowx("L.in(String) findStatic private", () -> L.in(String.class).findStatic(SbrW6Mhlookup.class, "secret",
                MethodType.methodType(String.class)));
        rowx("L.in(Nested) findStatic pkg", () -> L.in(Nested.class).findStatic(SbrW6Mhlookup.class, "pkg",
                MethodType.methodType(String.class)).invoke());
        rowx("L.in(Nested) findStatic private", () -> L.in(Nested.class).findStatic(SbrW6Mhlookup.class, "secret",
                MethodType.methodType(String.class)).invoke());
        rowx("L.lookupClass identity", () -> L.lookupClass() == SbrW6Mhlookup.class);

        // ------------------------------------------------ ensureInitialized
        rowx("ensureInitialized self", () -> L.ensureInitialized(SbrW6Mhlookup.class).getName());
        rowx("ensureInitialized lazy", () -> L.ensureInitialized(LazyInit.class).getSimpleName() + " " + LazyInit.n);
        rowx("ensureInitialized primitive", () -> L.ensureInitialized(int.class));
        rowx("ensureInitialized array", () -> L.ensureInitialized(int[].class));
        rowx("ensureInitialized null", () -> L.ensureInitialized(null));
        rowx("ensureInitialized String", () -> L.ensureInitialized(String.class).getName());
        rowx("ensureInitialized inaccessible", () -> MethodHandles.publicLookup().ensureInitialized(Nested.class));
        rowx("ensureInitialized public from publicLookup", () -> MethodHandles.publicLookup()
                .ensureInitialized(Pub.class).getSimpleName());
        rowx("ensureInitialized from in(String)", () -> L.in(String.class).ensureInitialized(Nested.class));
        rowx("ensureInitialized from dropped", () -> L.dropLookupMode(Lookup.PACKAGE).ensureInitialized(Nested.class));

        // ------------------------------------------------ Lookup.find* success and error rows
        rowx("findStatic ok", () -> L.findStatic(SbrW6Mhlookup.class, "inc", MethodType.methodType(int.class, int.class)).invoke(1));
        rowx("findStatic missing", () -> L.findStatic(SbrW6Mhlookup.class, "nope", MethodType.methodType(int.class)));
        rowx("findStatic virtual target", () -> L.findStatic(SbrW6Mhlookup.class, "hello", MethodType.methodType(String.class, String.class)));
        rowx("findStatic null name", () -> L.findStatic(SbrW6Mhlookup.class, null, MethodType.methodType(int.class)));
        rowx("findStatic null type", () -> L.findStatic(SbrW6Mhlookup.class, "inc", null));
        rowx("findStatic null class", () -> L.findStatic(null, "inc", MethodType.methodType(int.class, int.class)));
        rowx("findStatic private", () -> L.findStatic(SbrW6Mhlookup.class, "secret", MethodType.methodType(String.class)).invoke());
        rowx("findStatic other pkg", () -> L.findStatic(Nested.class, "mine", MethodType.methodType(Lookup.class)).type());
        rowx("findStatic wrong return", () -> L.findStatic(SbrW6Mhlookup.class, "inc", MethodType.methodType(long.class, int.class)));
        rowx("findVirtual ok", () -> L.findVirtual(SbrW6Mhlookup.class, "hello", MethodType.methodType(String.class, String.class))
                .invoke(new SbrW6Mhlookup(), "x"));
        rowx("findVirtual type", () -> L.findVirtual(SbrW6Mhlookup.class, "hello", MethodType.methodType(String.class, String.class)).type());
        rowx("findVirtual missing", () -> L.findVirtual(SbrW6Mhlookup.class, "nope", MethodType.methodType(void.class)));
        rowx("findVirtual static target", () -> L.findVirtual(SbrW6Mhlookup.class, "inc", MethodType.methodType(int.class, int.class)));
        rowx("findVirtual interface", () -> L.findVirtual(List.class, "size", MethodType.methodType(int.class)).invoke(List.of(1, 2)));
        rowx("findVirtual String.length", () -> L.findVirtual(String.class, "length", MethodType.methodType(int.class)).invoke("abcd"));
        rowx("findVirtual publicLookup non-public class", () -> MethodHandles.publicLookup()
                .findVirtual(Nested.class, "hashCode", MethodType.methodType(int.class)));
        rowx("findConstructor ok", () -> ((SbrW6Mhlookup) L.findConstructor(SbrW6Mhlookup.class, MethodType.methodType(void.class, int.class))
                .invoke(9)).f);
        rowx("findConstructor type", () -> L.findConstructor(SbrW6Mhlookup.class, MethodType.methodType(void.class, int.class)).type());
        rowx("findConstructor missing", () -> L.findConstructor(SbrW6Mhlookup.class, MethodType.methodType(void.class, String.class)));
        rowx("findConstructor bad return", () -> L.findConstructor(SbrW6Mhlookup.class, MethodType.methodType(int.class)));
        rowx("findConstructor abstract", () -> L.findConstructor(java.util.AbstractList.class, MethodType.methodType(void.class)));
        rowx("findConstructor interface", () -> L.findConstructor(Runnable.class, MethodType.methodType(void.class)));
        rowx("findGetter ok", () -> L.findGetter(SbrW6Mhlookup.class, "f", int.class).invoke(new SbrW6Mhlookup(3)));
        rowx("findGetter private", () -> L.findGetter(SbrW6Mhlookup.class, "priv", int.class).invoke(new SbrW6Mhlookup()));
        rowx("findGetter missing", () -> L.findGetter(SbrW6Mhlookup.class, "nope", int.class));
        rowx("findGetter wrong type", () -> L.findGetter(SbrW6Mhlookup.class, "f", long.class));
        rowx("findGetter static", () -> L.findGetter(SbrW6Mhlookup.class, "sf", int.class));
        rowx("findSetter ok", () -> {
            SbrW6Mhlookup o = new SbrW6Mhlookup();
            L.findSetter(SbrW6Mhlookup.class, "f", int.class).invoke(o, 11);
            return o.f;
        });
        rowx("findSetter final", () -> L.findSetter(String.class, "value", byte[].class));
        rowx("findStaticGetter ok", () -> L.findStaticGetter(SbrW6Mhlookup.class, "sf", int.class).invoke());
        rowx("findStaticGetter instance", () -> L.findStaticGetter(SbrW6Mhlookup.class, "f", int.class));
        rowx("findStaticGetter missing", () -> L.findStaticGetter(SbrW6Mhlookup.class, "nope", int.class));
        rowx("findVarHandle ok", () -> {
            VarHandle vh = L.findVarHandle(SbrW6Mhlookup.class, "f", int.class);
            SbrW6Mhlookup o = new SbrW6Mhlookup(4);
            return vh.get(o) + " " + vh.varType() + " " + vh.coordinateTypes();
        });
        rowx("findVarHandle missing", () -> L.findVarHandle(SbrW6Mhlookup.class, "nope", int.class));
        rowx("findVarHandle wrong type", () -> L.findVarHandle(SbrW6Mhlookup.class, "f", long.class));
        rowx("findVarHandle static field", () -> L.findVarHandle(SbrW6Mhlookup.class, "sf", int.class));
        rowx("findVarHandle private", () -> L.findVarHandle(SbrW6Mhlookup.class, "priv", int.class).varType());
        rowx("findVarHandle null name", () -> L.findVarHandle(SbrW6Mhlookup.class, null, int.class));
        rowx("findStaticVarHandle ok", () -> {
            VarHandle vh = L.findStaticVarHandle(SbrW6Mhlookup.class, "sf", int.class);
            return vh.get() + " " + vh.varType() + " " + vh.coordinateTypes();
        });
        rowx("findStaticVarHandle instance", () -> L.findStaticVarHandle(SbrW6Mhlookup.class, "f", int.class));
        rowx("unreflect", () -> L.unreflect(SbrW6Mhlookup.class.getMethod("hello", String.class))
                .invoke(new SbrW6Mhlookup(), "u"));
        rowx("unreflect type", () -> L.unreflect(SbrW6Mhlookup.class.getMethod("hello", String.class)).type());
        rowx("unreflect private", () -> L.unreflect(SbrW6Mhlookup.class.getDeclaredMethod("secret")).invoke());
        rowx("unreflect null", () -> L.unreflect(null));
        rowx("unreflectSpecial null", () -> L.unreflectSpecial(null, SbrW6Mhlookup.class));
        rowx("unreflectConstructor null", () -> L.unreflectConstructor(null));
        rowx("unreflectGetter null", () -> L.unreflectGetter(null));
        rowx("unreflectSetter null", () -> L.unreflectSetter(null));
        rowx("unreflect inaccessible", () -> MethodHandles.publicLookup()
                .unreflect(SbrW6Mhlookup.class.getDeclaredMethod("secret")));
        rowx("unreflectConstructor", () -> ((SbrW6Mhlookup) L.unreflectConstructor(SbrW6Mhlookup.class.getConstructor(int.class))
                .invoke(2)).f);
        rowx("unreflectGetter", () -> L.unreflectGetter(SbrW6Mhlookup.class.getField("f")).invoke(new SbrW6Mhlookup(8)));
        rowx("findClass", () -> L.findClass("java.lang.String").getName());
        rowx("findClass missing", () -> L.findClass("no.such.Klass"));
        rowx("accessClass", () -> L.accessClass(String.class).getName());
        rowx("accessClass nested private", () -> MethodHandles.publicLookup().accessClass(Nested.class));

        // ------------------------------------------------ MethodType
        rowx("mt void", () -> MethodType.methodType(void.class));
        rowx("mt (Class)", () -> MethodType.methodType(int.class));
        rowx("mt (Class,Class)", () -> MethodType.methodType(String.class, int.class));
        rowx("mt (Class,Class,Class...)", () -> MethodType.methodType(String.class, int.class, long.class, Object.class));
        rowx("mt (Class,Class[])", () -> MethodType.methodType(String.class, new Class<?>[] {int.class, double.class}));
        rowx("mt (Class,List)", () -> MethodType.methodType(String.class, List.of(int.class, byte.class)));
        rowx("mt (Class,MethodType)", () -> MethodType.methodType(char.class, MethodType.methodType(void.class, int.class)));
        rowx("mt interned same", () -> MethodType.methodType(int.class, String.class) == MethodType.methodType(int.class, String.class));
        rowx("mt interned arr vs var", () -> MethodType.methodType(int.class, String.class, long.class)
                == MethodType.methodType(int.class, new Class<?>[] {String.class, long.class}));
        rowx("mt interned 1arg vs arr", () -> MethodType.methodType(int.class, String.class)
                == MethodType.methodType(int.class, new Class<?>[] {String.class}));
        rowx("mt interned 0 vs empty", () -> MethodType.methodType(int.class) == MethodType.methodType(int.class, new Class<?>[0]));
        rowx("mt equals", () -> MethodType.methodType(int.class, String.class).equals(MethodType.methodType(int.class, String.class)));
        rowx("mt hashCode eq", () -> MethodType.methodType(int.class, String.class).hashCode() == MethodType.methodType(int.class, String.class).hashCode());
        rowx("mt null rtype", () -> MethodType.methodType(null));
        rowx("mt null rtype 2", () -> MethodType.methodType(null, int.class));
        rowx("mt null ptype", () -> MethodType.methodType(int.class, (Class<?>) null));
        rowx("mt null ptypes arr", () -> MethodType.methodType(int.class, (Class<?>[]) null));
        rowx("mt null elem arr", () -> MethodType.methodType(int.class, new Class<?>[] {String.class, null}));
        rowx("mt void ptype", () -> MethodType.methodType(int.class, void.class));
        rowx("mt void ptype 3", () -> MethodType.methodType(int.class, int.class, new Class<?>[] {void.class}));
        rowx("mt null more", () -> MethodType.methodType(int.class, String.class, (Class<?>[]) null));
        rowx("mt caller array mutated after", () -> {
            Class<?>[] a = {int.class, long.class};
            MethodType t = MethodType.methodType(void.class, a);
            a[0] = String.class;
            return t;
        });
        rowx("mt parameterType", () -> MethodType.methodType(void.class, int.class, String.class).parameterType(1));
        rowx("mt parameterType -1", () -> MethodType.methodType(void.class, int.class).parameterType(-1));
        rowx("mt parameterType oob", () -> MethodType.methodType(void.class, int.class).parameterType(1));
        rowx("mt parameterType empty", () -> MethodType.methodType(void.class).parameterType(0));
        rowx("mt parameterCount", () -> MethodType.methodType(void.class, int.class, String.class).parameterCount());
        rowx("mt returnType", () -> MethodType.methodType(long.class, int.class).returnType());
        rowx("mt toString", () -> MethodType.methodType(String[].class, int[].class, Nested.class, List.class));
        rowx("mt descriptorString", () -> MethodType.methodType(String[].class, int.class, List.class).descriptorString());
        rowx("mt toMethodDescriptorString", () -> MethodType.methodType(void.class, long.class, Object[].class).toMethodDescriptorString());
        rowx("mt fromMethodDescriptorString", () -> MethodType.fromMethodDescriptorString("(IJLjava/lang/String;)[I", null));
        rowx("mt fromMethodDescriptorString bad", () -> MethodType.fromMethodDescriptorString("(V)I", null));
        rowx("mt genericMethodType", () -> MethodType.genericMethodType(3));
        rowx("mt genericMethodType varargs", () -> MethodType.genericMethodType(2, true));
        rowx("mt genericMethodType neg", () -> MethodType.genericMethodType(-1));
        rowx("mt changeReturnType", () -> MethodType.methodType(int.class, String.class).changeReturnType(long.class));
        rowx("mt changeParameterType", () -> MethodType.methodType(int.class, String.class).changeParameterType(0, long.class));
        rowx("mt insertParameterTypes", () -> MethodType.methodType(int.class, String.class).insertParameterTypes(0, long.class, byte.class));
        rowx("mt appendParameterTypes", () -> MethodType.methodType(int.class, String.class).appendParameterTypes(long.class));
        rowx("mt dropParameterTypes", () -> MethodType.methodType(int.class, String.class, long.class).dropParameterTypes(0, 1));
        rowx("mt wrap", () -> MethodType.methodType(int.class, long.class, String.class).wrap());
        rowx("mt unwrap", () -> MethodType.methodType(Integer.class, Long.class, String.class).unwrap());
        rowx("mt erase", () -> MethodType.methodType(String.class, Integer.class, int.class).erase());
        rowx("mt generic", () -> MethodType.methodType(String.class, Integer.class, int.class).generic());
        rowx("mt hasPrimitives", () -> MethodType.methodType(String.class, Integer.class).hasPrimitives() + " "
                + MethodType.methodType(String.class, int.class).hasPrimitives());
        rowx("mt hasWrappers", () -> MethodType.methodType(String.class, Integer.class).hasWrappers());
        rowx("mt parameterList", () -> MethodType.methodType(String.class, Integer.class, int.class).parameterList());
        rowx("mt parameterList immutable", () -> MethodType.methodType(String.class, Integer.class).parameterList().add(int.class));
        rowx("mt lastParameterType", () -> MethodType.methodType(String.class, Integer.class, int.class).lastParameterType());
        rowx("mt lastParameterType empty", () -> MethodType.methodType(String.class).lastParameterType());
        rowx("mt parameterArray copy", () -> {
            MethodType t = MethodType.methodType(void.class, int.class, long.class);
            t.parameterArray()[0] = String.class;
            return t;
        });
        rowx("mt ptypes 255", () -> {
            Class<?>[] a = new Class<?>[255];
            java.util.Arrays.fill(a, int.class);
            return MethodType.methodType(void.class, a).parameterCount();
        });
        rowx("mt ptypes 256", () -> {
            Class<?>[] a = new Class<?>[256];
            java.util.Arrays.fill(a, int.class);
            return MethodType.methodType(void.class, a).parameterCount();
        });
        rowx("mt 128 longs", () -> {
            Class<?>[] a = new Class<?>[128];
            java.util.Arrays.fill(a, long.class);
            return MethodType.methodType(void.class, a).parameterCount();
        });
        rowx("mt in HashSet", () -> {
            java.util.Set<MethodType> s = new java.util.HashSet<>();
            s.add(MethodType.methodType(int.class, String.class));
            s.add(MethodType.methodType(int.class, String.class));
            s.add(MethodType.methodType(long.class, String.class));
            return s.size();
        });
        rowx("mt class name", () -> MethodType.methodType(int.class).getClass().getName());
        rowx("mt serializable", () -> MethodType.methodType(int.class) instanceof java.io.Serializable);

        // ------------------------------------------------ MethodHandle accessors
        rowx("mh type", () -> L.findStatic(SbrW6Mhlookup.class, "inc", MethodType.methodType(int.class, int.class)).type());
        rowx("mh type bound", () -> L.findVirtual(SbrW6Mhlookup.class, "hello", MethodType.methodType(String.class, String.class))
                .bindTo(new SbrW6Mhlookup()).type());
        rowx("mh type asType", () -> L.findStatic(SbrW6Mhlookup.class, "inc", MethodType.methodType(int.class, int.class))
                .asType(MethodType.methodType(Object.class, Integer.class)).type());
        rowx("mh type constant", () -> MethodHandles.constant(String.class, "k").type());
        rowx("mh type identity", () -> MethodHandles.identity(long.class).type());
        rowx("mh type insertArguments", () -> MethodHandles.insertArguments(
                L.findStatic(SbrW6Mhlookup.class, "inc", MethodType.methodType(int.class, int.class)), 0, 5).type());
        rowx("mh type arrayElementGetter", () -> MethodHandles.arrayElementGetter(int[].class).type());
        rowx("mh type empty", () -> MethodHandles.empty(MethodType.methodType(int.class, String.class)).type());
        rowx("mh isVarargsCollector", () -> L.findStatic(String.class, "format", MethodType.methodType(String.class, String.class, Object[].class))
                .isVarargsCollector());
        rowx("mh asFixedArity", () -> L.findStatic(String.class, "format", MethodType.methodType(String.class, String.class, Object[].class))
                .asFixedArity().isVarargsCollector());
        rowx("mh invokeWithArguments", () -> L.findStatic(SbrW6Mhlookup.class, "inc", MethodType.methodType(int.class, int.class))
                .invokeWithArguments(4));
        rowx("mh invokeWithArguments list", () -> L.findStatic(SbrW6Mhlookup.class, "inc", MethodType.methodType(int.class, int.class))
                .invokeWithArguments(List.of(6)));
        rowx("mh invokeWithArguments arity", () -> L.findStatic(SbrW6Mhlookup.class, "inc", MethodType.methodType(int.class, int.class))
                .invokeWithArguments(1, 2));
        rowx("mh invokeExact wrong type", () -> {
            MethodHandle mh = L.findStatic(SbrW6Mhlookup.class, "inc", MethodType.methodType(int.class, int.class));
            return (Object) mh.invokeExact((Object) Integer.valueOf(3));
        });
        rowx("mh asSpreader", () -> L.findStatic(Math.class, "max", MethodType.methodType(int.class, int.class, int.class))
                .asSpreader(int[].class, 2).invoke(new int[] {3, 9}));
        rowx("mh asCollector", () -> L.findStatic(java.util.Arrays.class, "toString", MethodType.methodType(String.class, Object[].class))
                .asCollector(Object[].class, 2).invoke("a", "b"));
        rowx("mh asVarargsCollector", () -> L.findStatic(java.util.Arrays.class, "toString", MethodType.methodType(String.class, Object[].class))
                .asVarargsCollector(Object[].class).invoke("a", "b", "c"));
        rowx("mh bindTo", () -> L.findVirtual(SbrW6Mhlookup.class, "hello", MethodType.methodType(String.class, String.class))
                .bindTo(new SbrW6Mhlookup()).invoke("z"));

        // ------------------------------------------------ MemberName via revealDirect
        rowx("reveal static", () -> info(L.revealDirect(L.findStatic(SbrW6Mhlookup.class, "inc", MethodType.methodType(int.class, int.class)))));
        rowx("reveal virtual", () -> info(L.revealDirect(L.findVirtual(SbrW6Mhlookup.class, "hello", MethodType.methodType(String.class, String.class)))));
        rowx("reveal interface", () -> info(L.revealDirect(L.findVirtual(List.class, "size", MethodType.methodType(int.class)))));
        rowx("reveal constructor", () -> info(L.revealDirect(L.findConstructor(SbrW6Mhlookup.class, MethodType.methodType(void.class, int.class)))));
        rowx("reveal getter", () -> info(L.revealDirect(L.findGetter(SbrW6Mhlookup.class, "f", int.class))));
        rowx("reveal setter", () -> info(L.revealDirect(L.findSetter(SbrW6Mhlookup.class, "f", int.class))));
        rowx("reveal staticGetter", () -> info(L.revealDirect(L.findStaticGetter(SbrW6Mhlookup.class, "sf", int.class))));
        rowx("reveal special", () -> info(L.revealDirect(L.findSpecial(SbrW6Mhlookup.class, "hello",
                MethodType.methodType(String.class, String.class), SbrW6Mhlookup.class))));
        rowx("reveal private", () -> {
            MethodHandleInfo i = L.revealDirect(L.findStatic(SbrW6Mhlookup.class, "secret", MethodType.methodType(String.class)));
            return i.getModifiers() + " " + i + " " + i.reflectAs(Method.class, L).getName();
        });
        rowx("reveal non-direct", () -> L.revealDirect(MethodHandles.identity(int.class)));
        rowx("reveal bound", () -> L.revealDirect(L.findVirtual(SbrW6Mhlookup.class, "hello", MethodType.methodType(String.class, String.class))
                .bindTo(new SbrW6Mhlookup())));
        rowx("reveal reflectAs", () -> L.revealDirect(L.findStatic(SbrW6Mhlookup.class, "inc", MethodType.methodType(int.class, int.class)))
                .reflectAs(Method.class, L).getName());
        rowx("reveal from lookup lacking access", () -> MethodHandles.publicLookup().revealDirect(
                L.findStatic(SbrW6Mhlookup.class, "secret", MethodType.methodType(String.class))));

        // ------------------------------------------------ hidden class / define
        rowx("defineHiddenClass bad bytes", () -> L.defineHiddenClass(new byte[] {1, 2, 3}, true));
        rowx("defineClass bad bytes", () -> L.defineClass(new byte[] {1, 2, 3}));
        rowx("defineClass null", () -> L.defineClass(null));
        rowx("defineHiddenClass null", () -> L.defineHiddenClass(null, true));

        // ------------------------------------------------ first run then hot loop
        String[] first = new String[ROWS.size()];
        for (int i = 0; i < ROWS.size(); i++) {
            first[i] = safe(ROWS.get(i));
            System.out.println("row " + LABELS.get(i) + " = " + first[i]);
        }
        int drift = 0;
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        for (int k = 0; k < iters; k++) {
            for (int i = 0; i < ROWS.size(); i++) {
                String v = safe(ROWS.get(i));
                if (!v.equals(first[i])) {
                    if (drift < 10) {
                        System.out.println("DRIFT at iter " + k + " " + LABELS.get(i) + " = " + v);
                    }
                    drift++;
                }
            }
        }
        System.out.println("rows " + ROWS.size() + " drift " + drift);
    }
}
