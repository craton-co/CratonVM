import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;

/**
 * `defineHiddenClass(bytes, initialize=true)` on a class whose `<clinit>` THROWS.
 *
 * JVMS 5.5: an initialization failure is reported to the caller — the original
 * `Error`, or `ExceptionInInitializerError` wrapping anything else. So asking for
 * initialization and getting a usable Lookup back is a silent wrong result: the
 * caller holds a class whose static state never ran.
 *
 * The two halves are asked separately because they have different answers:
 *   initialize=false  -> defining is fine, NO exception (nothing ran yet)
 *   initialize=true   -> must throw
 *
 * Determinism: no addresses, no hashes, no timing. Only the exception's class
 * name, its cause's class name, and whether a Lookup came back.
 */
public class L2InitFail {
    static final String NAME = "L2InitFailVictim";

    /** A class whose `<clinit>` is `throw new RuntimeException("boom")`. */
    static byte[] victim() {
        ClassDesc self = ClassDesc.of(NAME);
        ClassDesc rte = ClassDesc.of("java.lang.RuntimeException");
        return ClassFile.of().build(self, clb -> {
            clb.withSuperclass(ConstantDescs.CD_Object);
            clb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_FINAL);
            clb.withMethodBody(ConstantDescs.CLASS_INIT_NAME, MethodTypeDesc.of(ConstantDescs.CD_void),
                    ClassFile.ACC_STATIC,
                    cob -> cob.new_(rte)
                              .dup()
                              .loadConstant("boom")
                              .invokespecial(rte, ConstantDescs.INIT_NAME,
                                      MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_String))
                              .athrow());
        });
    }

    public static void main(String[] args) throws Throwable {
        byte[] bytes = victim();
        System.out.println("CK bytes=" + bytes.length);

        // Half 1 — no initialization requested: defining must SUCCEED quietly.
        try {
            MethodHandles.Lookup l = MethodHandles.lookup().defineHiddenClass(bytes, false);
            System.out.println("ROW noinit: OK lookup=" + (l != null)
                    + " class=" + (l == null ? "<null>" : l.lookupClass().getName()
                            .replaceAll("/0x[0-9a-fA-F]+", "/0xSCRUBBED")));
        } catch (Throwable t) {
            System.out.println("ROW noinit: THREW " + t.getClass().getName() + ": " + t.getMessage());
        }

        // Half 2 — initialization requested and `<clinit>` throws. The contract.
        try {
            MethodHandles.Lookup l = MethodHandles.lookup().defineHiddenClass(bytes, true);
            System.out.println("ROW init: *** NO THROW — returned lookup=" + (l != null)
                    + ", so a class whose <clinit> failed was handed back as initialized");
        } catch (Throwable t) {
            String cause = t.getCause() == null ? "<none>" : t.getCause().getClass().getName();
            System.out.println("ROW init: THREW " + t.getClass().getName()
                    + " cause=" + cause + " msg=" + t.getMessage());
        }
        System.out.println("CK done");
    }
}
