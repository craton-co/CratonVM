// The `java.lang.invoke` LOOKUP and TYPE surfaces — the 15 `native-won` triples
// the report names for `MethodHandles`, `MethodHandles$Lookup`, `MethodType`
// and the three `MethodHandle` accessors.
//
// Distinct from `L5ModuleInvokeSweep`, which probes DISPATCH (`invoke` /
// `invokeExact`). Nothing here calls a handle; every row asks what the lookup
// or the type object ANSWERS, and most ask what it REFUSES.
//
// Hygiene: stdout only, no identity hashes. `MethodType.toString()` is
// specified and stable, so it is printed; exception MESSAGES are not, so only
// class names are.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.List;

public class L8InvokeLookupSweep {
    interface Body { Object run() throws Throwable; }

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

    public static class Target {
        public int pubField;
        public static int staticField = 7;
        public final int finalField = 3;
        private int privField;
        public Target() {}
        public Target(int v) { pubField = v; }
        public int instanceM(int a) { return a + 1; }
        public static int staticM(int a) { return a + 2; }
        private int privM(int a) { return a + 3; }
        public static void voidStatic() {}
    }

    public static void main(String[] a) {
        methodTypeFactory();
        methodTypeAccessors();
        lookupShape();
        findRefusals();
        findHappyPath();
        handleAccessors();
        System.out.println("DONE");
    }

    // ---------------- MethodType.methodType, the three registered arities ----

    static void methodTypeFactory() {
        t("mt.1arg", () -> MethodType.methodType(int.class).toString());
        t("mt.2arg", () -> MethodType.methodType(int.class, String.class).toString());
        t("mt.varargs", () -> MethodType.methodType(int.class, String.class, new Class<?>[] { long.class }).toString());
        t("mt.voidReturn", () -> MethodType.methodType(void.class).toString());
        t("mt.arrayParam", () -> MethodType.methodType(int.class, int[].class).toString());
        // void is legal as a RETURN and illegal as a PARAMETER.
        t("mt.voidParam", () -> MethodType.methodType(int.class, void.class).toString());
        t("mt.voidParamVarargs",
            () -> MethodType.methodType(int.class, String.class, new Class<?>[] { void.class }).toString());
        // Nulls, at each position the three overloads offer.
        t("mt.nullReturn", () -> MethodType.methodType((Class<?>) null).toString());
        t("mt.nullParam", () -> MethodType.methodType(int.class, (Class<?>) null).toString());
        t("mt.nullArray", () -> MethodType.methodType(int.class, String.class, (Class<?>[]) null).toString());
        t("mt.nullInArray",
            () -> MethodType.methodType(int.class, String.class, new Class<?>[] { null }).toString());
        // INTERNED: the spec requires the same instance for the same descriptor.
        t("mt.interned.same", () -> MethodType.methodType(int.class, String.class)
                == MethodType.methodType(int.class, String.class));
        t("mt.interned.equals", () -> MethodType.methodType(int.class, String.class)
                .equals(MethodType.methodType(int.class, String.class)));
        t("mt.hashEquals", () -> MethodType.methodType(int.class, String.class).hashCode()
                == MethodType.methodType(int.class, String.class).hashCode());
        t("mt.notEquals", () -> MethodType.methodType(int.class, String.class)
                .equals(MethodType.methodType(long.class, String.class)));
    }

    // ---------------- MethodType accessors ----------------

    static void methodTypeAccessors() {
        MethodType mt = MethodType.methodType(int.class, String.class, long.class);
        t("mt.returnType", () -> mt.returnType().getName());
        t("mt.parameterCount", () -> mt.parameterCount());
        t("mt.parameterType0", () -> mt.parameterType(0).getName());
        t("mt.parameterType1", () -> mt.parameterType(1).getName());
        t("mt.parameterTypeOob", () -> mt.parameterType(2).getName());
        t("mt.parameterTypeNeg", () -> mt.parameterType(-1).getName());
        t("mt.parameterList", () -> mt.parameterList().toString());
        t("mt.parameterArray.len", () -> mt.parameterArray().length);
        // parameterArray() is a COPY: editing it must not change the type.
        t("mt.parameterArray.isCopy", () -> {
            Class<?>[] arr = mt.parameterArray();
            arr[0] = int.class;
            return mt.parameterType(0).getName();
        });
        t("mt.toString", () -> mt.toString());
        t("mt.descriptorString", () -> mt.descriptorString());
        t("mt.changeReturnType", () -> mt.changeReturnType(void.class).toString());
        t("mt.appendParameterTypes", () -> mt.appendParameterTypes(int.class).toString());
        t("mt.dropParameterTypes", () -> mt.dropParameterTypes(0, 1).toString());
        t("mt.dropParameterTypesOob", () -> mt.dropParameterTypes(0, 9).toString());
        t("mt.wrap", () -> mt.wrap().toString());
        t("mt.unwrap", () -> mt.wrap().unwrap().toString());
        t("mt.erase", () -> mt.erase().toString());
        t("mt.generic", () -> mt.generic().toString());
        t("mt.fromDescriptor", () -> MethodType.fromMethodDescriptorString(
                "(Ljava/lang/String;J)I", L8InvokeLookupSweep.class.getClassLoader()).toString());
        t("mt.fromBadDescriptor", () -> MethodType.fromMethodDescriptorString(
                "not a descriptor", L8InvokeLookupSweep.class.getClassLoader()).toString());
    }

    // ---------------- MethodHandles.lookup() ----------------

    static void lookupShape() {
        MethodHandles.Lookup l = MethodHandles.lookup();
        t("lookup.class", () -> l.lookupClass().getName());
        t("lookup.hasPrivate", () -> (l.lookupModes() & MethodHandles.Lookup.PRIVATE) != 0);
        t("lookup.hasPublic", () -> (l.lookupModes() & MethodHandles.Lookup.PUBLIC) != 0);
        t("lookup.hasModule", () -> (l.lookupModes() & MethodHandles.Lookup.MODULE) != 0);
        t("publicLookup.hasPrivate",
            () -> (MethodHandles.publicLookup().lookupModes() & MethodHandles.Lookup.PRIVATE) != 0);
        t("lookup.in.class", () -> l.in(String.class).lookupClass().getName());
    }

    // ---------------- what the finders must REFUSE ----------------

    static void findRefusals() {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodType ii = MethodType.methodType(int.class, int.class);

        t("find.staticOnInstance", () -> l.findStatic(Target.class, "instanceM", ii).type().toString());
        t("find.virtualOnStatic", () -> l.findVirtual(Target.class, "staticM", ii).type().toString());
        t("find.noSuchMethod", () -> l.findVirtual(Target.class, "noSuchName", ii).type().toString());
        t("find.wrongSignature",
            () -> l.findVirtual(Target.class, "instanceM",
                    MethodType.methodType(String.class, int.class)).type().toString());
        t("find.virtualInit", () -> l.findVirtual(Target.class, "<init>", ii).type().toString());
        t("find.nullClass", () -> l.findVirtual(null, "instanceM", ii).type().toString());
        t("find.nullName", () -> l.findVirtual(Target.class, null, ii).type().toString());
        t("find.nullType", () -> l.findVirtual(Target.class, "instanceM", null).type().toString());

        t("find.noSuchField", () -> l.findGetter(Target.class, "nope", int.class).type().toString());
        t("find.getterWrongType", () -> l.findGetter(Target.class, "pubField", String.class).type().toString());
        t("find.getterOnStatic", () -> l.findGetter(Target.class, "staticField", int.class).type().toString());
        t("find.staticGetterOnInstance",
            () -> l.findStaticGetter(Target.class, "pubField", int.class).type().toString());
        t("find.setterOnFinal", () -> l.findSetter(Target.class, "finalField", int.class).type().toString());
        t("find.nullFieldName", () -> l.findGetter(Target.class, null, int.class).type().toString());
        t("find.nullFieldType", () -> l.findGetter(Target.class, "pubField", null).type().toString());

        // findConstructor takes a type whose return is void; anything else is refused.
        t("ctor.nonVoidReturn",
            () -> l.findConstructor(Target.class, MethodType.methodType(Target.class, int.class)).type().toString());
        t("ctor.noSuchArity",
            () -> l.findConstructor(Target.class, MethodType.methodType(void.class, String.class)).type().toString());
        t("ctor.onInterface",
            () -> l.findConstructor(Runnable.class, MethodType.methodType(void.class)).type().toString());
        t("ctor.onPrimitive",
            () -> l.findConstructor(int.class, MethodType.methodType(void.class)).type().toString());
        t("ctor.nullType", () -> l.findConstructor(Target.class, null).type().toString());

        // publicLookup cannot see a private member, and says so with its own type.
        t("publicLookup.private", () -> MethodHandles.publicLookup()
                .findVirtual(Target.class, "privM", ii).type().toString());
    }

    // ---------------- and what they must ACCEPT ----------------

    static void findHappyPath() {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodType ii = MethodType.methodType(int.class, int.class);
        t("ok.findVirtual", () -> l.findVirtual(Target.class, "instanceM", ii).type().toString());
        t("ok.findStatic", () -> l.findStatic(Target.class, "staticM", ii).type().toString());
        t("ok.findPrivate", () -> l.findVirtual(Target.class, "privM", ii).type().toString());
        t("ok.findGetter", () -> l.findGetter(Target.class, "pubField", int.class).type().toString());
        t("ok.findSetter", () -> l.findSetter(Target.class, "pubField", int.class).type().toString());
        t("ok.findStaticGetter", () -> l.findStaticGetter(Target.class, "staticField", int.class).type().toString());
        t("ok.findCtorNoArg",
            () -> l.findConstructor(Target.class, MethodType.methodType(void.class)).type().toString());
        t("ok.findCtorInt",
            () -> l.findConstructor(Target.class, MethodType.methodType(void.class, int.class)).type().toString());
        t("ok.findVoidStatic",
            () -> l.findStatic(Target.class, "voidStatic", MethodType.methodType(void.class)).type().toString());
        t("ok.findOnInterface", () -> l.findVirtual(List.class, "size",
                MethodType.methodType(int.class)).type().toString());
        t("ok.findOnJdkClass", () -> l.findVirtual(String.class, "concat",
                MethodType.methodType(String.class, String.class)).type().toString());
    }

    // ---------------- the three MethodHandle accessors ----------------

    static void handleAccessors() {
        MethodHandles.Lookup l = MethodHandles.lookup();
        t("mh.type", () -> {
            MethodHandle h = l.findStatic(Target.class, "staticM", MethodType.methodType(int.class, int.class));
            return h.type().toString();
        });
        t("mh.asType.widen", () -> {
            MethodHandle h = l.findStatic(Target.class, "staticM", MethodType.methodType(int.class, int.class));
            return h.asType(MethodType.methodType(long.class, int.class)).type().toString();
        });
        t("mh.asType.null", () -> {
            MethodHandle h = l.findStatic(Target.class, "staticM", MethodType.methodType(int.class, int.class));
            return h.asType(null).type().toString();
        });
        t("mh.asType.identity", () -> {
            MethodHandle h = l.findStatic(Target.class, "staticM", MethodType.methodType(int.class, int.class));
            return h.asType(h.type()) == h;
        });
        t("mh.bindTo.primitiveLeading", () -> {
            MethodHandle h = l.findStatic(Target.class, "staticM", MethodType.methodType(int.class, int.class));
            return h.bindTo(1).type().toString();
        });
        t("mh.bindTo.null", () -> {
            MethodHandle h = l.findVirtual(String.class, "length", MethodType.methodType(int.class));
            return h.bindTo(null).type().toString();
        });
        t("mh.bindTo.wrongRefType", () -> {
            MethodHandle h = l.findVirtual(String.class, "length", MethodType.methodType(int.class));
            return h.bindTo(Integer.valueOf(1)).type().toString();
        });
        t("mh.bindTo.ok", () -> {
            MethodHandle h = l.findVirtual(String.class, "length", MethodType.methodType(int.class));
            return h.bindTo("abc").type().toString();
        });
    }
}
