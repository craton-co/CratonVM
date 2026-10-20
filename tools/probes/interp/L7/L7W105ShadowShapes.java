// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 105, lane L7 (i100-L7-native-shadow-npe-texts): the ANTLR,
// BouncyCastle and Lucene shadow probes (L7W100AntlrShadowNulls, L7W103BcShadowRefusals,
// L7W104LuceneEofRefusals) needed the library jars. This probe carries the minimal SHAPE of each
// shadowed method instead: classes with the library's names, the fields the native shadows read
// and small bodies, generated with java.lang.classfile, so no jar is needed. HotSpot runs the
// shapes' bytecode; CratonVM runs the registered shadows (ANTLR / BC are `Intrinsic`s, which
// win over class bytes), which must either give the same answer or, on a case they cannot
// handle, re-run that bytecode (`antlr_null_case_runs_bytecode`, `bc_register_rerun!`). The
// Lucene registrations are `Bridge`s: the shape's own bytes must win (99, where the Bridge
// would read the buffer and answer 3).
//
// Setup. The ANTLR and BC shim families are selected at boot from class-path witnesses
// (`app_shims.rs` `witness_resources`), so the shapes must be ON THE CLASS PATH, not defined
// later:
//   javac -d out L7W105ShadowShapes.java
//   java -cp out L7W105ShadowShapes --emit shapes          (any JVM: writes the shapes)
//   java -cp out:shapes L7W105ShadowShapes                 (HotSpot reference)
//   cratonvm -cp out:shapes L7W105ShadowShapes             (both modes; also --nojit)
// Without `shapes` on the class path the probe defines the same classes in a loader of its own
// and prints the same lines (stderr says so), but no shim family is selected, so that run is
// not evidence for the shadows.
//
// The NPE texts are JEP 358's for the shapes' own bytes (no LocalVariableTable: parameters read
// as "<parameterN>"); predicted below, the HotSpot run on the same shapes is the reference.
// HOTSPOT_EXPECTED_BEGIN (JDK 25; NPE texts predicted from JEP 358 for these bytes)
// transition-out-of-range -> java.lang.IndexOutOfBoundsException: Index 5 out of bounds for length 0
// transition-negative -> java.lang.IndexOutOfBoundsException: Index -1 out of bounds for length 0
// transition-warm -> [java.lang.IndexOutOfBoundsException: Index 5 out of bounds for length 0]
// transition-count -> 0
// set-contains-in -> true
// set-contains-out -> false
// notset-matches-in-vocab -> false
// notset-matches-outside -> true
// notset-null-set-out-of-vocab -> false
// notset-null-set-in-vocab -> java.lang.NullPointerException: Cannot invoke "org.antlr.v4.runtime.misc.IntervalSet.contains(int)" because "this.set" is null
// set-null-set -> java.lang.NullPointerException: Cannot invoke "org.antlr.v4.runtime.misc.IntervalSet.contains(int)" because "this.set" is null
// null-transitions-count -> java.lang.NullPointerException: Cannot invoke "java.util.List.size()" because "this.transitions" is null
// null-transitions-get -> java.lang.NullPointerException: Cannot invoke "java.util.List.get(int)" because "this.transitions" is null
// null-intervals-contains -> java.lang.NullPointerException: Cannot invoke "java.util.List.size()" because "this.intervals" is null
// array-context-null-parents -> java.lang.NullPointerException: Cannot read the array length because "<parameter1>" is null
// pack-le-short -> java.lang.ArrayIndexOutOfBoundsException: Index 3 out of bounds for length 3
// pack-be-negative -> java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 8
// pack-be-null -> java.lang.NullPointerException: Cannot load from byte/boolean array because "<parameter1>" is null
// pack-be-ok -> 16909060
// pack-int-array-partial -> java.lang.ArrayIndexOutOfBoundsException: Index 5 out of bounds for length 5
// pack-int-array-partial bytes 0102030405
// pack-to-int-array-partial -> java.lang.ArrayIndexOutOfBoundsException: Index 9 out of bounds for length 9
// pack-to-int-array-partial ints [1, 2, 0]
// key-parameter-ok -> 3
// key-parameter-past-end -> java.lang.ArrayIndexOutOfBoundsException: arraycopy: last source index 7 out of bounds for byte[4]
// key-parameter-negative-length -> java.lang.NegativeArraySizeException: -1
// key-parameter-null -> java.lang.NullPointerException: Cannot read the array length because "<parameter1>" is null
// sect113-add-short-z -> java.lang.ArrayIndexOutOfBoundsException: Index 1 out of bounds for length 1
// sect113-add-short-z z [5]
// sect113-add-ok -> [5, 10]
// lucene-readByte-at-2 -> 99
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassBuilder;
import java.lang.classfile.ClassFile;
import java.lang.classfile.CodeBuilder;
import java.lang.classfile.Label;
import java.lang.classfile.TypeKind;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.Set;
import java.util.TreeSet;
import java.util.function.Consumer;

public class L7W105ShadowShapes {

    /** What the generated driver does; every call site in it is an ordinary invoke of a shape. */
    public interface Api {
        Object newBasicState();

        Object transition(Object state, int i);

        int transitionCount(Object state);

        Object intervalSetOf(int a, int b);

        Object newSetTransition(Object target, Object set);

        Object newNotSetTransition(Object target, Object set);

        boolean matches(Object transition, int symbol, int min, int max);

        boolean contains(Object set, int symbol);

        Object newArrayContext(Object parents, int[] states);

        int packBigEndianToInt(byte[] bs, int off);

        int packLittleEndianToInt(byte[] bs, int off);

        void packIntToBigEndian(int[] ns, byte[] bs, int off);

        void packLittleEndianToIntArray(byte[] bs, int off, int[] ns);

        byte[] keyParameter(byte[] key);

        byte[] keyParameterRange(byte[] key, int off, int len);

        void sect113Add(long[] x, long[] y, long[] z);

        int luceneReadByteAt(long pos);
    }

    static final int PUB = ClassFile.ACC_PUBLIC;
    static final int PUB_SUPER = ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER;
    static final int PUB_STATIC = ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC;

    static final String A = "org.antlr.v4.runtime.";
    static final ClassDesc COMMON_TOKEN = ClassDesc.of(A + "CommonToken");
    static final ClassDesc ATN_STATE = ClassDesc.of(A + "atn.ATNState");
    static final ClassDesc BASIC_STATE = ClassDesc.of(A + "atn.BasicState");
    static final ClassDesc TRANSITION = ClassDesc.of(A + "atn.Transition");
    static final ClassDesc SET_TRANSITION = ClassDesc.of(A + "atn.SetTransition");
    static final ClassDesc NOT_SET_TRANSITION = ClassDesc.of(A + "atn.NotSetTransition");
    static final ClassDesc INTERVAL = ClassDesc.of(A + "misc.Interval");
    static final ClassDesc INTERVAL_SET = ClassDesc.of(A + "misc.IntervalSet");
    static final ClassDesc PC = ClassDesc.of(A + "atn.PredictionContext");
    static final ClassDesc ARRAY_PC = ClassDesc.of(A + "atn.ArrayPredictionContext");

    static final ClassDesc EC_POINT = ClassDesc.of("org.bouncycastle.math.ec.ECPoint");
    static final ClassDesc PACK = ClassDesc.of("org.bouncycastle.util.Pack");
    static final ClassDesc KEY_PARAMETER = ClassDesc.of("org.bouncycastle.crypto.params.KeyParameter");
    static final ClassDesc SECT113 = ClassDesc.of("org.bouncycastle.math.ec.custom.sec.SecT113Field");

    static final ClassDesc BBDI = ClassDesc.of("org.apache.lucene.store.ByteBuffersDataInput");

    static final ClassDesc DRIVER = ClassDesc.of("L7W105ShadowShapesDriver");
    static final ClassDesc API = ClassDesc.of("L7W105ShadowShapes$Api");

    static final ClassDesc CD_LIST = ClassDesc.of("java.util.List");
    static final ClassDesc CD_ARRAY_LIST = ClassDesc.of("java.util.ArrayList");
    static final ClassDesc CD_BYTE_BUFFER = ClassDesc.of("java.nio.ByteBuffer");
    static final ClassDesc CD_BYTES = ConstantDescs.CD_byte.arrayType();
    static final ClassDesc CD_INTS = ConstantDescs.CD_int.arrayType();
    static final ClassDesc CD_LONGS = ConstantDescs.CD_long.arrayType();

    static final ClassDesc I = ConstantDescs.CD_int;
    static final ClassDesc Z = ConstantDescs.CD_boolean;
    static final ClassDesc V = ConstantDescs.CD_void;
    static final ClassDesc J = ConstantDescs.CD_long;
    static final ClassDesc B = ConstantDescs.CD_byte;
    static final ClassDesc OBJ = ConstantDescs.CD_Object;

    static MethodTypeDesc mt(ClassDesc ret, ClassDesc... params) {
        return MethodTypeDesc.of(ret, params);
    }

    static byte[] cls(ClassDesc name, Consumer<ClassBuilder> body) {
        return cls(name, PUB_SUPER, body);
    }

    static byte[] cls(ClassDesc name, int flags, Consumer<ClassBuilder> body) {
        return ClassFile.of().build(name, cb -> {
            cb.withFlags(flags);
            cb.withSuperclass(OBJ);
            body.accept(cb);
        });
    }

    static final int PUB_ABSTRACT = PUB_SUPER | ClassFile.ACC_ABSTRACT;

    /** `<init>()V` calling `superClass.<init>()V`, then `rest`. */
    static void ctor(ClassBuilder cb, ClassDesc superClass, Consumer<CodeBuilder> rest) {
        cb.withMethodBody("<init>", mt(V), PUB, code -> {
            code.aload(0).invokespecial(superClass, "<init>", mt(V));
            rest.accept(code);
            code.return_();
        });
    }

    /** `bs[off] & 0xff` with `bs` in slot 0 and `off` in slot 1. */
    static void loadUnsigned(CodeBuilder c) {
        c.aload(0).iload(1).baload().sipush(255).iand();
    }

    static Map<String, byte[]> shapes() {
        Map<String, byte[]> out = new LinkedHashMap<>();

        // ---- ANTLR (shim witness: org/antlr/v4/runtime/CommonToken.class)
        out.put(A + "CommonToken", cls(COMMON_TOKEN, cb -> ctor(cb, OBJ, c -> {})));
        out.put(A + "atn.ATNState", cls(ATN_STATE, PUB_ABSTRACT, cb -> {
            cb.withField("transitions", CD_LIST, PUB);
            ctor(cb, OBJ, c -> c.aload(0).new_(CD_ARRAY_LIST).dup().iconst_4()
                .invokespecial(CD_ARRAY_LIST, "<init>", mt(V, I))
                .putfield(ATN_STATE, "transitions", CD_LIST));
            cb.withMethodBody("getNumberOfTransitions", mt(I), PUB, c -> c.aload(0)
                .getfield(ATN_STATE, "transitions", CD_LIST)
                .invokeinterface(CD_LIST, "size", mt(I)).ireturn());
            cb.withMethodBody("transition", mt(TRANSITION, I), PUB, c -> c.aload(0)
                .getfield(ATN_STATE, "transitions", CD_LIST).iload(1)
                .invokeinterface(CD_LIST, "get", mt(OBJ, I)).checkcast(TRANSITION).areturn());
        }));
        out.put(A + "atn.BasicState", cls(BASIC_STATE, cb -> {
            cb.withSuperclass(ATN_STATE);
            ctor(cb, ATN_STATE, c -> {});
        }));
        out.put(A + "misc.Interval", cls(INTERVAL, cb -> {
            cb.withField("a", I, PUB);
            cb.withField("b", I, PUB);
            cb.withMethodBody("<init>", mt(V, I, I), PUB, c -> c.aload(0)
                .invokespecial(OBJ, "<init>", mt(V))
                .aload(0).iload(1).putfield(INTERVAL, "a", I)
                .aload(0).iload(2).putfield(INTERVAL, "b", I).return_());
        }));
        out.put(A + "misc.IntervalSet", cls(INTERVAL_SET, cb -> {
            cb.withField("intervals", CD_LIST, PUB);
            ctor(cb, OBJ, c -> c.aload(0).new_(CD_ARRAY_LIST).dup().iconst_2()
                .invokespecial(CD_ARRAY_LIST, "<init>", mt(V, I))
                .putfield(INTERVAL_SET, "intervals", CD_LIST));
            cb.withMethodBody("of", mt(INTERVAL_SET, I, I), PUB_STATIC, c -> c
                .new_(INTERVAL_SET).dup().invokespecial(INTERVAL_SET, "<init>", mt(V)).astore(2)
                .aload(2).getfield(INTERVAL_SET, "intervals", CD_LIST)
                .new_(INTERVAL).dup().iload(0).iload(1).invokespecial(INTERVAL, "<init>", mt(V, I, I))
                .invokeinterface(CD_LIST, "add", mt(Z, OBJ)).pop()
                .aload(2).areturn());
            // ANTLR's binary search: n = intervals.size(); l = 0; r = n - 1; while (l <= r) {...}
            cb.withMethodBody("contains", mt(Z, I), PUB, c -> {
                Label loop = c.newLabel();
                Label notBelow = c.newLabel();
                Label found = c.newLabel();
                Label absent = c.newLabel();
                c.aload(0).getfield(INTERVAL_SET, "intervals", CD_LIST)
                    .invokeinterface(CD_LIST, "size", mt(I)).istore(2)
                    .iconst_0().istore(3)
                    .iload(2).iconst_1().isub().istore(4)
                    .labelBinding(loop)
                    .iload(3).iload(4).if_icmpgt(absent)
                    .iload(3).iload(4).iadd().iconst_2().idiv().istore(5)
                    .aload(0).getfield(INTERVAL_SET, "intervals", CD_LIST).iload(5)
                    .invokeinterface(CD_LIST, "get", mt(OBJ, I)).checkcast(INTERVAL).astore(6)
                    .aload(6).getfield(INTERVAL, "a", I).istore(7)
                    .aload(6).getfield(INTERVAL, "b", I).istore(8)
                    // if (b < el) l = m + 1
                    .iload(8).iload(1).if_icmpge(notBelow)
                    .iload(5).iconst_1().iadd().istore(3).goto_(loop)
                    .labelBinding(notBelow)
                    // else if (a > el) r = m - 1; else return true
                    .iload(7).iload(1).if_icmple(found)
                    .iload(5).iconst_1().isub().istore(4).goto_(loop)
                    .labelBinding(found)
                    .iconst_1().ireturn()
                    .labelBinding(absent)
                    .iconst_0().ireturn();
            });
        }));
        out.put(A + "atn.Transition", cls(TRANSITION, PUB_ABSTRACT, cb -> {
            cb.withField("target", ATN_STATE, PUB);
            cb.withMethodBody("<init>", mt(V, ATN_STATE), ClassFile.ACC_PROTECTED, c -> c.aload(0)
                .invokespecial(OBJ, "<init>", mt(V))
                .aload(0).aload(1).putfield(TRANSITION, "target", ATN_STATE).return_());
            cb.withMethod("matches", mt(Z, I, I, I), PUB | ClassFile.ACC_ABSTRACT, mb -> {});
        }));
        out.put(A + "atn.SetTransition", cls(SET_TRANSITION, cb -> {
            cb.withSuperclass(TRANSITION);
            cb.withField("set", INTERVAL_SET, PUB);
            cb.withMethodBody("<init>", mt(V, ATN_STATE, INTERVAL_SET), PUB, c -> c.aload(0).aload(1)
                .invokespecial(TRANSITION, "<init>", mt(V, ATN_STATE))
                .aload(0).aload(2).putfield(SET_TRANSITION, "set", INTERVAL_SET).return_());
            cb.withMethodBody("matches", mt(Z, I, I, I), PUB, c -> c.aload(0)
                .getfield(SET_TRANSITION, "set", INTERVAL_SET).iload(1)
                .invokevirtual(INTERVAL_SET, "contains", mt(Z, I)).ireturn());
        }));
        out.put(A + "atn.NotSetTransition", cls(NOT_SET_TRANSITION, cb -> {
            cb.withSuperclass(SET_TRANSITION);
            cb.withMethodBody("<init>", mt(V, ATN_STATE, INTERVAL_SET), PUB, c -> c.aload(0).aload(1).aload(2)
                .invokespecial(SET_TRANSITION, "<init>", mt(V, ATN_STATE, INTERVAL_SET)).return_());
            // symbol >= min && symbol <= max && !super.matches(symbol, min, max)
            cb.withMethodBody("matches", mt(Z, I, I, I), PUB, c -> {
                Label no = c.newLabel();
                c.iload(1).iload(2).if_icmplt(no)
                    .iload(1).iload(3).if_icmpgt(no)
                    .aload(0).iload(1).iload(2).iload(3)
                    .invokespecial(SET_TRANSITION, "matches", mt(Z, I, I, I)).ifne(no)
                    .iconst_1().ireturn()
                    .labelBinding(no)
                    .iconst_0().ireturn();
            });
        }));
        out.put(A + "atn.PredictionContext", cls(PC, PUB_ABSTRACT, cb -> {
            cb.withField("id", I, PUB);
            cb.withField("cachedHashCode", I, PUB);
            cb.withMethodBody("<init>", mt(V, I), ClassFile.ACC_PROTECTED, c -> c.aload(0)
                .invokespecial(OBJ, "<init>", mt(V))
                .aload(0).iload(1).putfield(PC, "cachedHashCode", I).return_());
            // h = 1; for (i = 0; i < parents.length; i++) h = 31 * h + i; return h + returnStates.length
            cb.withMethodBody("calculateHashCode", mt(I, PC.arrayType(), CD_INTS),
                ClassFile.ACC_PROTECTED | ClassFile.ACC_STATIC, c -> {
                    Label loop = c.newLabel();
                    Label done = c.newLabel();
                    c.iconst_1().istore(2).iconst_0().istore(3)
                        .labelBinding(loop)
                        .iload(3).aload(0).arraylength().if_icmpge(done)
                        .iload(2).bipush(31).imul().iload(3).iadd().istore(2)
                        .iinc(3, 1).goto_(loop)
                        .labelBinding(done)
                        .iload(2).aload(1).arraylength().iadd().ireturn();
                });
        }));
        out.put(A + "atn.ArrayPredictionContext", cls(ARRAY_PC, cb -> {
            cb.withSuperclass(PC);
            cb.withField("parents", PC.arrayType(), PUB);
            cb.withField("returnStates", CD_INTS, PUB);
            cb.withMethodBody("<init>", mt(V, PC.arrayType(), CD_INTS), PUB, c -> c.aload(0)
                .aload(1).aload(2).invokestatic(PC, "calculateHashCode", mt(I, PC.arrayType(), CD_INTS))
                .invokespecial(PC, "<init>", mt(V, I))
                .aload(0).aload(1).putfield(ARRAY_PC, "parents", PC.arrayType())
                .aload(0).aload(2).putfield(ARRAY_PC, "returnStates", CD_INTS).return_());
        }));

        // ---- BouncyCastle (shim witness: org/bouncycastle/math/ec/ECPoint.class)
        out.put("org.bouncycastle.math.ec.ECPoint", cls(EC_POINT, PUB_ABSTRACT, cb -> {
            ctor(cb, OBJ, c -> {});
        }));
        out.put("org.bouncycastle.util.Pack", cls(PACK, cb -> {
            // n = bs[off] << 24; n |= (bs[++off] & 0xff) << 16; ... << 8; n |= bs[++off] & 0xff
            cb.withMethodBody("bigEndianToInt", mt(I, CD_BYTES, I), PUB_STATIC, c -> {
                c.aload(0).iload(1).baload().bipush(24).ishl().istore(2);
                c.iinc(1, 1).iload(2);
                loadUnsigned(c);
                c.bipush(16).ishl().ior().istore(2);
                c.iinc(1, 1).iload(2);
                loadUnsigned(c);
                c.bipush(8).ishl().ior().istore(2);
                c.iinc(1, 1).iload(2);
                loadUnsigned(c);
                c.ior().istore(2);
                c.iload(2).ireturn();
            });
            // n = bs[off] & 0xff; n |= (bs[++off] & 0xff) << 8; ... << 16; n |= bs[++off] << 24
            cb.withMethodBody("littleEndianToInt", mt(I, CD_BYTES, I), PUB_STATIC, c -> {
                loadUnsigned(c);
                c.istore(2);
                c.iinc(1, 1).iload(2);
                loadUnsigned(c);
                c.bipush(8).ishl().ior().istore(2);
                c.iinc(1, 1).iload(2);
                loadUnsigned(c);
                c.bipush(16).ishl().ior().istore(2);
                c.iinc(1, 1).iload(2).aload(0).iload(1).baload().bipush(24).ishl().ior().istore(2);
                c.iload(2).ireturn();
            });
            cb.withMethodBody("intToBigEndian", mt(V, I, CD_BYTES, I), PUB_STATIC, c -> {
                c.aload(1).iload(2).iload(0).bipush(24).iushr().i2b().bastore();
                c.iinc(2, 1).aload(1).iload(2).iload(0).bipush(16).iushr().i2b().bastore();
                c.iinc(2, 1).aload(1).iload(2).iload(0).bipush(8).iushr().i2b().bastore();
                c.iinc(2, 1).aload(1).iload(2).iload(0).i2b().bastore();
                c.return_();
            });
            // for (i = 0; i < ns.length; ++i) { intToBigEndian(ns[i], bs, off); off += 4; }
            cb.withMethodBody("intToBigEndian", mt(V, CD_INTS, CD_BYTES, I), PUB_STATIC, c -> {
                Label loop = c.newLabel();
                Label done = c.newLabel();
                c.iconst_0().istore(3)
                    .labelBinding(loop)
                    .iload(3).aload(0).arraylength().if_icmpge(done)
                    .aload(0).iload(3).iaload().aload(1).iload(2)
                    .invokestatic(PACK, "intToBigEndian", mt(V, I, CD_BYTES, I))
                    .iinc(2, 4).iinc(3, 1).goto_(loop)
                    .labelBinding(done)
                    .return_();
            });
            // for (i = 0; i < ns.length; ++i) { ns[i] = littleEndianToInt(bs, off); off += 4; }
            cb.withMethodBody("littleEndianToInt", mt(V, CD_BYTES, I, CD_INTS), PUB_STATIC, c -> {
                Label loop = c.newLabel();
                Label done = c.newLabel();
                c.iconst_0().istore(3)
                    .labelBinding(loop)
                    .iload(3).aload(2).arraylength().if_icmpge(done)
                    .aload(2).iload(3).aload(0).iload(1)
                    .invokestatic(PACK, "littleEndianToInt", mt(I, CD_BYTES, I)).iastore()
                    .iinc(1, 4).iinc(3, 1).goto_(loop)
                    .labelBinding(done)
                    .return_();
            });
        }));
        out.put("org.bouncycastle.crypto.params.KeyParameter", cls(KEY_PARAMETER, cb -> {
            cb.withField("key", CD_BYTES, ClassFile.ACC_PRIVATE);
            // this(key, 0, key.length)
            cb.withMethodBody("<init>", mt(V, CD_BYTES), PUB, c -> c.aload(0).aload(1).iconst_0()
                .aload(1).arraylength().invokespecial(KEY_PARAMETER, "<init>", mt(V, CD_BYTES, I, I)).return_());
            // this(keyLen); System.arraycopy(key, keyOff, this.key, 0, keyLen)
            cb.withMethodBody("<init>", mt(V, CD_BYTES, I, I), PUB, c -> c.aload(0).iload(3)
                .invokespecial(KEY_PARAMETER, "<init>", mt(V, I))
                .aload(1).iload(2).aload(0).getfield(KEY_PARAMETER, "key", CD_BYTES).iconst_0().iload(3)
                .invokestatic(ClassDesc.of("java.lang.System"), "arraycopy", mt(V, OBJ, I, OBJ, I, I))
                .return_());
            cb.withMethodBody("<init>", mt(V, I), ClassFile.ACC_PRIVATE, c -> c.aload(0)
                .invokespecial(OBJ, "<init>", mt(V))
                .aload(0).iload(1).newarray(TypeKind.BYTE).putfield(KEY_PARAMETER, "key", CD_BYTES).return_());
            cb.withMethodBody("getKey", mt(CD_BYTES), PUB, c -> c.aload(0)
                .getfield(KEY_PARAMETER, "key", CD_BYTES).areturn());
        }));
        out.put("org.bouncycastle.math.ec.custom.sec.SecT113Field", cls(SECT113, cb -> {
            // z[0] = x[0] ^ y[0]; z[1] = x[1] ^ y[1]
            cb.withMethodBody("add", mt(V, CD_LONGS, CD_LONGS, CD_LONGS), PUB_STATIC, c -> c
                .aload(2).iconst_0().aload(0).iconst_0().laload().aload(1).iconst_0().laload().lxor().lastore()
                .aload(2).iconst_1().aload(0).iconst_1().laload().aload(1).iconst_1().laload().lxor().lastore()
                .return_());
        }));

        // ---- Lucene (Bridges: the shape's bytes must win)
        out.put("org.apache.lucene.store.ByteBuffersDataInput", cls(BBDI, cb -> {
            cb.withField("blocks", CD_BYTE_BUFFER.arrayType(), ClassFile.ACC_PRIVATE);
            cb.withField("blockBits", I, ClassFile.ACC_PRIVATE);
            cb.withField("blockMask", I, ClassFile.ACC_PRIVATE);
            cb.withField("size", J, ClassFile.ACC_PRIVATE);
            cb.withField("offset", J, ClassFile.ACC_PRIVATE);
            cb.withField("pos", J, ClassFile.ACC_PRIVATE);
            // One 16-byte block holding {1, 2, ..., 8}: the Bridge would answer 3 for readByte(2).
            ctor(cb, OBJ, c -> {
                Label loop = c.newLabel();
                Label done = c.newLabel();
                c.bipush(8).newarray(TypeKind.BYTE).astore(1)
                    .iconst_0().istore(2)
                    .labelBinding(loop)
                    .iload(2).bipush(8).if_icmpge(done)
                    .aload(1).iload(2).iload(2).iconst_1().iadd().i2b().bastore()
                    .iinc(2, 1).goto_(loop)
                    .labelBinding(done)
                    .aload(0).iconst_1().anewarray(CD_BYTE_BUFFER).dup().iconst_0().aload(1)
                    .invokestatic(CD_BYTE_BUFFER, "wrap", mt(CD_BYTE_BUFFER, CD_BYTES)).aastore()
                    .putfield(BBDI, "blocks", CD_BYTE_BUFFER.arrayType())
                    .aload(0).iconst_4().putfield(BBDI, "blockBits", I)
                    .aload(0).bipush(15).putfield(BBDI, "blockMask", I)
                    .aload(0).loadConstant(8L).putfield(BBDI, "size", J)
                    .aload(0).lconst_0().putfield(BBDI, "offset", J);
            });
            cb.withMethodBody("readByte", mt(B, J), PUB, c -> c.bipush(99).ireturn());
        }));

        // ---- The driver: ordinary invokes of the shapes (interpreted, then compiled).
        out.put("L7W105ShadowShapesDriver", cls(DRIVER, cb -> {
            cb.withInterfaceSymbols(API);
            ctor(cb, OBJ, c -> {});
            cb.withMethodBody("newBasicState", mt(OBJ), PUB, c -> c.new_(BASIC_STATE).dup()
                .invokespecial(BASIC_STATE, "<init>", mt(V)).areturn());
            cb.withMethodBody("transition", mt(OBJ, OBJ, I), PUB, c -> c.aload(1).checkcast(ATN_STATE).iload(2)
                .invokevirtual(ATN_STATE, "transition", mt(TRANSITION, I)).areturn());
            cb.withMethodBody("transitionCount", mt(I, OBJ), PUB, c -> c.aload(1).checkcast(ATN_STATE)
                .invokevirtual(ATN_STATE, "getNumberOfTransitions", mt(I)).ireturn());
            cb.withMethodBody("intervalSetOf", mt(OBJ, I, I), PUB, c -> c.iload(1).iload(2)
                .invokestatic(INTERVAL_SET, "of", mt(INTERVAL_SET, I, I)).areturn());
            cb.withMethodBody("newSetTransition", mt(OBJ, OBJ, OBJ), PUB, c -> c.new_(SET_TRANSITION).dup()
                .aload(1).checkcast(ATN_STATE).aload(2).checkcast(INTERVAL_SET)
                .invokespecial(SET_TRANSITION, "<init>", mt(V, ATN_STATE, INTERVAL_SET)).areturn());
            cb.withMethodBody("newNotSetTransition", mt(OBJ, OBJ, OBJ), PUB, c -> c.new_(NOT_SET_TRANSITION).dup()
                .aload(1).checkcast(ATN_STATE).aload(2).checkcast(INTERVAL_SET)
                .invokespecial(NOT_SET_TRANSITION, "<init>", mt(V, ATN_STATE, INTERVAL_SET)).areturn());
            cb.withMethodBody("matches", mt(Z, OBJ, I, I, I), PUB, c -> c.aload(1).checkcast(TRANSITION)
                .iload(2).iload(3).iload(4).invokevirtual(TRANSITION, "matches", mt(Z, I, I, I)).ireturn());
            cb.withMethodBody("contains", mt(Z, OBJ, I), PUB, c -> c.aload(1).checkcast(INTERVAL_SET).iload(2)
                .invokevirtual(INTERVAL_SET, "contains", mt(Z, I)).ireturn());
            cb.withMethodBody("newArrayContext", mt(OBJ, OBJ, CD_INTS), PUB, c -> c.new_(ARRAY_PC).dup()
                .aload(1).checkcast(PC.arrayType()).aload(2)
                .invokespecial(ARRAY_PC, "<init>", mt(V, PC.arrayType(), CD_INTS)).areturn());
            cb.withMethodBody("packBigEndianToInt", mt(I, CD_BYTES, I), PUB, c -> c.aload(1).iload(2)
                .invokestatic(PACK, "bigEndianToInt", mt(I, CD_BYTES, I)).ireturn());
            cb.withMethodBody("packLittleEndianToInt", mt(I, CD_BYTES, I), PUB, c -> c.aload(1).iload(2)
                .invokestatic(PACK, "littleEndianToInt", mt(I, CD_BYTES, I)).ireturn());
            cb.withMethodBody("packIntToBigEndian", mt(V, CD_INTS, CD_BYTES, I), PUB, c -> c.aload(1).aload(2)
                .iload(3).invokestatic(PACK, "intToBigEndian", mt(V, CD_INTS, CD_BYTES, I)).return_());
            cb.withMethodBody("packLittleEndianToIntArray", mt(V, CD_BYTES, I, CD_INTS), PUB, c -> c.aload(1)
                .iload(2).aload(3).invokestatic(PACK, "littleEndianToInt", mt(V, CD_BYTES, I, CD_INTS)).return_());
            cb.withMethodBody("keyParameter", mt(CD_BYTES, CD_BYTES), PUB, c -> c.new_(KEY_PARAMETER).dup()
                .aload(1).invokespecial(KEY_PARAMETER, "<init>", mt(V, CD_BYTES))
                .invokevirtual(KEY_PARAMETER, "getKey", mt(CD_BYTES)).areturn());
            cb.withMethodBody("keyParameterRange", mt(CD_BYTES, CD_BYTES, I, I), PUB, c -> c.new_(KEY_PARAMETER)
                .dup().aload(1).iload(2).iload(3).invokespecial(KEY_PARAMETER, "<init>", mt(V, CD_BYTES, I, I))
                .invokevirtual(KEY_PARAMETER, "getKey", mt(CD_BYTES)).areturn());
            cb.withMethodBody("sect113Add", mt(V, CD_LONGS, CD_LONGS, CD_LONGS), PUB, c -> c.aload(1).aload(2)
                .aload(3).invokestatic(SECT113, "add", mt(V, CD_LONGS, CD_LONGS, CD_LONGS)).return_());
            cb.withMethodBody("luceneReadByteAt", mt(I, J), PUB, c -> c.new_(BBDI).dup()
                .invokespecial(BBDI, "<init>", mt(V)).lload(1).invokevirtual(BBDI, "readByte", mt(B, J)).ireturn());
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

    static String answer(Row r) {
        try {
            return String.valueOf(r.run());
        } catch (Throwable t) {
            return t.toString();
        }
    }

    static void row(String name, Row r) {
        System.out.println(name + " -> " + answer(r));
    }

    static String hex(byte[] b) {
        StringBuilder sb = new StringBuilder();
        for (byte x : b) {
            sb.append(String.format("%02x", x & 0xff));
        }
        return sb.toString();
    }

    static void nullField(ClassLoader loader, String owner, String field, Object target) throws Exception {
        Class.forName(owner, false, loader).getField(field).set(target, null);
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
        ClassLoader loader = L7W105ShadowShapes.class.getClassLoader();
        if (loader.getResource("org/antlr/v4/runtime/CommonToken.class") == null) {
            System.err.println("L7W105ShadowShapes: shapes not on the class path (run with --emit <dir> and add"
                + " <dir> to -cp); defining them in a loader of the probe's own, no shim family selected");
            loader = new MapLoader(shapes(), loader);
        }
        Api api = (Api) Class.forName("L7W105ShadowShapesDriver", true, loader)
            .getDeclaredConstructor().newInstance();

        // ANTLR: ATNState.transition / getNumberOfTransitions
        Object empty = api.newBasicState();
        row("transition-out-of-range", () -> api.transition(empty, 5));
        row("transition-negative", () -> api.transition(empty, -1));
        Set<String> warm = new TreeSet<>();
        for (int i = 0; i < 20000; i++) {
            warm.add(answer(() -> api.transition(empty, 5)));
        }
        System.out.println("transition-warm -> " + warm);
        row("transition-count", () -> api.transitionCount(empty));

        // ANTLR: IntervalSet.contains, SetTransition / NotSetTransition.matches
        Object target = api.newBasicState();
        row("set-contains-in", () -> api.contains(api.intervalSetOf(3, 4), 4));
        row("set-contains-out", () -> api.contains(api.intervalSetOf(3, 4), 5));
        Object notSetOk = api.newNotSetTransition(target, api.intervalSetOf(3, 4));
        row("notset-matches-in-vocab", () -> api.matches(notSetOk, 3, 1, 10));
        row("notset-matches-outside", () -> api.matches(notSetOk, 5, 1, 10));
        Object notSet = api.newNotSetTransition(target, api.intervalSetOf(3, 4));
        nullField(loader, A + "atn.SetTransition", "set", notSet);
        row("notset-null-set-out-of-vocab", () -> api.matches(notSet, 50, 1, 10));
        row("notset-null-set-in-vocab", () -> api.matches(notSet, 5, 1, 10));
        Object set = api.newSetTransition(target, api.intervalSetOf(3, 4));
        nullField(loader, A + "atn.SetTransition", "set", set);
        row("set-null-set", () -> api.matches(set, 5, 1, 10));

        Object noList = api.newBasicState();
        nullField(loader, A + "atn.ATNState", "transitions", noList);
        row("null-transitions-count", () -> api.transitionCount(noList));
        row("null-transitions-get", () -> api.transition(noList, 0));

        Object intervals = api.intervalSetOf(1, 2);
        nullField(loader, A + "misc.IntervalSet", "intervals", intervals);
        row("null-intervals-contains", () -> api.contains(intervals, 1));

        row("array-context-null-parents", () -> api.newArrayContext(null, new int[] {1}));

        // BouncyCastle: Pack, KeyParameter, SecT113Field
        row("pack-le-short", () -> api.packLittleEndianToInt(new byte[3], 0));
        row("pack-be-negative", () -> api.packBigEndianToInt(new byte[8], -1));
        row("pack-be-null", () -> api.packBigEndianToInt(null, 0));
        row("pack-be-ok", () -> api.packBigEndianToInt(new byte[] {1, 2, 3, 4}, 0));
        byte[] out = new byte[5];
        row("pack-int-array-partial", () -> {
            api.packIntToBigEndian(new int[] {0x01020304, 0x05060708}, out, 0);
            return "ok";
        });
        System.out.println("pack-int-array-partial bytes " + hex(out));
        int[] ns = new int[3];
        row("pack-to-int-array-partial", () -> {
            api.packLittleEndianToIntArray(new byte[] {1, 0, 0, 0, 2, 0, 0, 0, 3}, 0, ns);
            return "ok";
        });
        System.out.println("pack-to-int-array-partial ints " + Arrays.toString(ns));

        row("key-parameter-ok", () -> api.keyParameter(new byte[] {1, 2, 3}).length);
        row("key-parameter-past-end", () -> api.keyParameterRange(new byte[4], 2, 5).length);
        row("key-parameter-negative-length", () -> api.keyParameterRange(new byte[4], 0, -1).length);
        row("key-parameter-null", () -> api.keyParameter(null).length);

        long[] shortZ = new long[1];
        row("sect113-add-short-z", () -> {
            api.sect113Add(new long[] {1, 2}, new long[] {4, 8}, shortZ);
            return "ok";
        });
        System.out.println("sect113-add-short-z z " + Arrays.toString(shortZ));
        long[] z = new long[2];
        row("sect113-add-ok", () -> {
            api.sect113Add(new long[] {1, 2}, new long[] {4, 8}, z);
            return Arrays.toString(z);
        });

        // Lucene: a Bridge stands in front of these bytes; the bytes must win.
        row("lucene-readByte-at-2", () -> api.luceneReadByteAt(2L));
    }
}
