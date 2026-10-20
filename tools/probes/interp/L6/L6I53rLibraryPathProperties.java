// The i53 residual: `jdk-only` dropped an explicit `-Djava.library.path`
// (docs/internal/fixed-bugs/jdk-only-i53r-jdk-only-drops-an-explicit-java-library-path-FIXED-20261007.md).
//
// Prints both library path properties, then `System.loadLibrary(args[0])`
// (an application library HotSpot finds through `-Djava.library.path`), then
// uses two JDK natives that load through `sun.boot.library.path` (libzip's
// `Inflater`, libnet's `InetAddress`).
//
// Run unset, with `-Djava.library.path=<dir holding the library>`, and with
// `-Dsun.boot.library.path=<anything>` (HotSpot appends it to the default).
public class L6I53rLibraryPathProperties {
    public static void main(String[] args) throws Exception {
        System.out.println("java.library.path=" + System.getProperty("java.library.path"));
        System.out.println("sun.boot.library.path=" + System.getProperty("sun.boot.library.path"));
        String name = args.length > 0 ? args[0] : "zzlp";
        try {
            System.loadLibrary(name);
            System.out.println("loadLibrary(" + name + "): loaded");
        } catch (UnsatisfiedLinkError e) {
            System.out.println("loadLibrary(" + name + "): " + e.getMessage());
        }
        java.util.zip.Inflater inf = new java.util.zip.Inflater();
        System.out.println("inflater: " + inf.getRemaining());
        inf.end();
        System.out.println("loopback: " + java.net.InetAddress.getLoopbackAddress().getHostAddress());
    }
}
