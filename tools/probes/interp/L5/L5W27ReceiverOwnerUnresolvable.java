// Interpreter round i1, wave 27, lane L5 — `invokevirtual` with a NON-null
// receiver resolves the `Methodref`'s class through the caller's loader first
// (JVMS §6.5 / §5.4.3.3 / §5.4.3.1), so an owner that loader cannot load is a
// `NoClassDefFoundError`, recorded against the class entry (§5.4.3), whatever
// the receiver is.
//
// The shape is verifier-legal: loader P defines `l5r.Opt` (v() = 7),
// `l5r.Sub extends Opt` (v() = 8), the interface `l5r.Sink { int take(Opt) }`
// and `l5r.Driver`, whose `drive(Sink)` calls `sink.take(new Sub())`. Loader C
// defines `l5r.Caller implements Sink`, delegating `l5r.Sink` to P but REFUSING
// `l5r.Opt` (counted); `Caller.take(Opt o)` is `o.v()` — `aload_1;
// invokevirtual l5r/Opt.v()I` on a receiver typed `Opt` by the descriptor, so
// the verifier needs no class. HotSpot resolves `Opt` through C on the first
// `take` and fails; the second call rethrows the recorded error without asking
// C again.
//
// Run (no setup):
//   javac -d out L5W27ReceiverOwnerUnresolvable.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W27ReceiverOwnerUnresolvable
//
// Expected HotSpot 25 output (compare verbatim):
//   take#0: java.lang.NoClassDefFoundError: l5r/Opt
//   take#1: java.lang.NoClassDefFoundError: l5r/Opt
//   Opt requests from C: 1
//
// CratonVM before wave 27 (from the code, not run): the receiver decided —
// `take#0: 8`, `take#1: 8`, `Opt requests from C: 0` (execute_invoke_kind
// resolved the owner only for a null receiver). Wave 27, lane L5
// (`invoke::resolve_owner_unknown_to_caller`): `--jdk-only` (the default)
// prints HotSpot's rows; `--compatible` still prints the old rows and counts
// the case (`CRATONVM_DBG=access` exit line, `receiver-owner-unknown=`). See
// docs/internal/fixed-bugs/interpreter-L5-invokevirtual-does-not-resolve-an-unloaded-owner-before-the-null-check-FIXED-20260930.md.

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.HashMap;
import java.util.Map;
import java.util.concurrent.atomic.AtomicInteger;

public class L5W27ReceiverOwnerUnresolvable {
    static final ClassDesc OPT = ClassDesc.of("l5r.Opt");
    static final ClassDesc SUB = ClassDesc.of("l5r.Sub");
    static final ClassDesc SINK = ClassDesc.of("l5r.Sink");
    static final ClassDesc DRIVER = ClassDesc.of("l5r.Driver");
    static final ClassDesc CALLER = ClassDesc.of("l5r.Caller");
    static final MethodTypeDesc INT = MethodTypeDesc.of(ConstantDescs.CD_int);
    static final MethodTypeDesc TAKE = MethodTypeDesc.of(ConstantDescs.CD_int, OPT);
    static final int PUBLIC = ClassFile.ACC_PUBLIC;

    static byte[] withCtor(ClassDesc self, ClassDesc sup, int v) {
        return ClassFile.of().build(self, clb -> clb
                .withFlags(PUBLIC)
                .withSuperclass(sup)
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(sup, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                                .return_())
                .withMethodBody("v", INT, PUBLIC, cb -> cb.bipush(v).ireturn()));
    }

    static byte[] sink() {
        return ClassFile.of().build(SINK, clb -> clb
                .withFlags(PUBLIC | ClassFile.ACC_INTERFACE | ClassFile.ACC_ABSTRACT)
                .withMethod("take", TAKE, PUBLIC | ClassFile.ACC_ABSTRACT, mb -> { }));
    }

    static byte[] driver() {
        return ClassFile.of().build(DRIVER, clb -> clb
                .withFlags(PUBLIC)
                .withMethodBody("drive", MethodTypeDesc.of(ConstantDescs.CD_int, SINK),
                        PUBLIC | ClassFile.ACC_STATIC,
                        cb -> cb.aload(0)
                                .new_(SUB).dup()
                                .invokespecial(SUB, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                                .invokeinterface(SINK, "take", TAKE)
                                .ireturn()));
    }

    static byte[] caller() {
        return ClassFile.of().build(CALLER, clb -> clb
                .withFlags(PUBLIC)
                .withInterfaceSymbols(SINK)
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                        ConstantDescs.MTD_void)
                                .return_())
                .withMethodBody("take", TAKE, PUBLIC,
                        cb -> cb.aload(1).invokevirtual(OPT, "v", INT).ireturn()));
    }

    /// Child-first for its own names; `delegate` answers the names it lists;
    /// `refused` names are refused (and counted).
    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();
        final Map<String, ClassLoader> delegate = new HashMap<>();
        final String refused;
        final AtomicInteger refusals = new AtomicInteger();

        Loader(String refused) {
            super(ClassLoader.getPlatformClassLoader());
            this.refused = refused;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                if (name.equals(refused)) {
                    refusals.incrementAndGet();
                    throw new ClassNotFoundException(name);
                }
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                ClassLoader d = delegate.get(name);
                if (d != null) {
                    return d.loadClass(name);
                }
                byte[] b = bytes.get(name);
                if (b == null) {
                    return super.loadClass(name, resolve);
                }
                return defineClass(name, b, 0, b.length);
            }
        }
    }

    public static void main(String[] args) throws Exception {
        Loader p = new Loader(null);
        p.bytes.put("l5r.Opt", withCtor(OPT, ConstantDescs.CD_Object, 7));
        p.bytes.put("l5r.Sub", withCtor(SUB, OPT, 8));
        p.bytes.put("l5r.Sink", sink());
        p.bytes.put("l5r.Driver", driver());
        Loader c = new Loader("l5r.Opt");
        c.bytes.put("l5r.Caller", caller());
        c.delegate.put("l5r.Sink", p);

        Class<?> sinkClass = p.loadClass("l5r.Sink");
        Method drive = p.loadClass("l5r.Driver").getMethod("drive", sinkClass);
        Object callerObj = c.loadClass("l5r.Caller").getConstructor().newInstance();
        for (int i = 0; i < 2; i++) {
            try {
                System.out.println("take#" + i + ": " + drive.invoke(null, callerObj));
            } catch (InvocationTargetException e) {
                Throwable t = e.getCause();
                System.out.println("take#" + i + ": " + t.getClass().getName() + ": " + t.getMessage());
            }
        }
        System.out.println("Opt requests from C: " + c.refusals.get());
    }
}
