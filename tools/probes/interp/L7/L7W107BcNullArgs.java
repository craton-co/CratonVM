// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 107, lane L7
// (i106-L7-bc-plain-shadows-answer-null-arrays-with-their-own-npe): a null object argument to a
// PLAIN BouncyCastle shadow (`bc_register_null_rerun!` in
// native-builtins/src/phases_late/bouncycastle.rs) must run BC's bytecode, never the shadow body,
// whose `obj_arg` answered "java.lang.NullPointerException: null object argument" (no HotSpot run
// prints that). Self-contained SHAPES generated with java.lang.classfile (classes with BC's
// names and descriptors, small bodies), so no bcprov jar is needed. A representative spread:
// block ciphers' processBlock (SM4, XTEA, AES) and AESEngine.init, a stream cipher's
// processBytes (Salsa20), the ChaCha core, the SPHINCS hash layer, Arrays.copyOf, the Strings
// UTF-8 pair, the SCrypt and BCrypt generators, an EC field element, plus two rows each through
// the `bc_register_rerun!` families (Poly1305 update/doFinal, Pack.bigEndianToInt) so both
// wrappers are pinned. Every row passes a null; the zero-length rows show the bytecode answering
// with no exception at all.
//
// Setup (the BC shim family is selected at boot from the class-path witness
// `org/bouncycastle/math/ec/ECPoint.class`, which --emit writes too):
//   javac -d out L7W107BcNullArgs.java
//   java -cp out L7W107BcNullArgs --emit shapes          (any JVM: writes the shapes)
//   java -cp out:shapes L7W107BcNullArgs                 (HotSpot reference)
//   cratonvm [--compatible] [--nojit] -cp out:shapes L7W107BcNullArgs
// Evidence that the shadows ran and handed back (not just the bytes):
//   CRATONVM_DBG_SHADOW_RERUN=1 cratonvm --nojit -cp out:shapes L7W107BcNullArgs 2>err.txt
//   grep -c '^\[shadow-rerun\] org/bouncycastle/' err.txt     (expect 18: one per row)
//   grep -c 'after a null argument' err.txt                     (expect 14: the plain rows; the
//   four Poly1305 / Pack rows hand back through the own-refusal path, "after NullPointerException")
// Before wave 107 every plain row printed "java.lang.NullPointerException: null object argument"
// (the zero-length Salsa20 row too).
// Without `shapes` on the class path the probe defines the same classes in a loader of its own
// and prints the same lines (stderr says so), but no shim family is selected: not evidence.
//
// The NPE texts are JEP 358's for the shapes' own bytes (no LocalVariableTable: parameters read
// as "<parameterN>"); the HotSpot run on the emitted shapes is the reference.
// HOTSPOT_EXPECTED_BEGIN (JDK 25; NPE texts predicted from JEP 358 for these bytes)
// sm4-null-in -> java.lang.NullPointerException: Cannot load from byte/boolean array because "<parameter1>" is null
// xtea-null-out -> java.lang.NullPointerException: Cannot load from byte/boolean array because "<parameter3>" is null
// aes-null-in -> java.lang.NullPointerException: Cannot load from byte/boolean array because "<parameter1>" is null
// aes-init-null-params -> java.lang.NullPointerException: Cannot invoke "Object.toString()" because "<parameter2>" is null
// salsa-null-in -> java.lang.NullPointerException: Cannot load from byte/boolean array because "<parameter1>" is null
// salsa-null-in-zero-len -> 0
// chacha-core-null-input -> java.lang.NullPointerException: Cannot load from int array because "<parameter2>" is null
// sphincs-hash-null-out -> java.lang.NullPointerException: Cannot store to byte/boolean array because "<parameter1>" is null
// arrays-copyof-null -> java.lang.NullPointerException: Cannot read the array length because "<parameter1>" is null
// strings-to-utf8-null -> java.lang.NullPointerException: Cannot invoke "String.toCharArray()" because "<parameter1>" is null
// strings-from-utf8-null -> java.lang.NullPointerException: Cannot read the array length because "<parameter1>" is null
// scrypt-null-pass -> java.lang.IllegalArgumentException: Passphrase P must be provided.
// bcrypt-null-salt -> java.lang.IllegalArgumentException: Salt required.
// fp-add-null -> java.lang.NullPointerException: Cannot invoke "org.bouncycastle.math.ec.ECFieldElement.toBigInteger()" because "<parameter1>" is null
// poly-update-null -> java.lang.NullPointerException: Cannot load from byte/boolean array because "<parameter1>" is null
// poly-update-null-zero-len -> ok
// poly-dofinal-null -> java.lang.NullPointerException: Cannot read the array length because "<parameter1>" is null
// pack-be-null -> java.lang.NullPointerException: Cannot load from byte/boolean array because "<parameter1>" is null
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

public class L7W107BcNullArgs {

    /** What the generated driver does; every call site in it is an ordinary invoke of a shape. */
    public interface Api {
        Object newSm4();

        int sm4(Object e, byte[] in, int inOff, byte[] out, int outOff);

        Object newXtea();

        int xtea(Object e, byte[] in, int inOff, byte[] out, int outOff);

        Object newAes();

        int aes(Object e, byte[] in, int inOff, byte[] out, int outOff);

        void aesInit(Object e, boolean forEncryption, Object params);

        Object newSalsa();

        int salsa(Object e, byte[] in, int inOff, int len, byte[] out, int outOff);

        void chachaCore(int rounds, int[] input, int[] x);

        Object newHashFunctions();

        int hashNN(Object hf, byte[] out, int outOff, byte[] in, int inOff);

        byte[] arraysCopyOf(byte[] data, int newLength);

        byte[] toUtf8(String s);

        String fromUtf8(byte[] b);

        byte[] scrypt(byte[] p, byte[] s, int n, int r, int par, int len);

        byte[] bcrypt(byte[] password, byte[] salt, int cost);

        Object newFp();

        Object fpAdd(Object a, Object b);

        Object newPoly();

        void polyUpdate(Object p, byte[] in, int inOff, int len);

        int polyDoFinal(Object p, byte[] out, int outOff);

        int packBe(byte[] bs, int off);
    }

    static final int PUB = ClassFile.ACC_PUBLIC;
    static final int PUB_STATIC = ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC;
    static final int PUB_SUPER = ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER;
    static final int PUB_ABSTRACT = PUB_SUPER | ClassFile.ACC_ABSTRACT;
    static final int PUB_INTERFACE = ClassFile.ACC_PUBLIC | ClassFile.ACC_INTERFACE | ClassFile.ACC_ABSTRACT;

    static final ClassDesc EC_POINT = ClassDesc.of("org.bouncycastle.math.ec.ECPoint");
    static final ClassDesc EC_FIELD_ELEMENT = ClassDesc.of("org.bouncycastle.math.ec.ECFieldElement");
    static final ClassDesc EC_FP = ClassDesc.of("org.bouncycastle.math.ec.ECFieldElement$Fp");
    static final ClassDesc CIPHER_PARAMETERS = ClassDesc.of("org.bouncycastle.crypto.CipherParameters");
    static final ClassDesc SM4 = ClassDesc.of("org.bouncycastle.crypto.engines.SM4Engine");
    static final ClassDesc XTEA = ClassDesc.of("org.bouncycastle.crypto.engines.XTEAEngine");
    static final ClassDesc AES = ClassDesc.of("org.bouncycastle.crypto.engines.AESEngine");
    static final ClassDesc SALSA = ClassDesc.of("org.bouncycastle.crypto.engines.Salsa20Engine");
    static final ClassDesc CHACHA = ClassDesc.of("org.bouncycastle.crypto.engines.ChaChaEngine");
    static final ClassDesc HASH_FUNCTIONS = ClassDesc.of("org.bouncycastle.pqc.crypto.sphincs.HashFunctions");
    static final ClassDesc BC_ARRAYS = ClassDesc.of("org.bouncycastle.util.Arrays");
    static final ClassDesc BC_STRINGS = ClassDesc.of("org.bouncycastle.util.Strings");
    static final ClassDesc SCRYPT = ClassDesc.of("org.bouncycastle.crypto.generators.SCrypt");
    static final ClassDesc BCRYPT = ClassDesc.of("org.bouncycastle.crypto.generators.BCrypt");
    static final ClassDesc POLY1305 = ClassDesc.of("org.bouncycastle.crypto.macs.Poly1305");
    static final ClassDesc PACK = ClassDesc.of("org.bouncycastle.util.Pack");

    static final ClassDesc DRIVER = ClassDesc.of("L7W107BcNullArgsDriver");
    static final ClassDesc API = ClassDesc.of("L7W107BcNullArgs$Api");

    static final ClassDesc CD_BYTES = ConstantDescs.CD_byte.arrayType();
    static final ClassDesc CD_INTS = ConstantDescs.CD_int.arrayType();
    static final ClassDesc CD_CHARS = ConstantDescs.CD_char.arrayType();
    static final ClassDesc CD_SYSTEM = ClassDesc.of("java.lang.System");
    static final ClassDesc CD_MATH = ClassDesc.of("java.lang.Math");
    static final ClassDesc CD_RTE = ClassDesc.of("java.lang.RuntimeException");
    static final ClassDesc CD_IAE = ClassDesc.of("java.lang.IllegalArgumentException");
    static final ClassDesc CD_BIG_INTEGER = ClassDesc.of("java.math.BigInteger");
    static final ClassDesc CD_STRING = ConstantDescs.CD_String;

    static final ClassDesc I = ConstantDescs.CD_int;
    static final ClassDesc Z = ConstantDescs.CD_boolean;
    static final ClassDesc V = ConstantDescs.CD_void;
    static final ClassDesc OBJ = ConstantDescs.CD_Object;

    static final MethodTypeDesc PROCESS_BLOCK = MethodTypeDesc.of(I, CD_BYTES, I, CD_BYTES, I);
    static final MethodTypeDesc PROCESS_BYTES = MethodTypeDesc.of(I, CD_BYTES, I, I, CD_BYTES, I);
    static final MethodTypeDesc INIT = MethodTypeDesc.of(V, Z, CIPHER_PARAMETERS);
    static final MethodTypeDesc CORE = MethodTypeDesc.of(V, I, CD_INTS, CD_INTS);
    static final MethodTypeDesc HASH_N_N = MethodTypeDesc.of(I, CD_BYTES, I, CD_BYTES, I);
    static final MethodTypeDesc COPY_OF = MethodTypeDesc.of(CD_BYTES, CD_BYTES, I);
    static final MethodTypeDesc TO_UTF8 = MethodTypeDesc.of(CD_BYTES, CD_STRING);
    static final MethodTypeDesc FROM_UTF8 = MethodTypeDesc.of(CD_STRING, CD_BYTES);
    static final MethodTypeDesc SCRYPT_GENERATE = MethodTypeDesc.of(CD_BYTES, CD_BYTES, CD_BYTES, I, I, I, I);
    static final MethodTypeDesc BCRYPT_GENERATE = MethodTypeDesc.of(CD_BYTES, CD_BYTES, CD_BYTES, I);
    static final MethodTypeDesc FE_ADD = MethodTypeDesc.of(EC_FIELD_ELEMENT, EC_FIELD_ELEMENT);
    static final MethodTypeDesc POLY_UPDATE = MethodTypeDesc.of(V, CD_BYTES, I, I);
    static final MethodTypeDesc POLY_DO_FINAL = MethodTypeDesc.of(I, CD_BYTES, I);
    static final MethodTypeDesc PACK_BE = MethodTypeDesc.of(I, CD_BYTES, I);
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

    /** `<init>()V` calling `superClass.<init>()V`. */
    static void ctor(ClassBuilder cb, ClassDesc superClass) {
        cb.withMethodBody("<init>", mt(V), PUB, code -> code.aload(0)
            .invokespecial(superClass, "<init>", mt(V)).return_());
    }

    static void ctor(ClassBuilder cb) {
        ctor(cb, OBJ);
    }

    /** `return in[inOff] + out[outOff];` */
    static void blockBody(CodeBuilder c) {
        c.aload(1).iload(2).baload().aload(3).iload(4).baload().iadd().ireturn();
    }

    /** `throw new IllegalArgumentException(text)`. */
    static void throwIae(CodeBuilder c, String text) {
        c.new_(CD_IAE).dup().ldc(text).invokespecial(CD_IAE, "<init>", mt(V, CD_STRING)).athrow();
    }

    static byte[] blockEngine(ClassDesc name) {
        return cls(name, cb -> {
            ctor(cb);
            cb.withMethodBody("processBlock", PROCESS_BLOCK, PUB, L7W107BcNullArgs::blockBody);
        });
    }

    static Map<String, byte[]> shapes() {
        Map<String, byte[]> out = new LinkedHashMap<>();

        // Shim witness for the BouncyCastle family.
        out.put("org.bouncycastle.math.ec.ECPoint", cls(EC_POINT, PUB_ABSTRACT, OBJ, cb -> ctor(cb)));
        out.put("org.bouncycastle.crypto.CipherParameters",
            cls(CIPHER_PARAMETERS, PUB_INTERFACE, OBJ, cb -> {}));

        // ---- Block ciphers: processBlock([BI[BI)I.
        out.put("org.bouncycastle.crypto.engines.SM4Engine", blockEngine(SM4));
        out.put("org.bouncycastle.crypto.engines.XTEAEngine", blockEngine(XTEA));
        // AESEngine: processBlock, and init(boolean, CipherParameters) calling params.toString().
        out.put("org.bouncycastle.crypto.engines.AESEngine", cls(AES, cb -> {
            ctor(cb);
            cb.withMethodBody("processBlock", PROCESS_BLOCK, PUB, L7W107BcNullArgs::blockBody);
            cb.withMethodBody("init", INIT, PUB, c -> c.aload(2)
                .invokevirtual(OBJ, "toString", mt(CD_STRING)).pop().return_());
        }));

        // ---- Salsa20Engine.processBytes: for (i < len) out[outOff + i] = in[inOff + i]; return len.
        out.put("org.bouncycastle.crypto.engines.Salsa20Engine", cls(SALSA, cb -> {
            ctor(cb);
            cb.withMethodBody("processBytes", PROCESS_BYTES, PUB, c -> {
                Label loop = c.newLabel();
                Label done = c.newLabel();
                c.iconst_0().istore(6)
                    .labelBinding(loop)
                    .iload(6).iload(3).if_icmpge(done)
                    .aload(4).iload(5).iload(6).iadd()
                    .aload(1).iload(2).iload(6).iadd().baload()
                    .bastore()
                    .iinc(6, 1).goto_(loop)
                    .labelBinding(done)
                    .iload(3).ireturn();
            });
        }));

        // ---- ChaChaEngine.chachaCore (static): x[0] = input[0].
        out.put("org.bouncycastle.crypto.engines.ChaChaEngine", cls(CHACHA, cb -> {
            ctor(cb);
            cb.withMethodBody("chachaCore", CORE, PUB_STATIC, c -> c
                .aload(2).iconst_0().aload(1).iconst_0().iaload().iastore().return_());
        }));

        // ---- SPHINCS HashFunctions.hash_n_n: out[outOff] = in[inOff]; return 0.
        out.put("org.bouncycastle.pqc.crypto.sphincs.HashFunctions", cls(HASH_FUNCTIONS, cb -> {
            ctor(cb);
            cb.withMethodBody("hash_n_n", HASH_N_N, PUB, c -> c
                .aload(1).iload(2).aload(3).iload(4).baload().bastore().iconst_0().ireturn());
        }));

        // ---- Arrays.copyOf (static): BC's body.
        out.put("org.bouncycastle.util.Arrays", cls(BC_ARRAYS, cb -> {
            ctor(cb);
            cb.withMethodBody("copyOf", COPY_OF, PUB_STATIC, c -> c
                .iload(1).newarray(TypeKind.BYTE).astore(2)
                .aload(0).iconst_0().aload(2).iconst_0()
                .iload(1).aload(0).arraylength().invokestatic(CD_MATH, "min", mt(I, I, I))
                .invokestatic(CD_SYSTEM, "arraycopy", ARRAYCOPY)
                .aload(2).areturn());
        }));

        // ---- Strings (static): toUTF8ByteArray(String) reads string.toCharArray();
        // fromUTF8ByteArray(byte[]) reads bytes.length.
        out.put("org.bouncycastle.util.Strings", cls(BC_STRINGS, cb -> {
            ctor(cb);
            cb.withMethodBody("toUTF8ByteArray", TO_UTF8, PUB_STATIC, c -> c
                .aload(0).invokevirtual(CD_STRING, "toCharArray", mt(CD_CHARS)).arraylength()
                .newarray(TypeKind.BYTE).areturn());
            cb.withMethodBody("fromUTF8ByteArray", FROM_UTF8, PUB_STATIC, c -> c
                .aload(0).arraylength().invokestatic(CD_STRING, "valueOf", mt(CD_STRING, I)).areturn());
        }));

        // ---- Generators (static): the library's own argument checks.
        out.put("org.bouncycastle.crypto.generators.SCrypt", cls(SCRYPT, cb -> {
            ctor(cb);
            cb.withMethodBody("generate", SCRYPT_GENERATE, PUB_STATIC, c -> {
                Label ok = c.newLabel();
                c.aload(0).ifnonnull(ok);
                throwIae(c, "Passphrase P must be provided.");
                c.labelBinding(ok).iload(5).newarray(TypeKind.BYTE).areturn();
            });
        }));
        out.put("org.bouncycastle.crypto.generators.BCrypt", cls(BCRYPT, cb -> {
            ctor(cb);
            cb.withMethodBody("generate", BCRYPT_GENERATE, PUB_STATIC, c -> {
                Label ok = c.newLabel();
                c.aload(1).ifnonnull(ok);
                throwIae(c, "Salt required.");
                c.labelBinding(ok).bipush(24).newarray(TypeKind.BYTE).areturn();
            });
        }));

        // ---- ECFieldElement$Fp.add(ECFieldElement): b.toBigInteger(); return this.
        out.put("org.bouncycastle.math.ec.ECFieldElement", cls(EC_FIELD_ELEMENT, PUB_ABSTRACT, OBJ, cb -> {
            ctor(cb);
            cb.withMethod("toBigInteger", mt(CD_BIG_INTEGER), PUB | ClassFile.ACC_ABSTRACT, mb -> {});
        }));
        out.put("org.bouncycastle.math.ec.ECFieldElement$Fp", cls(EC_FP, PUB_SUPER, EC_FIELD_ELEMENT, cb -> {
            ctor(cb, EC_FIELD_ELEMENT);
            cb.withMethodBody("toBigInteger", mt(CD_BIG_INTEGER), PUB, c -> c.aconst_null().areturn());
            cb.withMethodBody("add", FE_ADD, PUB, c -> c
                .aload(1).invokevirtual(EC_FIELD_ELEMENT, "toBigInteger", mt(CD_BIG_INTEGER)).pop()
                .aload(0).areturn());
        }));

        // ---- Poly1305 (a `bc_register_rerun!` family): update reads in[inOff + i] for i < len;
        // doFinal checks outOff + 16 > out.length.
        out.put("org.bouncycastle.crypto.macs.Poly1305", cls(POLY1305, cb -> {
            ctor(cb);
            cb.withMethodBody("update", POLY_UPDATE, PUB, c -> {
                Label loop = c.newLabel();
                Label done = c.newLabel();
                c.iconst_0().istore(4)
                    .labelBinding(loop)
                    .iload(4).iload(3).if_icmpge(done)
                    .aload(1).iload(2).iload(4).iadd().baload().pop()
                    .iinc(4, 1).goto_(loop)
                    .labelBinding(done)
                    .return_();
            });
            cb.withMethodBody("doFinal", POLY_DO_FINAL, PUB, c -> {
                Label ok = c.newLabel();
                c.iload(2).bipush(16).iadd().aload(1).arraylength().if_icmple(ok)
                    .new_(CD_RTE).dup().ldc("Output buffer is too short.")
                    .invokespecial(CD_RTE, "<init>", mt(V, CD_STRING)).athrow()
                    .labelBinding(ok)
                    .bipush(16).ireturn();
            });
        }));

        // ---- Pack.bigEndianToInt (static, a `bc_register_rerun!` family): return bs[off].
        out.put("org.bouncycastle.util.Pack", cls(PACK, cb -> {
            ctor(cb);
            cb.withMethodBody("bigEndianToInt", PACK_BE, PUB_STATIC, c -> c
                .aload(0).iload(1).baload().ireturn());
        }));

        // ---- The driver: ordinary invokes of the shapes.
        out.put("L7W107BcNullArgsDriver", cls(DRIVER, cb -> {
            cb.withInterfaceSymbols(API);
            ctor(cb);
            for (Object[] e : new Object[][] {{"newSm4", SM4}, {"newXtea", XTEA}, {"newAes", AES},
                {"newSalsa", SALSA}, {"newHashFunctions", HASH_FUNCTIONS}, {"newFp", EC_FP},
                {"newPoly", POLY1305}}) {
                ClassDesc owner = (ClassDesc) e[1];
                cb.withMethodBody((String) e[0], mt(OBJ), PUB, c -> c.new_(owner).dup()
                    .invokespecial(owner, "<init>", mt(V)).areturn());
            }
            for (Object[] e : new Object[][] {{"sm4", SM4}, {"xtea", XTEA}, {"aes", AES}}) {
                ClassDesc owner = (ClassDesc) e[1];
                cb.withMethodBody((String) e[0], mt(I, OBJ, CD_BYTES, I, CD_BYTES, I), PUB, c -> c
                    .aload(1).checkcast(owner).aload(2).iload(3).aload(4).iload(5)
                    .invokevirtual(owner, "processBlock", PROCESS_BLOCK).ireturn());
            }
            cb.withMethodBody("aesInit", mt(V, OBJ, Z, OBJ), PUB, c -> c
                .aload(1).checkcast(AES).iload(2).aload(3).checkcast(CIPHER_PARAMETERS)
                .invokevirtual(AES, "init", INIT).return_());
            cb.withMethodBody("salsa", mt(I, OBJ, CD_BYTES, I, I, CD_BYTES, I), PUB, c -> c
                .aload(1).checkcast(SALSA).aload(2).iload(3).iload(4).aload(5).iload(6)
                .invokevirtual(SALSA, "processBytes", PROCESS_BYTES).ireturn());
            cb.withMethodBody("chachaCore", mt(V, I, CD_INTS, CD_INTS), PUB, c -> c
                .iload(1).aload(2).aload(3).invokestatic(CHACHA, "chachaCore", CORE).return_());
            cb.withMethodBody("hashNN", mt(I, OBJ, CD_BYTES, I, CD_BYTES, I), PUB, c -> c
                .aload(1).checkcast(HASH_FUNCTIONS).aload(2).iload(3).aload(4).iload(5)
                .invokevirtual(HASH_FUNCTIONS, "hash_n_n", HASH_N_N).ireturn());
            cb.withMethodBody("arraysCopyOf", mt(CD_BYTES, CD_BYTES, I), PUB, c -> c
                .aload(1).iload(2).invokestatic(BC_ARRAYS, "copyOf", COPY_OF).areturn());
            cb.withMethodBody("toUtf8", mt(CD_BYTES, CD_STRING), PUB, c -> c
                .aload(1).invokestatic(BC_STRINGS, "toUTF8ByteArray", TO_UTF8).areturn());
            cb.withMethodBody("fromUtf8", mt(CD_STRING, CD_BYTES), PUB, c -> c
                .aload(1).invokestatic(BC_STRINGS, "fromUTF8ByteArray", FROM_UTF8).areturn());
            cb.withMethodBody("scrypt", mt(CD_BYTES, CD_BYTES, CD_BYTES, I, I, I, I), PUB, c -> c
                .aload(1).aload(2).iload(3).iload(4).iload(5).iload(6)
                .invokestatic(SCRYPT, "generate", SCRYPT_GENERATE).areturn());
            cb.withMethodBody("bcrypt", mt(CD_BYTES, CD_BYTES, CD_BYTES, I), PUB, c -> c
                .aload(1).aload(2).iload(3).invokestatic(BCRYPT, "generate", BCRYPT_GENERATE).areturn());
            cb.withMethodBody("fpAdd", mt(OBJ, OBJ, OBJ), PUB, c -> c
                .aload(1).checkcast(EC_FP).aload(2).checkcast(EC_FIELD_ELEMENT)
                .invokevirtual(EC_FP, "add", FE_ADD).areturn());
            cb.withMethodBody("polyUpdate", mt(V, OBJ, CD_BYTES, I, I), PUB, c -> c
                .aload(1).checkcast(POLY1305).aload(2).iload(3).iload(4)
                .invokevirtual(POLY1305, "update", POLY_UPDATE).return_());
            cb.withMethodBody("polyDoFinal", mt(I, OBJ, CD_BYTES, I), PUB, c -> c
                .aload(1).checkcast(POLY1305).aload(2).iload(3)
                .invokevirtual(POLY1305, "doFinal", POLY_DO_FINAL).ireturn());
            cb.withMethodBody("packBe", mt(I, CD_BYTES, I), PUB, c -> c
                .aload(1).iload(2).invokestatic(PACK, "bigEndianToInt", PACK_BE).ireturn());
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
            a = v instanceof byte[] b ? "byte[" + b.length + "]" : String.valueOf(v);
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
        ClassLoader loader = L7W107BcNullArgs.class.getClassLoader();
        if (loader.getResource("org/bouncycastle/math/ec/ECPoint.class") == null) {
            System.err.println("L7W107BcNullArgs: shapes not on the class path (run with --emit <dir> and add"
                + " <dir> to -cp); defining them in a loader of the probe's own, no shim family selected");
            loader = new MapLoader(shapes(), loader);
        }
        Api api = (Api) Class.forName("L7W107BcNullArgsDriver", true, loader).getDeclaredConstructor().newInstance();

        row("sm4-null-in", () -> api.sm4(api.newSm4(), null, 0, new byte[16], 0));
        row("xtea-null-out", () -> api.xtea(api.newXtea(), new byte[8], 0, null, 0));
        row("aes-null-in", () -> api.aes(api.newAes(), null, 0, new byte[16], 0));
        row("aes-init-null-params", () -> {
            api.aesInit(api.newAes(), true, null);
            return "ok";
        });
        row("salsa-null-in", () -> api.salsa(api.newSalsa(), null, 0, 1, new byte[4], 0));
        row("salsa-null-in-zero-len", () -> api.salsa(api.newSalsa(), null, 0, 0, new byte[4], 0));
        row("chacha-core-null-input", () -> {
            api.chachaCore(20, null, new int[16]);
            return "ok";
        });
        row("sphincs-hash-null-out", () -> api.hashNN(api.newHashFunctions(), null, 0, new byte[64], 0));
        row("arrays-copyof-null", () -> api.arraysCopyOf(null, 4));
        row("strings-to-utf8-null", () -> api.toUtf8(null));
        row("strings-from-utf8-null", () -> api.fromUtf8(null));
        row("scrypt-null-pass", () -> api.scrypt(null, new byte[8], 16, 1, 1, 32));
        row("bcrypt-null-salt", () -> api.bcrypt(new byte[8], null, 4));
        row("fp-add-null", () -> api.fpAdd(api.newFp(), null));
        row("poly-update-null", () -> {
            api.polyUpdate(api.newPoly(), null, 0, 1);
            return "ok";
        });
        row("poly-update-null-zero-len", () -> {
            api.polyUpdate(api.newPoly(), null, 0, 0);
            return "ok";
        });
        row("poly-dofinal-null", () -> api.polyDoFinal(api.newPoly(), null, 0));
        row("pack-be-null", () -> api.packBe(null, 0));
    }
}
