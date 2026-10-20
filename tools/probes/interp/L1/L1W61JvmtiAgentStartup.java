// Interpreter round i2 wave 61, lane L1: the C JVMTI functions native agents
// call first at startup that the table still answered with
// JVMTI_ERROR_NOT_AVAILABLE (98), against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-lacks-the-functions-and-events-agents-call-first-FIXED-20261007.md):
// the VM system properties (130-132; async-profiler, YourKit and most
// loggers read `java.vm.*` and `java.home` first), the class search
// additions (149 / 151; an agent injecting its runtime classes),
// `GetObjectHashCode` (58), `GetObjectSize` (154) and `GetTimerInfo` (138).
//
// Rows:
//
// * `onload ...`: what `Agent_OnLoad` saw (the OnLoad phase);
// * `prop <key>`: `GetSystemProperty` in the live phase, `same=1` when the
//   answer is `System.getProperty(key)`; a key the VM does not list answers
//   an error;
// * `props`: `GetSystemProperties`, whether the list holds a few keys;
// * `set ...`: `SetSystemProperty` in the live phase;
// * `hash` / `size ...`: `GetObjectHashCode` (compared with
//   `System.identityHashCode`) and `GetObjectSize`;
// * `boot ...` / `system ...`: the class search additions, then whether the
//   bootstrap loader finds a class of the added jar and the system loader a
//   resource of the other one.
//
// The first-call rows run again in a second round where they make sense.
//
// Needs the native shim tools/probes/interp/L1/L1W61JvmtiAgentStartup.c,
// loaded both as an agent and with `System.load` (for the natives):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w61start.so tools/probes/interp/L1/L1W61JvmtiAgentStartup.c
//   javac -d /tmp/l1w61start tools/probes/interp/L1/L1W61JvmtiAgentStartup.java
//   java     -agentpath:/tmp/libl1w61start.so -cp /tmp/l1w61start L1W61JvmtiAgentStartup /tmp/libl1w61start.so
//   cratonvm -agentpath:/tmp/libl1w61start.so -cp /tmp/l1w61start L1W61JvmtiAgentStartup /tmp/libl1w61start.so
//
// Without the argument it prints only the usage line. The Rust port of the
// shim is tools/probes/interp/L1/rust-ports/L1W61JvmtiAgentStartup (README in
// tools/probes/interp/L1/rust-ports/).
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim (same calls, same order, same output), three runs, the
// same each time:
//
//   onload GetSystemProperty java.vm.specification.version: err=0 value=25
//   onload GetSystemProperty java.home: err=0
//   onload GetSystemProperty l1w61.missing: err=98
//   onload GetSystemProperties: err=0 java.home=1 java.vm.version=1 user.dir=0 os.name=0
//   onload SetSystemProperty java.vm.version: err=98
//   onload SetSystemProperty l1w61.missing: err=98
//   onload SetSystemProperty java.library.path to itself: err=0
//   onload GetTimerInfo: err=0
//   round 1
//   prop java.vm.specification.name: err=0 same=1
//   prop java.vm.specification.vendor: err=0 same=1
//   prop java.vm.specification.version: err=0 same=1
//   prop java.vm.name: err=0 same=1
//   prop java.vm.vendor: err=0 same=1
//   prop java.vm.version: err=0 same=1
//   prop java.vm.info: err=0 same=1
//   prop java.home: err=0 same=1
//   prop java.class.path: err=0 same=1
//   prop java.library.path: err=0 same=1
//   prop sun.boot.library.path: err=0 same=1
//   prop jdk.debug: err=0 same=1
//   prop user.dir: err=98
//   prop user.home: err=98
//   prop os.name: err=98
//   prop file.encoding: err=98
//   prop line.separator: err=98
//   prop java.version: err=98
//   prop l1w61.missing: err=98
//   props: err=0 java.vm.specification.name=1 java.vm.specification.vendor=1 java.vm.specification.version=1 java.vm.name=1 java.vm.vendor=1 java.vm.version=1 java.vm.info=1 java.home=1 java.class.path=1 java.library.path=1 sun.boot.library.path=1 jdk.debug=1 user.dir=0 user.home=0 os.name=0 file.encoding=0 line.separator=0 java.version=0 l1w61.missing=0
//   set java.vm.version: err=112
//   set java.class.path: err=112
//   null GetSystemProperty key: err=100
//   null GetSystemProperty value_ptr: err=100
//   null GetSystemProperties count_ptr: err=100
//   null GetSystemProperties property_ptr: err=100
//   null SetSystemProperty key: err=112
//   null GetObjectHashCode object: err=20
//   null GetObjectHashCode hash_code_ptr: err=100
//   null GetObjectSize object: err=20
//   null GetObjectSize size_ptr: err=100
//   null GetTimerInfo: err=100
//   null AddToBootstrapClassLoaderSearch: err=100
//   null AddToSystemClassLoaderSearch: err=100
//   timer: err=0 max=-1 forward=0 backward=0 kind=32
//   hash: err=0 same=1
//   hash string: err=0 same=1 not-hashCode=1
//   size Object: 16
//   size int[10]: 56
//   size long[0]: 16
//   size byte[100]: 120
//   size Object[3]: 32
//   size String: 24
//   size Integer: 16
//   size Class: positive
//   round 2
//   prop java.vm.specification.name: err=0 same=1
//   prop java.vm.specification.vendor: err=0 same=1
//   prop java.vm.specification.version: err=0 same=1
//   prop java.vm.name: err=0 same=1
//   prop java.vm.vendor: err=0 same=1
//   prop java.vm.version: err=0 same=1
//   prop java.vm.info: err=0 same=1
//   prop java.home: err=0 same=1
//   prop java.class.path: err=0 same=1
//   prop java.library.path: err=0 same=1
//   prop sun.boot.library.path: err=0 same=1
//   prop jdk.debug: err=0 same=1
//   prop user.dir: err=98
//   prop user.home: err=98
//   prop os.name: err=98
//   prop file.encoding: err=98
//   prop line.separator: err=98
//   prop java.version: err=98
//   prop l1w61.missing: err=98
//   props: err=0 java.vm.specification.name=1 java.vm.specification.vendor=1 java.vm.specification.version=1 java.vm.name=1 java.vm.vendor=1 java.vm.version=1 java.vm.info=1 java.home=1 java.class.path=1 java.library.path=1 sun.boot.library.path=1 jdk.debug=1 user.dir=0 user.home=0 os.name=0 file.encoding=0 line.separator=0 java.version=0 l1w61.missing=0
//   set java.vm.version: err=112
//   set java.class.path: err=112
//   null GetSystemProperty key: err=100
//   null GetSystemProperty value_ptr: err=100
//   null GetSystemProperties count_ptr: err=100
//   null GetSystemProperties property_ptr: err=100
//   null SetSystemProperty key: err=112
//   null GetObjectHashCode object: err=20
//   null GetObjectHashCode hash_code_ptr: err=100
//   null GetObjectSize object: err=20
//   null GetObjectSize size_ptr: err=100
//   null GetTimerInfo: err=100
//   null AddToBootstrapClassLoaderSearch: err=100
//   null AddToSystemClassLoaderSearch: err=100
//   timer: err=0 max=-1 forward=0 backward=0 kind=32
//   hash: err=0 same=1
//   hash string: err=0 same=1 not-hashCode=1
//   size Object: 16
//   size int[10]: 56
//   size long[0]: 16
//   size byte[100]: 120
//   size Object[3]: 32
//   size String: 24
//   size Integer: 16
//   size Class: positive
//   boot before: not found
//   system before: not found
//   boot missing: err=103
//   boot not a jar: err=103
//   boot directory: err=103
//   system missing: err=103
//   system not a jar: err=103
//   boot jar: err=0
//   system jar: err=0
//   boot after: found loader=boot
//   system after: found
//
// CratonVM before wave 61 (code read): `err=98` for every one of these
// functions (the slots were `not_available`): every `onload`, `prop`,
// `set`, `null`, `timer`, `hash` and search row, and the sizes `-98`.
// Since wave 61 (a debug `cratonvm` with `experimental-debug`,
// `--jdk-only` and `--compatible`): HotSpot's lines
// except the two `size Object[3]` rows, `40` here (this VM's references are
// 8 bytes; `Instrumentation.getObjectSize` answers the same):
// docs/internal/fixed-bugs/interpreter-L1-get-object-size-answers-this-vms-footprint-not-hotspots-FIXED-20261026.md.
// Since wave 62 (lane L1), with `CRATONVM_HOTSPOT_OBJECT_SIZE` on (the
// default from the lane's switch-on commit): HotSpot's `32` there too.
import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.HashSet;
import java.util.Set;
import java.util.jar.JarEntry;
import java.util.jar.JarOutputStream;

public class L1W61JvmtiAgentStartup {
    /** The lines `Agent_OnLoad` recorded. */
    static native String onload();

    /** `GetSystemProperty(key)`: `=value`, or `!err`. */
    static native String getProp(String key);

    /** `GetSystemProperties`: `!err`, or the keys, one per line. */
    static native String propKeys();

    /** `SetSystemProperty(key, value)`'s error. */
    static native int setProp(String key, String value);

    /** The NULL-argument rows, one line each. */
    static native String nullRows(Object any);

    /** `GetObjectHashCode`: `(err << 32) | hash`. */
    static native long hash(Object o);

    /** `GetObjectSize`: the size, or `-err`. */
    static native long size(Object o);

    /** `GetTimerInfo`'s line. */
    static native String timer();

    /** `AddToBootstrapClassLoaderSearch(path)`'s error. */
    static native int addBoot(String path);

    /** `AddToSystemClassLoaderSearch(path)`'s error. */
    static native int addSystem(String path);

    static final String[] VM_KEYS = {
        "java.vm.specification.name",
        "java.vm.specification.vendor",
        "java.vm.specification.version",
        "java.vm.name",
        "java.vm.vendor",
        "java.vm.version",
        "java.vm.info",
        "java.home",
        "java.class.path",
        "java.library.path",
        "sun.boot.library.path",
        "jdk.debug",
    };

    static final String[] OTHER_KEYS = {
        "user.dir", "user.home", "os.name", "file.encoding", "line.separator", "java.version",
        "l1w61.missing",
    };

    static void props() {
        for (String key : VM_KEYS) {
            String got = getProp(key);
            if (got.startsWith("=")) {
                String want = System.getProperty(key);
                boolean same = got.substring(1).equals(want);
                System.out.println("prop " + key + ": err=0 same=" + (same ? 1 : 0));
            } else {
                System.out.println("prop " + key + ": err=" + got.substring(1));
            }
        }
        for (String key : OTHER_KEYS) {
            String got = getProp(key);
            System.out.println("prop " + key + ": err=" + (got.startsWith("=") ? "0" : got.substring(1)));
        }
        String keys = propKeys();
        if (keys.startsWith("!")) {
            System.out.println("props: err=" + keys.substring(1));
        } else {
            Set<String> listed = new HashSet<>(Arrays.asList(keys.split("\n")));
            StringBuilder row = new StringBuilder("props: err=0");
            for (String key : VM_KEYS) {
                row.append(' ').append(key).append('=').append(listed.contains(key) ? 1 : 0);
            }
            for (String key : OTHER_KEYS) {
                row.append(' ').append(key).append('=').append(listed.contains(key) ? 1 : 0);
            }
            System.out.println(row);
        }
    }

    static void objects() {
        Object plain = new Object();
        long h = hash(plain);
        int err = (int) (h >>> 32);
        System.out.println("hash: err=" + err + " same=" + ((int) h == System.identityHashCode(plain) ? 1 : 0));
        String text = new String(new char[] {'h', 'e', 'l', 'l', 'o'});
        long h2 = hash(text);
        System.out.println("hash string: err=" + (int) (h2 >>> 32) + " same="
            + ((int) h2 == System.identityHashCode(text) ? 1 : 0)
            + " not-hashCode=" + ((int) h2 != text.hashCode() ? 1 : 0));
        System.out.println("size Object: " + size(new Object()));
        System.out.println("size int[10]: " + size(new int[10]));
        System.out.println("size long[0]: " + size(new long[0]));
        System.out.println("size byte[100]: " + size(new byte[100]));
        System.out.println("size Object[3]: " + size(new Object[3]));
        System.out.println("size String: " + size(text));
        System.out.println("size Integer: " + size(Integer.valueOf(1000)));
        System.out.println("size Class: " + (size(String.class) > 0 ? "positive" : "" + size(String.class)));
    }

    static byte[] helperBytes() throws Exception {
        try (InputStream in = L1W61JvmtiAgentStartup.class.getResourceAsStream("/L1W61BootHelper.class")) {
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            in.transferTo(out);
            return out.toByteArray();
        }
    }

    static void jar(Path path, String entry, byte[] bytes) throws Exception {
        try (JarOutputStream out = new JarOutputStream(Files.newOutputStream(path))) {
            out.putNextEntry(new JarEntry(entry));
            out.write(bytes);
            out.closeEntry();
        }
    }

    static String bootFinds() {
        try {
            Class<?> c = Class.forName("L1W61BootHelper", false, null);
            return "found loader=" + (c.getClassLoader() == null ? "boot" : "other");
        } catch (ClassNotFoundException e) {
            return "not found";
        }
    }

    static String systemFinds() {
        return ClassLoader.getSystemResource("l1w61/sys.txt") != null ? "found" : "not found";
    }

    static void search() throws Exception {
        Path dir = Files.createTempDirectory("l1w61start");
        Path boot = dir.resolve("boot.jar");
        Path sys = dir.resolve("sys.jar");
        Path plain = dir.resolve("plain.txt");
        jar(boot, "L1W61BootHelper.class", helperBytes());
        jar(sys, "l1w61/sys.txt", "hi".getBytes("UTF-8"));
        Files.write(plain, "not a jar".getBytes("UTF-8"));
        System.out.println("boot before: " + bootFinds());
        System.out.println("system before: " + systemFinds());
        System.out.println("boot missing: err=" + addBoot(dir.resolve("missing.jar").toString()));
        System.out.println("boot not a jar: err=" + addBoot(plain.toString()));
        System.out.println("boot directory: err=" + addBoot(dir.toString()));
        System.out.println("system missing: err=" + addSystem(dir.resolve("missing.jar").toString()));
        System.out.println("system not a jar: err=" + addSystem(plain.toString()));
        System.out.println("boot jar: err=" + addBoot(boot.toString()));
        System.out.println("system jar: err=" + addSystem(sys.toString()));
        System.out.println("boot after: " + bootFinds());
        System.out.println("system after: " + systemFinds());
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W61JvmtiAgentStartup <path of the shim library>");
            return;
        }
        System.load(args[0]);
        System.out.print(onload());
        for (int round = 1; round <= 2; round++) {
            System.out.println("round " + round);
            props();
            System.out.println("set java.vm.version: err=" + setProp("java.vm.version", "x"));
            System.out.println("set java.class.path: err=" + setProp("java.class.path", System.getProperty("java.class.path")));
            System.out.print(nullRows(new Object()));
            System.out.print(timer());
            objects();
        }
        search();
    }
}

/** Loaded only by the bootstrap loader, from the jar the probe writes. */
class L1W61BootHelper {
}
