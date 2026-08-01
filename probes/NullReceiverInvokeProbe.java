/**
 * JVMS §6.5: invokevirtual / invokespecial / invokeinterface must throw
 * NullPointerException when objectref is null — BEFORE the callee frame is
 * pushed. If a VM instead pushes a frame with `this == null`, the failure
 * surfaces one frame deeper and names the wrong thing, and any registered
 * native for a 0-arg instance method that answers a null receiver with a null
 * RETURN converts it into a bogus "field X is null" NPE in the callee.
 *
 * That is exactly the shape of `Class.getInterfaces(Class.java:1217)`:
 * `Cannot read field "interfaces" because "rd" is null`, where the real fault
 * is a null `c` at `Class.isDirectSubType`'s `c.getInterfaces(false)`.
 */
public class NullReceiverInvokeProbe {

    interface Iface {
        int ifaceCall();
    }

    static class Impl implements Iface {
        public int ifaceCall() {
            return 1;
        }

        public int virtualCall() {
            return 2;
        }

        private int privateCall() {
            return 3;
        }

        // javac emits `invokespecial Impl.privateCall()` here — the same opcode
        // JDK's `Class.isDirectSubType` uses for `c.getInterfaces(boolean)`.
        static int callPrivateOn(Impl target) {
            return target.privateCall();
        }
    }

    static void check(String label, Runnable r) {
        try {
            r.run();
            System.out.println(label + " NO-THROW  <-- WRONG");
        } catch (NullPointerException npe) {
            StackTraceElement top = npe.getStackTrace().length > 0 ? npe.getStackTrace()[0] : null;
            System.out.println(label + " NPE ok msg=" + npe.getMessage() + " top=" + top);
        } catch (Throwable t) {
            System.out.println(label + " OTHER " + t);
        }
    }

    public static void main(String[] args) {
        Impl nullImpl = null;
        Iface nullIface = null;
        Class<?> nullClass = null;

        check("invokevirtual", () -> System.out.print(nullImpl.virtualCall() + ""));
        check("invokeinterface", () -> System.out.print(nullIface.ifaceCall() + ""));
        check("invokespecial-private", () -> System.out.print(Impl.callPrivateOn(nullImpl) + ""));
        check("invokevirtual-Class.getInterfaces",
                () -> System.out.print(nullClass.getInterfaces().length + ""));
        check("invokevirtual-Class.isSealed",
                () -> System.out.print(nullClass.isSealed() + ""));
        check("invokevirtual-Object.hashCode",
                () -> System.out.print(nullImpl.hashCode() + ""));

        // Warm each site so the inline caches are populated, then repeat: the
        // cached-invoke arms are a separate code path from the cold slow path.
        Impl real = new Impl();
        for (int i = 0; i < 50000; i++) {
            Impl.callPrivateOn(real);
            real.virtualCall();
            ((Iface) real).ifaceCall();
        }
        System.out.println("--- after warming inline caches ---");
        check("invokevirtual", () -> System.out.print(nullImpl.virtualCall() + ""));
        check("invokeinterface", () -> System.out.print(nullIface.ifaceCall() + ""));
        check("invokespecial-private", () -> System.out.print(Impl.callPrivateOn(nullImpl) + ""));
        System.out.println("PROBE-DONE");
    }
}
