// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 106, lane L7 (i100-L7-native-shadow-npe-texts, the jar-only residue
// of L7W105ShadowShapes): BouncyCastle's WhirlpoolDigest.update([BII)V,
// DigestRandomGenerator.nextBytes([B)V / ([BII)V and CBCBlockCipher.processBlock([BI[BI)I
// reduced to self-contained SHAPES generated with java.lang.classfile (classes with BC's
// names, the fields the native shadows read, small bodies), so no bcprov jar is needed.
// HotSpot runs the shapes' bytecode. CratonVM runs the registered `Intrinsic` shadows
// (`register_bc_whirlpool_digest`, `register_bc_digest_random_generator`,
// `register_bc_cbc_block_cipher`), which must either answer as the bytecode (the positive CBC
// rows, through a non-AES engine) or, on every case they cannot handle (a null array, a
// negative offset or length, a range past the end, a null `cipher`), hand the call back to that
// bytecode before writing anything, so the exception, its text and the partial effects (the
// `state=` / `pos=` / `v=` columns) are the bytecode's.
// Row `drg-null-one-arg` was "java.lang.NullPointerException: null object argument" before
// wave 106 (the `([B)V` native was not wrapped).
//
// Setup (the BC shim family is selected at boot from the class-path witness
// `org/bouncycastle/math/ec/ECPoint.class`, which --emit writes too):
//   javac -d out L7W106ShadowShapes.java
//   java -cp out L7W106ShadowShapes --emit shapes          (any JVM: writes the shapes)
//   java -cp out:shapes L7W106ShadowShapes                 (HotSpot reference)
//   cratonvm [--compatible] [--nojit] -cp out:shapes L7W106ShadowShapes
// Evidence that the shadows ran (not just the bytes):
//   CRATONVM_DBG_SHADOW_RERUN=1 cratonvm --nojit -cp out:shapes L7W106ShadowShapes 2>err.txt
//   grep -c '^\[shadow-rerun\] org/bouncycastle/' err.txt     (expect 16: every row but wp-negative-len, cbc-encrypt-ok, cbc-decrypt-ok, cbc-short-in)
//   CRATONVM_DBG_FUNNEL_INTRINSIC_CENSUS=1 cratonvm --nojit -cp out:shapes L7W106ShadowShapes 2>err.txt
//   grep 'funnel-intrinsic-census\]   org/bouncycastle/' err.txt   (dispatches >= 1 per shadow;
//   the column is a floor, so 0 is not proof of absence)
// Without `shapes` on the class path the probe defines the same classes in a loader of its own
// and prints the same lines (stderr says so), but no shim family is selected, so that run is
// not evidence.
//
// The NPE texts are JEP 358's for the shapes' own bytes (no LocalVariableTable: parameters read
// as "<parameterN>"); the HotSpot run on the emitted shapes is the reference.
// HOTSPOT_EXPECTED_BEGIN (JDK 25; NPE texts predicted from JEP 358 for these bytes)
// wp-negative-off -> java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 4 | pos=0 buf=0000
// wp-past-end -> java.lang.ArrayIndexOutOfBoundsException: Index 4 out of bounds for length 4 | pos=2 buf=0304
// wp-null-input -> java.lang.NullPointerException: Cannot load from byte/boolean array because "<parameter1>" is null | pos=0 buf=0000
// wp-null-zero-len -> ok | pos=0 buf=0000
// wp-negative-len -> ok | pos=0 buf=0000
// drg-negative-start -> java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 4 | counter=2 bytes=00000000
// drg-negative-len -> java.lang.ArrayIndexOutOfBoundsException: Index 4 out of bounds for length 4 | counter=3 bytes=02030403
// drg-past-end -> java.lang.ArrayIndexOutOfBoundsException: Index 4 out of bounds for length 4 | counter=2 bytes=00000203
// drg-null-zero-len -> ok | counter=2 bytes=-
// drg-null-len1 -> java.lang.NullPointerException: Cannot store to byte/boolean array because "<parameter1>" is null | counter=2 bytes=-
// drg-null-one-arg -> java.lang.NullPointerException: Cannot read the array length because "<parameter1>" is null | counter=1 bytes=-
// cbc-encrypt-ok -> 4 | out=4b78691e v=4b78691e
// cbc-decrypt-ok -> 4 | out=4b78691e v=10203040
// cbc-null-in -> java.lang.NullPointerException: Cannot read the array length because "<parameter1>" is null | out=00000000 v=01020304
// cbc-null-out -> java.lang.NullPointerException: Cannot store to byte/boolean array because "<parameter3>" is null | out=- v=11223344
// cbc-negative-in -> java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 8 | out=00000000 v=01020304
// cbc-negative-out -> java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 4 | out=00000000 v=11223344
// cbc-null-cipher -> java.lang.NullPointerException: Cannot invoke "org.bouncycastle.crypto.BlockCipher.processBlock(byte[], int, byte[], int)" because "this.cipher" is null | out=00000000 v=11223344
// cbc-short-in -> org.bouncycastle.crypto.DataLengthException: input buffer too short | out=00000000 v=01020304
// cbc-decrypt-negative-in -> java.lang.ArrayIndexOutOfBoundsException: arraycopy: source index -1 out of bounds for byte[8] | out=00000000 v=01020304
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
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.function.Consumer;

public class L7W106ShadowShapes {

    /** What the generated driver does; every call site in it is an ordinary invoke of a shape. */
    public interface Api {
        Object newWhirlpool();

        void wpUpdate(Object d, byte[] in, int off, int len);

        Object newDrg();

        void drgNext(Object g, byte[] bytes);

        void drgNext3(Object g, byte[] bytes, int start, int len);

        Object newXorEngine();

        Object newCbc(Object engine, boolean encrypting);

        int cbcProcess(Object cbc, byte[] in, int inOff, byte[] out, int outOff);
    }

    static final int PUB = ClassFile.ACC_PUBLIC;
    static final int PUB_SUPER = ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER;
    static final int PUB_ABSTRACT = PUB_SUPER | ClassFile.ACC_ABSTRACT;
    static final int PUB_INTERFACE = ClassFile.ACC_PUBLIC | ClassFile.ACC_INTERFACE | ClassFile.ACC_ABSTRACT;

    static final ClassDesc EC_POINT = ClassDesc.of("org.bouncycastle.math.ec.ECPoint");
    static final ClassDesc WHIRLPOOL = ClassDesc.of("org.bouncycastle.crypto.digests.WhirlpoolDigest");
    static final ClassDesc DRG = ClassDesc.of("org.bouncycastle.crypto.prng.DigestRandomGenerator");
    static final ClassDesc BLOCK_CIPHER = ClassDesc.of("org.bouncycastle.crypto.BlockCipher");
    static final ClassDesc DLE = ClassDesc.of("org.bouncycastle.crypto.DataLengthException");
    static final ClassDesc CBC = ClassDesc.of("org.bouncycastle.crypto.modes.CBCBlockCipher");
    static final ClassDesc XOR_ENGINE = ClassDesc.of("L7W106XorEngine");

    static final ClassDesc DRIVER = ClassDesc.of("L7W106ShadowShapesDriver");
    static final ClassDesc API = ClassDesc.of("L7W106ShadowShapes$Api");

    static final ClassDesc CD_BYTES = ConstantDescs.CD_byte.arrayType();
    static final ClassDesc CD_SYSTEM = ClassDesc.of("java.lang.System");
    static final ClassDesc CD_RTE = ClassDesc.of("java.lang.RuntimeException");
    static final ClassDesc CD_STRING = ConstantDescs.CD_String;

    static final ClassDesc I = ConstantDescs.CD_int;
    static final ClassDesc Z = ConstantDescs.CD_boolean;
    static final ClassDesc V = ConstantDescs.CD_void;
    static final ClassDesc J = ConstantDescs.CD_long;
    static final ClassDesc B = ConstantDescs.CD_byte;
    static final ClassDesc OBJ = ConstantDescs.CD_Object;

    static final MethodTypeDesc PROCESS_BLOCK = MethodTypeDesc.of(I, CD_BYTES, I, CD_BYTES, I);
    static final MethodTypeDesc ARRAYCOPY = MethodTypeDesc.of(V, OBJ, I, OBJ, I, I);

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

    static byte[] cls(ClassDesc name, Consumer<ClassBuilder> body) {
        return cls(name, PUB_SUPER, OBJ, body);
    }

    /** `<init>()V` calling `Object.<init>()V`, then `rest`. */
    static void ctor(ClassBuilder cb, Consumer<CodeBuilder> rest) {
        cb.withMethodBody("<init>", mt(V), PUB, code -> {
            code.aload(0).invokespecial(OBJ, "<init>", mt(V));
            rest.accept(code);
            code.return_();
        });
    }

    /** `if (inOff + blockSize > in.length) throw new DataLengthException("input buffer too short")`. */
    static void cbcLengthCheck(CodeBuilder c) {
        Label ok = c.newLabel();
        c.iload(2).aload(0).getfield(CBC, "blockSize", I).iadd().aload(1).arraylength().if_icmple(ok)
            .new_(DLE).dup().ldc("input buffer too short")
            .invokespecial(DLE, "<init>", mt(V, CD_STRING)).athrow()
            .labelBinding(ok);
    }

    static Map<String, byte[]> shapes() {
        Map<String, byte[]> out = new LinkedHashMap<>();

        // Shim witness for the BouncyCastle family.
        out.put("org.bouncycastle.math.ec.ECPoint", cls(EC_POINT, PUB_ABSTRACT, OBJ, cb -> ctor(cb, c -> {})));

        // ---- WhirlpoolDigest: update(byte) appends; update(byte[], int, int) is BC's loop.
        out.put("org.bouncycastle.crypto.digests.WhirlpoolDigest", cls(WHIRLPOOL, cb -> {
            cb.withField("_buffer", CD_BYTES, PUB);
            cb.withField("_bufferPos", I, PUB);
            ctor(cb, c -> c.aload(0).bipush(64).newarray(TypeKind.BYTE).putfield(WHIRLPOOL, "_buffer", CD_BYTES));
            cb.withMethodBody("update", mt(V, B), PUB, c -> c
                .aload(0).getfield(WHIRLPOOL, "_buffer", CD_BYTES)
                .aload(0).getfield(WHIRLPOOL, "_bufferPos", I).iload(1).bastore()
                .aload(0).dup().getfield(WHIRLPOOL, "_bufferPos", I).iconst_1().iadd()
                .putfield(WHIRLPOOL, "_bufferPos", I).return_());
            // while (len > 0) { update(in[inOff]); ++inOff; --len; }
            cb.withMethodBody("update", mt(V, CD_BYTES, I, I), PUB, c -> {
                Label loop = c.newLabel();
                Label done = c.newLabel();
                c.labelBinding(loop)
                    .iload(3).ifle(done)
                    .aload(0).aload(1).iload(2).baload().invokevirtual(WHIRLPOOL, "update", mt(V, B))
                    .iinc(2, 1).iinc(3, -1).goto_(loop)
                    .labelBinding(done)
                    .return_();
            });
        }));

        // ---- DigestRandomGenerator: generateState() bumps the counter and refills a 3-byte
        // state; nextBytes(byte[], int, int) is BC's loop (synchronized method here).
        out.put("org.bouncycastle.crypto.prng.DigestRandomGenerator", cls(DRG, cb -> {
            cb.withField("state", CD_BYTES, PUB);
            cb.withField("stateCounter", J, PUB);
            ctor(cb, c -> c.aload(0).iconst_3().newarray(TypeKind.BYTE).putfield(DRG, "state", CD_BYTES)
                .aload(0).lconst_1().putfield(DRG, "stateCounter", J));
            cb.withMethodBody("generateState", mt(V), PUB, c -> {
                Label loop = c.newLabel();
                Label done = c.newLabel();
                c.aload(0).dup().getfield(DRG, "stateCounter", J).lconst_1().ladd().putfield(DRG, "stateCounter", J)
                    .iconst_0().istore(1)
                    .labelBinding(loop)
                    .iload(1).aload(0).getfield(DRG, "state", CD_BYTES).arraylength().if_icmpge(done)
                    .aload(0).getfield(DRG, "state", CD_BYTES).iload(1)
                    .aload(0).getfield(DRG, "stateCounter", J).l2i().iload(1).iadd().i2b().bastore()
                    .iinc(1, 1).goto_(loop)
                    .labelBinding(done)
                    .return_();
            });
            cb.withMethodBody("nextBytes", mt(V, CD_BYTES), PUB, c -> c
                .aload(0).aload(1).iconst_0().aload(1).arraylength()
                .invokevirtual(DRG, "nextBytes", mt(V, CD_BYTES, I, I)).return_());
            // stateOff = 0; generateState(); end = start + len;
            // for (i = start; i != end; i++) { if (stateOff == state.length) { generateState();
            // stateOff = 0; } bytes[i] = state[stateOff++]; }
            cb.withMethodBody("nextBytes", mt(V, CD_BYTES, I, I), PUB | ClassFile.ACC_SYNCHRONIZED, c -> {
                Label loop = c.newLabel();
                Label store = c.newLabel();
                Label done = c.newLabel();
                c.iconst_0().istore(4)
                    .aload(0).invokevirtual(DRG, "generateState", mt(V))
                    .iload(2).iload(3).iadd().istore(5)
                    .iload(2).istore(6)
                    .labelBinding(loop)
                    .iload(6).iload(5).if_icmpeq(done)
                    .iload(4).aload(0).getfield(DRG, "state", CD_BYTES).arraylength().if_icmpne(store)
                    .aload(0).invokevirtual(DRG, "generateState", mt(V))
                    .iconst_0().istore(4)
                    .labelBinding(store)
                    .aload(1).iload(6)
                    .aload(0).getfield(DRG, "state", CD_BYTES).iload(4).iinc(4, 1).baload()
                    .bastore()
                    .iinc(6, 1).goto_(loop)
                    .labelBinding(done)
                    .return_();
            });
        }));

        // ---- CBCBlockCipher over a BlockCipher, with BC's encryptBlock / decryptBlock.
        out.put("org.bouncycastle.crypto.BlockCipher", cls(BLOCK_CIPHER, PUB_INTERFACE, OBJ, cb ->
            cb.withMethod("processBlock", PROCESS_BLOCK, PUB | ClassFile.ACC_ABSTRACT, mb -> {})));
        out.put("org.bouncycastle.crypto.DataLengthException", cls(DLE, PUB_SUPER, CD_RTE, cb ->
            cb.withMethodBody("<init>", mt(V, CD_STRING), PUB, c -> c.aload(0).aload(1)
                .invokespecial(CD_RTE, "<init>", mt(V, CD_STRING)).return_())));
        // out[outOff + i] = (byte) (in[inOff + i] ^ 0x5a) for i < 4; return 4
        out.put("L7W106XorEngine", cls(XOR_ENGINE, cb -> {
            cb.withInterfaceSymbols(BLOCK_CIPHER);
            ctor(cb, c -> {});
            cb.withMethodBody("processBlock", PROCESS_BLOCK, PUB, c -> {
                Label loop = c.newLabel();
                Label done = c.newLabel();
                c.iconst_0().istore(5)
                    .labelBinding(loop)
                    .iload(5).iconst_4().if_icmpge(done)
                    .aload(3).iload(4).iload(5).iadd()
                    .aload(1).iload(2).iload(5).iadd().baload().bipush(0x5a).ixor().i2b()
                    .bastore()
                    .iinc(5, 1).goto_(loop)
                    .labelBinding(done)
                    .iconst_4().ireturn();
            });
        }));
        out.put("org.bouncycastle.crypto.modes.CBCBlockCipher", cls(CBC, cb -> {
            cb.withField("cbcV", CD_BYTES, PUB);
            cb.withField("cbcNextV", CD_BYTES, PUB);
            cb.withField("blockSize", I, PUB);
            cb.withField("cipher", BLOCK_CIPHER, PUB);
            cb.withField("encrypting", Z, PUB);
            // this.cipher = cipher; blockSize = 4; cbcV = {1, 2, 3, 4}; cbcNextV = new byte[4];
            // this.encrypting = encrypting
            cb.withMethodBody("<init>", mt(V, BLOCK_CIPHER, Z), PUB, c -> {
                c.aload(0).invokespecial(OBJ, "<init>", mt(V))
                    .aload(0).aload(1).putfield(CBC, "cipher", BLOCK_CIPHER)
                    .aload(0).iconst_4().putfield(CBC, "blockSize", I)
                    .aload(0).iconst_4().newarray(TypeKind.BYTE);
                for (int k = 0; k < 4; k++) {
                    c.dup().loadConstant(k).loadConstant(k + 1).bastore();
                }
                c.putfield(CBC, "cbcV", CD_BYTES)
                    .aload(0).iconst_4().newarray(TypeKind.BYTE).putfield(CBC, "cbcNextV", CD_BYTES)
                    .aload(0).iload(2).putfield(CBC, "encrypting", Z)
                    .return_();
            });
            cb.withMethodBody("processBlock", PROCESS_BLOCK, PUB, c -> {
                Label dec = c.newLabel();
                c.aload(0).getfield(CBC, "encrypting", Z).ifeq(dec)
                    .aload(0).aload(1).iload(2).aload(3).iload(4)
                    .invokevirtual(CBC, "encryptBlock", PROCESS_BLOCK).ireturn()
                    .labelBinding(dec)
                    .aload(0).aload(1).iload(2).aload(3).iload(4)
                    .invokevirtual(CBC, "decryptBlock", PROCESS_BLOCK).ireturn();
            });
            // length check; for (i < blockSize) cbcV[i] ^= in[inOff + i];
            // length = cipher.processBlock(cbcV, 0, out, outOff);
            // System.arraycopy(out, outOff, cbcV, 0, cbcV.length); return length
            cb.withMethodBody("encryptBlock", PROCESS_BLOCK, PUB, c -> {
                Label loop = c.newLabel();
                Label done = c.newLabel();
                cbcLengthCheck(c);
                c.iconst_0().istore(5)
                    .labelBinding(loop)
                    .iload(5).aload(0).getfield(CBC, "blockSize", I).if_icmpge(done)
                    .aload(0).getfield(CBC, "cbcV", CD_BYTES).iload(5).dup2().baload()
                    .aload(1).iload(2).iload(5).iadd().baload().ixor().i2b().bastore()
                    .iinc(5, 1).goto_(loop)
                    .labelBinding(done)
                    .aload(0).getfield(CBC, "cipher", BLOCK_CIPHER)
                    .aload(0).getfield(CBC, "cbcV", CD_BYTES).iconst_0().aload(3).iload(4)
                    .invokeinterface(BLOCK_CIPHER, "processBlock", PROCESS_BLOCK).istore(6)
                    .aload(3).iload(4).aload(0).getfield(CBC, "cbcV", CD_BYTES).iconst_0()
                    .aload(0).getfield(CBC, "cbcV", CD_BYTES).arraylength()
                    .invokestatic(CD_SYSTEM, "arraycopy", ARRAYCOPY)
                    .iload(6).ireturn();
            });
            // length check; System.arraycopy(in, inOff, cbcNextV, 0, blockSize);
            // length = cipher.processBlock(in, inOff, out, outOff);
            // for (i < blockSize) out[outOff + i] ^= cbcV[i];
            // tmp = cbcV; cbcV = cbcNextV; cbcNextV = tmp; return length
            cb.withMethodBody("decryptBlock", PROCESS_BLOCK, PUB, c -> {
                Label loop = c.newLabel();
                Label done = c.newLabel();
                cbcLengthCheck(c);
                c.aload(1).iload(2).aload(0).getfield(CBC, "cbcNextV", CD_BYTES).iconst_0()
                    .aload(0).getfield(CBC, "blockSize", I)
                    .invokestatic(CD_SYSTEM, "arraycopy", ARRAYCOPY)
                    .aload(0).getfield(CBC, "cipher", BLOCK_CIPHER)
                    .aload(1).iload(2).aload(3).iload(4)
                    .invokeinterface(BLOCK_CIPHER, "processBlock", PROCESS_BLOCK).istore(5)
                    .iconst_0().istore(6)
                    .labelBinding(loop)
                    .iload(6).aload(0).getfield(CBC, "blockSize", I).if_icmpge(done)
                    .aload(3).iload(4).iload(6).iadd().dup2().baload()
                    .aload(0).getfield(CBC, "cbcV", CD_BYTES).iload(6).baload().ixor().i2b().bastore()
                    .iinc(6, 1).goto_(loop)
                    .labelBinding(done)
                    .aload(0).getfield(CBC, "cbcV", CD_BYTES).astore(7)
                    .aload(0).aload(0).getfield(CBC, "cbcNextV", CD_BYTES).putfield(CBC, "cbcV", CD_BYTES)
                    .aload(0).aload(7).putfield(CBC, "cbcNextV", CD_BYTES)
                    .iload(5).ireturn();
            });
        }));

        // ---- The driver: ordinary invokes of the shapes (interpreted, then compiled).
        out.put("L7W106ShadowShapesDriver", cls(DRIVER, cb -> {
            cb.withInterfaceSymbols(API);
            ctor(cb, c -> {});
            cb.withMethodBody("newWhirlpool", mt(OBJ), PUB, c -> c.new_(WHIRLPOOL).dup()
                .invokespecial(WHIRLPOOL, "<init>", mt(V)).areturn());
            cb.withMethodBody("wpUpdate", mt(V, OBJ, CD_BYTES, I, I), PUB, c -> c.aload(1).checkcast(WHIRLPOOL)
                .aload(2).iload(3).iload(4).invokevirtual(WHIRLPOOL, "update", mt(V, CD_BYTES, I, I)).return_());
            cb.withMethodBody("newDrg", mt(OBJ), PUB, c -> c.new_(DRG).dup()
                .invokespecial(DRG, "<init>", mt(V)).areturn());
            cb.withMethodBody("drgNext", mt(V, OBJ, CD_BYTES), PUB, c -> c.aload(1).checkcast(DRG)
                .aload(2).invokevirtual(DRG, "nextBytes", mt(V, CD_BYTES)).return_());
            cb.withMethodBody("drgNext3", mt(V, OBJ, CD_BYTES, I, I), PUB, c -> c.aload(1).checkcast(DRG)
                .aload(2).iload(3).iload(4).invokevirtual(DRG, "nextBytes", mt(V, CD_BYTES, I, I)).return_());
            cb.withMethodBody("newXorEngine", mt(OBJ), PUB, c -> c.new_(XOR_ENGINE).dup()
                .invokespecial(XOR_ENGINE, "<init>", mt(V)).areturn());
            cb.withMethodBody("newCbc", mt(OBJ, OBJ, Z), PUB, c -> c.new_(CBC).dup()
                .aload(1).checkcast(BLOCK_CIPHER).iload(2)
                .invokespecial(CBC, "<init>", mt(V, BLOCK_CIPHER, Z)).areturn());
            cb.withMethodBody("cbcProcess", mt(I, OBJ, CD_BYTES, I, CD_BYTES, I), PUB, c -> c.aload(1).checkcast(CBC)
                .aload(2).iload(3).aload(4).iload(5).invokevirtual(CBC, "processBlock", PROCESS_BLOCK).ireturn());
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

    interface State {
        String get() throws Throwable;
    }

    static String answer(Row r) {
        try {
            return String.valueOf(r.run());
        } catch (Throwable t) {
            return t.toString();
        }
    }

    static void row(String name, Row r, State s) {
        String a = answer(r);
        String st;
        try {
            st = s.get();
        } catch (Throwable t) {
            st = "state " + t;
        }
        System.out.println(name + " -> " + a + " | " + st);
    }

    static String hex(byte[] b, int n) {
        if (b == null) {
            return "-";
        }
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < Math.min(n, b.length); i++) {
            sb.append(String.format("%02x", b[i] & 0xff));
        }
        return sb.toString();
    }

    static Object field(Object o, String name) throws Exception {
        return o.getClass().getField(name).get(o);
    }

    static ClassLoader loader;
    static Api api;

    static void whirlpool(String name, byte[] in, int off, int len) {
        Object d = api.newWhirlpool();
        row(name, () -> {
            api.wpUpdate(d, in, off, len);
            return "ok";
        }, () -> "pos=" + field(d, "_bufferPos") + " buf=" + hex((byte[]) field(d, "_buffer"), 2));
    }

    static void drg(String name, byte[] bytes, int start, int len, boolean oneArg) {
        Object g = api.newDrg();
        row(name, () -> {
            if (oneArg) {
                api.drgNext(g, bytes);
            } else {
                api.drgNext3(g, bytes, start, len);
            }
            return "ok";
        }, () -> "counter=" + field(g, "stateCounter") + " bytes=" + hex(bytes, 8));
    }

    static void cbc(String name, boolean encrypting, boolean withCipher, byte[] in, int inOff, byte[] out,
                    int outOff) {
        Object c = api.newCbc(withCipher ? api.newXorEngine() : null, encrypting);
        row(name, () -> api.cbcProcess(c, in, inOff, out, outOff),
            () -> "out=" + hex(out, 8) + " v=" + hex((byte[]) field(c, "cbcV"), 8));
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
        loader = L7W106ShadowShapes.class.getClassLoader();
        if (loader.getResource("org/bouncycastle/math/ec/ECPoint.class") == null) {
            System.err.println("L7W106ShadowShapes: shapes not on the class path (run with --emit <dir> and add"
                + " <dir> to -cp); defining them in a loader of the probe's own, no shim family selected");
            loader = new MapLoader(shapes(), loader);
        }
        api = (Api) Class.forName("L7W106ShadowShapesDriver", true, loader).getDeclaredConstructor().newInstance();

        // WhirlpoolDigest.update([BII)V
        whirlpool("wp-negative-off", new byte[4], -1, 2);
        whirlpool("wp-past-end", new byte[] {1, 2, 3, 4}, 2, 5);
        whirlpool("wp-null-input", null, 0, 1);
        whirlpool("wp-null-zero-len", null, 0, 0);
        whirlpool("wp-negative-len", new byte[4], 0, -3);

        // DigestRandomGenerator.nextBytes([BII)V and ([B)V
        drg("drg-negative-start", new byte[4], -1, 2, false);
        drg("drg-negative-len", new byte[4], 0, -1, false);
        drg("drg-past-end", new byte[4], 2, 5, false);
        drg("drg-null-zero-len", null, 0, 0, false);
        drg("drg-null-len1", null, 0, 1, false);
        drg("drg-null-one-arg", null, 0, 0, true);

        // CBCBlockCipher.processBlock([BI[BI)I
        byte[] plain = {0x10, 0x20, 0x30, 0x40};
        cbc("cbc-encrypt-ok", true, true, plain.clone(), 0, new byte[4], 0);
        cbc("cbc-decrypt-ok", false, true, plain.clone(), 0, new byte[4], 0);
        cbc("cbc-null-in", true, true, null, 0, new byte[4], 0);
        cbc("cbc-null-out", true, true, plain.clone(), 0, null, 0);
        cbc("cbc-negative-in", true, true, new byte[8], -1, new byte[4], 0);
        cbc("cbc-negative-out", true, true, plain.clone(), 0, new byte[4], -1);
        cbc("cbc-null-cipher", true, false, plain.clone(), 0, new byte[4], 0);
        cbc("cbc-short-in", true, true, new byte[2], 0, new byte[4], 0);
        cbc("cbc-decrypt-negative-in", false, true, new byte[8], -1, new byte[4], 0);
    }
}
