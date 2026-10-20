// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 108, lane L7
// (i107-L7-other-plain-shadows-answer-a-null-argument-with-their-own-npe, the Hibernate family):
// a null REQUIRED argument to a Hibernate `Intrinsic` shadow (`orm_hibernate.rs`,
// `hibernate_null_required_arg_runs_bytecode`) runs Hibernate's bytecode -- the receiver's class
// for an instance method, the declaring class for a static one -- never the shadow body, whose
// `obj_arg` answered "java.lang.NullPointerException: null object argument". Self-contained
// SHAPES generated with java.lang.classfile (classes with hibernate-models' / hibernate-core's
// names and descriptors, small bodies), so no Hibernate jar is needed:
//   find-usage-null-descriptor  static AnnotationUsageHelper.findUsage(null, map): the body calls
//                               descriptor.getAnnotationType() -> JEP 358 NPE on <parameter1>
//   has-direct-null-type        AnnotationTargetSupport.hasDirectAnnotationUsage(null), an
//                               interface default method run for an implementing class:
//                               getUsageMap().containsKey(null) -> false, no exception at all
//   indexed-for-each-null       ImmutableAttributeMappingList.indexedForEach(null): the body calls
//                               consumer.accept(0, null) -> JEP 358 NPE on <parameter1>
//
// Setup (the AppIntrinsics family is selected at boot from the class-path witness
// `org/hibernate/spi/NavigablePath.class`, which --emit writes too):
//   javac -d out L7W108HibernateNullArgs.java
//   java -cp out L7W108HibernateNullArgs --emit shapes          (any JVM: writes the shapes)
//   java -cp out:shapes L7W108HibernateNullArgs                 (HotSpot reference)
//   cratonvm [--compatible] [--nojit] -cp out:shapes L7W108HibernateNullArgs
// Evidence that the shadows ran and handed back:
//   CRATONVM_DBG_SHADOW_RERUN=1 cratonvm --nojit -cp out:shapes L7W108HibernateNullArgs 2>err.txt
//   grep -c "^\[shadow-rerun\] .*after a null argument" err.txt     (expect 3)
// Before wave 108 every row printed "java.lang.NullPointerException: null object argument".
// Without `shapes` on the class path the probe defines the classes in a loader of its own: no
// shim family is selected, so that run is not evidence.
// HOTSPOT_EXPECTED_BEGIN (JDK 25; NPE texts predicted from JEP 358 for these bytes)
// find-usage-null-descriptor -> java.lang.NullPointerException: Cannot invoke "org.hibernate.models.spi.AnnotationDescriptor.getAnnotationType()" because "<parameter1>" is null
// has-direct-null-type -> false
// indexed-for-each-null -> java.lang.NullPointerException: Cannot invoke "org.hibernate.internal.util.IndexedConsumer.accept(int, Object)" because "<parameter1>" is null
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassBuilder;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.function.Consumer;

public class L7W108HibernateNullArgs {

    /** What the generated driver does; every call site in it is an ordinary invoke of a shape. */
    public interface Api {
        Object findUsage(Object descriptor, Object map);

        boolean hasDirect(Object target, Object type);

        Object newTarget();

        void indexedForEach(Object list, Object consumer);

        Object newList();
    }

    static final int PUB = ClassFile.ACC_PUBLIC;
    static final int PUB_STATIC = ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC;
    static final int PUB_ABSTRACT_METHOD = ClassFile.ACC_PUBLIC | ClassFile.ACC_ABSTRACT;
    static final int PUB_SUPER = ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER;
    static final int PUB_INTERFACE = ClassFile.ACC_PUBLIC | ClassFile.ACC_INTERFACE | ClassFile.ACC_ABSTRACT;

    static final ClassDesc NAVIGABLE_PATH = ClassDesc.of("org.hibernate.spi.NavigablePath");
    static final ClassDesc DESCRIPTOR = ClassDesc.of("org.hibernate.models.spi.AnnotationDescriptor");
    static final ClassDesc HELPER = ClassDesc.of("org.hibernate.models.internal.AnnotationUsageHelper");
    static final ClassDesc TARGET_SUPPORT = ClassDesc.of("org.hibernate.models.internal.AnnotationTargetSupport");
    static final ClassDesc INDEXED_CONSUMER = ClassDesc.of("org.hibernate.internal.util.IndexedConsumer");
    static final ClassDesc MAPPING_LIST =
        ClassDesc.of("org.hibernate.metamodel.mapping.internal.ImmutableAttributeMappingList");
    static final ClassDesc TARGET_IMPL = ClassDesc.of("L7W108HibTarget");
    static final ClassDesc DRIVER = ClassDesc.of("L7W108HibernateNullArgsDriver");
    static final ClassDesc API = ClassDesc.of("L7W108HibernateNullArgs$Api");

    static final ClassDesc OBJ = ConstantDescs.CD_Object;
    static final ClassDesc CLASS = ConstantDescs.CD_Class;
    static final ClassDesc MAP = ClassDesc.of("java.util.Map");
    static final ClassDesc HASH_MAP = ClassDesc.of("java.util.HashMap");
    static final ClassDesc ANNOTATION = ClassDesc.of("java.lang.annotation.Annotation");
    static final ClassDesc I = ConstantDescs.CD_int;
    static final ClassDesc Z = ConstantDescs.CD_boolean;
    static final ClassDesc V = ConstantDescs.CD_void;

    static final MethodTypeDesc FIND_USAGE = MethodTypeDesc.of(ANNOTATION, DESCRIPTOR, MAP);
    static final MethodTypeDesc HAS_DIRECT = MethodTypeDesc.of(Z, CLASS);
    static final MethodTypeDesc INDEXED_FOR_EACH = MethodTypeDesc.of(V, INDEXED_CONSUMER);

    static MethodTypeDesc mt(ClassDesc ret, ClassDesc... params) {
        return MethodTypeDesc.of(ret, params);
    }

    static byte[] cls(ClassDesc name, int flags, ClassDesc superClass, Consumer<ClassBuilder> body) {
        return ClassFile.of().build(name, cb -> {
            cb.withFlags(flags);
            cb.withSuperclass(superClass);
            body.accept(cb);
        });
    }

    static void ctor(ClassBuilder cb, ClassDesc superClass) {
        cb.withMethodBody("<init>", mt(V), PUB, c -> c.aload(0).invokespecial(superClass, "<init>", mt(V)).return_());
    }

    static Map<String, byte[]> shapes() {
        Map<String, byte[]> out = new LinkedHashMap<>();
        // Shim witness for the AppIntrinsics family.
        out.put("org.hibernate.spi.NavigablePath", cls(NAVIGABLE_PATH, PUB_SUPER, OBJ, cb -> ctor(cb, OBJ)));
        out.put("org.hibernate.models.spi.AnnotationDescriptor", cls(DESCRIPTOR, PUB_INTERFACE, OBJ, cb ->
            cb.withMethod("getAnnotationType", mt(CLASS), PUB_ABSTRACT_METHOD, mb -> { })));
        // static Annotation findUsage(AnnotationDescriptor d, Map m): (Annotation) m.get(d.getAnnotationType())
        out.put("org.hibernate.models.internal.AnnotationUsageHelper", cls(HELPER, PUB_SUPER, OBJ, cb -> {
            ctor(cb, OBJ);
            cb.withMethodBody("findUsage", FIND_USAGE, PUB_STATIC, c -> c
                .aload(1).aload(0).invokeinterface(DESCRIPTOR, "getAnnotationType", mt(CLASS))
                .invokeinterface(MAP, "get", mt(OBJ, OBJ)).checkcast(ANNOTATION).areturn());
        }));
        // interface AnnotationTargetSupport { Map getUsageMap();
        //   default boolean hasDirectAnnotationUsage(Class t) { return getUsageMap().containsKey(t); } }
        out.put("org.hibernate.models.internal.AnnotationTargetSupport", cls(TARGET_SUPPORT, PUB_INTERFACE, OBJ, cb -> {
            cb.withMethod("getUsageMap", mt(MAP), PUB_ABSTRACT_METHOD, mb -> { });
            cb.withMethodBody("hasDirectAnnotationUsage", HAS_DIRECT, PUB, c -> c
                .aload(0).invokeinterface(TARGET_SUPPORT, "getUsageMap", mt(MAP))
                .aload(1).invokeinterface(MAP, "containsKey", mt(Z, OBJ)).ireturn());
        }));
        out.put("org.hibernate.internal.util.IndexedConsumer", cls(INDEXED_CONSUMER, PUB_INTERFACE, OBJ, cb ->
            cb.withMethod("accept", mt(V, I, OBJ), PUB_ABSTRACT_METHOD, mb -> { })));
        // ImmutableAttributeMappingList.indexedForEach(IndexedConsumer c): c.accept(0, null)
        out.put("org.hibernate.metamodel.mapping.internal.ImmutableAttributeMappingList",
            cls(MAPPING_LIST, PUB_SUPER, OBJ, cb -> {
                ctor(cb, OBJ);
                cb.withMethodBody("indexedForEach", INDEXED_FOR_EACH, PUB, c -> c
                    .aload(1).iconst_0().aconst_null()
                    .invokeinterface(INDEXED_CONSUMER, "accept", mt(V, I, OBJ)).return_());
            }));
        // An implementing class with its own usage map (not a Hibernate name).
        out.put("L7W108HibTarget", cls(TARGET_IMPL, PUB_SUPER, OBJ, cb -> {
            cb.withInterfaceSymbols(TARGET_SUPPORT);
            ctor(cb, OBJ);
            cb.withMethodBody("getUsageMap", mt(MAP), PUB, c -> c
                .new_(HASH_MAP).dup().invokespecial(HASH_MAP, "<init>", mt(V)).areturn());
        }));
        out.put("L7W108HibernateNullArgsDriver", cls(DRIVER, PUB_SUPER, OBJ, cb -> {
            cb.withInterfaceSymbols(API);
            ctor(cb, OBJ);
            cb.withMethodBody("findUsage", mt(OBJ, OBJ, OBJ), PUB, c -> c
                .aload(1).checkcast(DESCRIPTOR).aload(2).checkcast(MAP)
                .invokestatic(HELPER, "findUsage", FIND_USAGE).areturn());
            cb.withMethodBody("hasDirect", mt(Z, OBJ, OBJ), PUB, c -> c
                .aload(1).checkcast(TARGET_SUPPORT).aload(2).checkcast(CLASS)
                .invokeinterface(TARGET_SUPPORT, "hasDirectAnnotationUsage", HAS_DIRECT).ireturn());
            cb.withMethodBody("newTarget", mt(OBJ), PUB, c -> c.new_(TARGET_IMPL).dup()
                .invokespecial(TARGET_IMPL, "<init>", mt(V)).areturn());
            cb.withMethodBody("indexedForEach", mt(V, OBJ, OBJ), PUB, c -> c
                .aload(1).checkcast(MAPPING_LIST).aload(2).checkcast(INDEXED_CONSUMER)
                .invokevirtual(MAPPING_LIST, "indexedForEach", INDEXED_FOR_EACH).return_());
            cb.withMethodBody("newList", mt(OBJ), PUB, c -> c.new_(MAPPING_LIST).dup()
                .invokespecial(MAPPING_LIST, "<init>", mt(V)).areturn());
        }));
        return out;
    }

    static final class MapLoader extends ClassLoader {
        final Map<String, byte[]> classes;

        MapLoader(Map<String, byte[]> classes, ClassLoader parent) {
            super(parent);
            this.classes = classes;
        }

        @Override
        protected Class<?> findClass(String n) throws ClassNotFoundException {
            byte[] bytes = classes.get(n);
            if (bytes != null) {
                return defineClass(n, bytes, 0, bytes.length);
            }
            throw new ClassNotFoundException(n);
        }
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String a;
        try {
            Object v = r.run();
            a = v == null ? "null" : v instanceof Boolean ? v.toString() : v.getClass().getName();
        } catch (Throwable t) {
            a = t.toString();
        }
        System.out.println(name + " -> " + a);
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("--emit")) {
            Path dir = Path.of(args[1]);
            for (Map.Entry<String, byte[]> e : shapes().entrySet()) {
                Path file = dir.resolve(e.getKey().replace('.', '/') + ".class");
                Files.createDirectories(file.getParent());
                Files.write(file, e.getValue());
            }
            System.out.println("emitted " + shapes().size() + " classes into " + dir);
            return;
        }
        ClassLoader loader = L7W108HibernateNullArgs.class.getClassLoader();
        if (loader.getResource("org/hibernate/spi/NavigablePath.class") == null) {
            System.err.println("L7W108HibernateNullArgs: shapes not on the class path (run with --emit <dir>"
                + " and add <dir> to -cp); defining them in a loader of the probe's own, no shim family selected");
            loader = new MapLoader(shapes(), loader);
        }
        Api api = (Api) Class.forName("L7W108HibernateNullArgsDriver", true, loader).getDeclaredConstructor()
            .newInstance();
        row("find-usage-null-descriptor", () -> api.findUsage(null, new java.util.HashMap<>()));
        row("has-direct-null-type", () -> api.hasDirect(api.newTarget(), null));
        row("indexed-for-each-null", () -> {
            api.indexedForEach(api.newList(), null);
            return "returned";
        });
    }
}
