// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 107, lane L7
// (i107-L7-other-plain-shadows-answer-a-null-argument-with-their-own-npe, the ANTLR half): a
// null REQUIRED argument to an ANTLR `Intrinsic` shadow (`antlr_intrinsics.rs`,
// `antlr_null_required_arg_runs_bytecode`) runs ANTLR's bytecode, never the shadow body, whose
// `obj_arg` answered "java.lang.NullPointerException: null object argument". Self-contained
// SHAPES generated with java.lang.classfile (classes with ANTLR's names and descriptors, small
// bodies), so no antlr4-runtime jar is needed.
//
// Setup (the AppIntrinsics family is selected at boot from the class-path witness
// `org/antlr/v4/runtime/CommonToken.class`, which --emit writes too):
//   javac -d out L7W107AntlrNullArgs.java
//   java -cp out L7W107AntlrNullArgs --emit shapes          (any JVM: writes the shapes)
//   java -cp out:shapes L7W107AntlrNullArgs                 (HotSpot reference)
//   cratonvm [--compatible] [--nojit] -cp out:shapes L7W107AntlrNullArgs
// Evidence that the shadows ran and handed back:
//   CRATONVM_DBG_SHADOW_RERUN=1 cratonvm --nojit -cp out:shapes L7W107AntlrNullArgs 2>err.txt
//   grep -c "^\[shadow-rerun\] org/antlr/" err.txt     (expect 4, "the ANTLR shadow's null case")
// Before wave 107 every row printed "java.lang.NullPointerException: null object argument".
// Without `shapes` on the class path the probe defines the classes in a loader of its own: no
// shim family is selected, so that run is not evidence.
// HOTSPOT_EXPECTED_BEGIN (JDK 25; NPE texts predicted from JEP 358 for these bytes)
// merge-null-a -> java.lang.NullPointerException: Cannot invoke "Object.equals(Object)" because "<parameter1>" is null
// merge-null-b -> java.lang.NullPointerException: Cannot invoke "Object.hashCode()" because "<parameter2>" is null
// configset-add-null -> java.lang.NullPointerException: Cannot read field "alt" because "<parameter1>" is null
// arraypc-init-null-singleton -> java.lang.NullPointerException: Cannot read field "returnState" because "<parameter1>" is null
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

public class L7W107AntlrNullArgs {

    /** What the generated driver does; every call site in it is an ordinary invoke of a shape. */
    public interface Api {
        Object merge(Object a, Object b);

        Object newConfigSet();

        boolean add(Object set, Object config);

        Object newArrayPc(Object singleton);

        Object newSingleton();
    }

    static final int PUB = ClassFile.ACC_PUBLIC;
    static final int PUB_STATIC = ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC;
    static final int PUB_SUPER = ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER;
    static final int PUB_ABSTRACT = PUB_SUPER | ClassFile.ACC_ABSTRACT;

    static final String RT = "org.antlr.v4.runtime.";
    static final ClassDesc COMMON_TOKEN = ClassDesc.of(RT + "CommonToken");
    static final ClassDesc PC = ClassDesc.of(RT + "atn.PredictionContext");
    static final ClassDesc SINGLETON_PC = ClassDesc.of(RT + "atn.SingletonPredictionContext");
    static final ClassDesc ARRAY_PC = ClassDesc.of(RT + "atn.ArrayPredictionContext");
    static final ClassDesc DOUBLE_KEY_MAP = ClassDesc.of(RT + "misc.DoubleKeyMap");
    static final ClassDesc ATN_CONFIG = ClassDesc.of(RT + "atn.ATNConfig");
    static final ClassDesc ATN_CONFIG_SET = ClassDesc.of(RT + "atn.ATNConfigSet");
    static final ClassDesc DRIVER = ClassDesc.of("L7W107AntlrNullArgsDriver");
    static final ClassDesc API = ClassDesc.of("L7W107AntlrNullArgs$Api");

    static final ClassDesc OBJ = ConstantDescs.CD_Object;
    static final ClassDesc I = ConstantDescs.CD_int;
    static final ClassDesc Z = ConstantDescs.CD_boolean;
    static final ClassDesc V = ConstantDescs.CD_void;

    static final MethodTypeDesc MERGE = MethodTypeDesc.of(PC, PC, PC, Z, DOUBLE_KEY_MAP);
    static final MethodTypeDesc ADD = MethodTypeDesc.of(Z, ATN_CONFIG);
    static final MethodTypeDesc ARRAY_FROM_SINGLETON = MethodTypeDesc.of(V, SINGLETON_PC);

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
        out.put(RT + "CommonToken", cls(COMMON_TOKEN, PUB_SUPER, OBJ, cb -> ctor(cb, OBJ)));
        out.put(RT + "misc.DoubleKeyMap", cls(DOUBLE_KEY_MAP, PUB_SUPER, OBJ, cb -> ctor(cb, OBJ)));
        // PredictionContext.merge(a, b, rootIsWildcard, mergeCache): if (a.equals(b)) return a;
        // b.hashCode(); return a.
        out.put(RT + "atn.PredictionContext", cls(PC, PUB_ABSTRACT, OBJ, cb -> {
            ctor(cb, OBJ);
            cb.withMethodBody("merge", MERGE, PUB_STATIC, c -> c
                .aload(0).aload(1).invokevirtual(OBJ, "equals", mt(Z, OBJ)).pop()
                .aload(1).invokevirtual(OBJ, "hashCode", mt(I)).pop()
                .aload(0).areturn());
        }));
        out.put(RT + "atn.SingletonPredictionContext", cls(SINGLETON_PC, PUB_SUPER, PC, cb -> {
            cb.withField("returnState", I, PUB);
            ctor(cb, PC);
        }));
        // ArrayPredictionContext(SingletonPredictionContext a): super(); this.n = a.returnState.
        out.put(RT + "atn.ArrayPredictionContext", cls(ARRAY_PC, PUB_SUPER, PC, cb -> {
            cb.withField("n", I, PUB);
            cb.withMethodBody("<init>", ARRAY_FROM_SINGLETON, PUB, c -> c
                .aload(0).invokespecial(PC, "<init>", mt(V))
                .aload(0).aload(1).getfield(SINGLETON_PC, "returnState", I).putfield(ARRAY_PC, "n", I)
                .return_());
        }));
        out.put(RT + "atn.ATNConfig", cls(ATN_CONFIG, PUB_SUPER, OBJ, cb -> {
            cb.withField("alt", I, PUB);
            ctor(cb, OBJ);
        }));
        // ATNConfigSet.add(ATNConfig config): return config.alt != 0 (reads the field first).
        out.put(RT + "atn.ATNConfigSet", cls(ATN_CONFIG_SET, PUB_SUPER, OBJ, cb -> {
            ctor(cb, OBJ);
            cb.withMethodBody("add", ADD, PUB, c -> c.aload(1).getfield(ATN_CONFIG, "alt", I).ireturn());
        }));
        out.put("L7W107AntlrNullArgsDriver", cls(DRIVER, PUB_SUPER, OBJ, cb -> {
            cb.withInterfaceSymbols(API);
            ctor(cb, OBJ);
            cb.withMethodBody("merge", mt(OBJ, OBJ, OBJ), PUB, c -> c
                .aload(1).checkcast(PC).aload(2).checkcast(PC).iconst_0().aconst_null()
                .invokestatic(PC, "merge", MERGE).areturn());
            cb.withMethodBody("newConfigSet", mt(OBJ), PUB, c -> c.new_(ATN_CONFIG_SET).dup()
                .invokespecial(ATN_CONFIG_SET, "<init>", mt(V)).areturn());
            cb.withMethodBody("add", mt(Z, OBJ, OBJ), PUB, c -> c
                .aload(1).checkcast(ATN_CONFIG_SET).aload(2).checkcast(ATN_CONFIG)
                .invokevirtual(ATN_CONFIG_SET, "add", ADD).ireturn());
            cb.withMethodBody("newArrayPc", mt(OBJ, OBJ), PUB, c -> c.new_(ARRAY_PC).dup()
                .aload(1).checkcast(SINGLETON_PC)
                .invokespecial(ARRAY_PC, "<init>", ARRAY_FROM_SINGLETON).areturn());
            cb.withMethodBody("newSingleton", mt(OBJ), PUB, c -> c.new_(SINGLETON_PC).dup()
                .invokespecial(SINGLETON_PC, "<init>", mt(V)).areturn());
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
            a = v == null ? "null" : v.getClass().getName();
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
        ClassLoader loader = L7W107AntlrNullArgs.class.getClassLoader();
        if (loader.getResource("org/antlr/v4/runtime/CommonToken.class") == null) {
            System.err.println("L7W107AntlrNullArgs: shapes not on the class path (run with --emit <dir> and"
                + " add <dir> to -cp); defining them in a loader of the probe's own, no shim family selected");
            loader = new MapLoader(shapes(), loader);
        }
        Api api = (Api) Class.forName("L7W107AntlrNullArgsDriver", true, loader).getDeclaredConstructor()
            .newInstance();
        row("merge-null-a", () -> api.merge(null, api.newSingleton()));
        row("merge-null-b", () -> api.merge(api.newSingleton(), null));
        row("configset-add-null", () -> api.add(api.newConfigSet(), null));
        row("arraypc-init-null-singleton", () -> api.newArrayPc(null));
    }
}
