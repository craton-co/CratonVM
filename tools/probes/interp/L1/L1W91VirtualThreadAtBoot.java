// Interpreter round i4 wave 91, lane L1: is `java.lang.VirtualThread` loaded
// before `main`, as HotSpot loads it with its well-known classes at genesis
// (`vmClassMacros.hpp`: `VirtualThread_klass`, `BaseVirtualThread_klass`)?
// Page docs/internal/fixed-bugs/interpreter-i49-L1-proposal-load-virtualthread-at-boot-as-hotspot-does-DONE-20261224.md,
// switch `CRATONVM_BOOT_LOADS_VIRTUAL_THREAD` (default off).
//
// The C agent's `GetLoadedClasses` answers whether each class is loaded at the
// start of `main`; then `Class.forName(.., false, null)` (load, no
// initialisation) and the `ClassPrepare` count since `VMInit`: 0 on HotSpot
// (both classes were prepared in the primordial phase, before any event).
//
// Needs the native shim tools/probes/interp/L1/L1W91VirtualThreadAtBoot.c,
// loaded both as an agent and with `System.load`:
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w91vt.so tools/probes/interp/L1/L1W91VirtualThreadAtBoot.c
//   javac -d /tmp/l1w91vt tools/probes/interp/L1/L1W91VirtualThreadAtBoot.java
//   java -agentpath:/tmp/libl1w91vt.so -cp /tmp/l1w91vt L1W91VirtualThreadAtBoot /tmp/libl1w91vt.so
//   cratonvm [--compatible] -agentpath:/tmp/libl1w91vt.so -cp /tmp/l1w91vt L1W91VirtualThreadAtBoot /tmp/libl1w91vt.so
//   CRATONVM_BOOT_LOADS_VIRTUAL_THREAD=1 cratonvm [--compatible] -agentpath:... (same arguments)
//
// Expected without the switch (code reading): `at main` rows false (or the
// VirtualThread row alone false), `prepares` 100 or 101 -- the class is first
// loaded by the probe's own `forName`. With the switch: HotSpot's lines.
// Without the argument it prints only the usage line.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, predicted from vmClassMacros.hpp; to measure)
// at main: VirtualThread=1 BaseVirtualThread=1
// forName: java.lang.VirtualThread
// after forName: VirtualThread=1 BaseVirtualThread=1
// prepares since VMInit: VirtualThread=0 BaseVirtualThread=0
// HOTSPOT_EXPECTED_END
public class L1W91VirtualThreadAtBoot {
    static native int loaded(String signature);

    static native int prepares();

    static String row() {
        return "VirtualThread=" + loaded("Ljava/lang/VirtualThread;")
                + " BaseVirtualThread=" + loaded("Ljava/lang/BaseVirtualThread;");
    }

    public static void main(String[] args) throws Exception {
        if (args.length < 1) {
            System.out.println("usage: L1W91VirtualThreadAtBoot <path of the shim, also given as -agentpath>");
            return;
        }
        System.load(args[0]);
        System.out.println("at main: " + row());
        Class<?> c = Class.forName("java.lang.VirtualThread", false, null);
        System.out.println("forName: " + c.getName());
        System.out.println("after forName: " + row());
        int p = prepares();
        System.out.println("prepares since VMInit: VirtualThread=" + (p / 100)
                + " BaseVirtualThread=" + (p % 100));
    }
}
