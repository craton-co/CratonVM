// Interpreter round i4 wave 87, lane L1: JVMTI object tags of objects the
// collector moves, against HotSpot. A C agent tags 1000 objects, the program
// allocates enough for young collections to move them, and the tags must
// still be where they were put: GetTag of each, GetObjectsWithTags (count,
// tag sum, and that each result is the very object tagged so), FollowReferences
// from the holding array, ForceGarbageCollection, and one ObjectFree per
// tagged object that died (posted after the collection, from a service
// thread on both VMs: the probe polls for up to five seconds).
//
// CratonVM fixes this probe guards (wave 87): GetTag / SetTag decoded the
// handle and looked the address up in two steps a collection could fall
// between; GetObjectsWithTags resolved its objects before entering the VM;
// the heap walks refreshed the tag table's address index before their pause
// and read the initial object before it
// (vm/src/jvmti/native_env/heap.rs).
//
// Build (Linux gcc):
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w87tags.so tools/probes/interp/L1/L1W87TagsAcrossGc.c
//   javac -d /tmp/l1w87tags tools/probes/interp/L1/L1W87TagsAcrossGc.java
//   java     -agentpath:/tmp/libl1w87tags.so -cp /tmp/l1w87tags L1W87TagsAcrossGc /tmp/libl1w87tags.so
//   cratonvm -agentpath:/tmp/libl1w87tags.so -cp /tmp/l1w87tags L1W87TagsAcrossGc /tmp/libl1w87tags.so
//   cratonvm --compatible -agentpath:/tmp/libl1w87tags.so -cp /tmp/l1w87tags L1W87TagsAcrossGc /tmp/libl1w87tags.so
// Build (Windows MSVC, a Developer Command Prompt):
//   cl /LD /I"%JAVA_HOME%\include" /I"%JAVA_HOME%\include\win32" tools\probes\interp\L1\L1W87TagsAcrossGc.c /Fe:l1w87tags.dll
//   javac -d l1w87tags tools\probes\interp\L1\L1W87TagsAcrossGc.java
//   java -agentpath:%CD%\l1w87tags.dll -cp l1w87tags L1W87TagsAcrossGc %CD%\l1w87tags.dll
// (a cratonvm built with --features experimental-debug)
//
// HOTSPOT_EXPECTED_BEGIN
// tagAll: 0
// mismatches after collections: 0
// withTags: 1000 1499500 1000
// forceGc: 0
// objectFree: 500 750000
// mismatches after freeing: 0
// withTags after freeing: 500 749500 500
// reached from the array: 500
// HOTSPOT_EXPECTED_END
//
// Derived from the JVMTI specification, not yet measured on HotSpot.
public class L1W87TagsAcrossGc {
    static final class Item {
        final int value;

        Item(int value) {
            this.value = value;
        }
    }

    static native int tagAll(Object[] objs, long base);

    static native int mismatches(Object[] objs, long base);

    static native String withTags(Object[] objs, long base, int n);

    static native int reached(Object root, long base, int n);

    static native int forceGc();

    static native long freedCount();

    static native long freedSum();

    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W87TagsAcrossGc <path of the shim library>");
            return;
        }
        System.load(args[0]);
        final int n = 1000;
        final long base = 1000;
        Object[] objs = new Object[n];
        for (int i = 0; i < n; i++) {
            objs[i] = new Item(i);
        }
        System.out.println("tagAll: " + tagAll(objs, base));
        // Young collections move the tagged objects.
        for (int round = 0; round < 200_000; round++) {
            sink = new byte[128];
        }
        System.gc();
        System.out.println("mismatches after collections: " + mismatches(objs, base));
        System.out.println("withTags: " + withTags(objs, base, n));
        for (int i = 1; i < n; i += 2) {
            objs[i] = null;
        }
        System.out.println("forceGc: " + forceGc());
        long deadline = System.nanoTime() + 5_000_000_000L;
        while (freedCount() < n / 2 && System.nanoTime() < deadline) {
            Thread.sleep(10);
            forceGc();
        }
        System.out.println("objectFree: " + freedCount() + " " + freedSum());
        System.out.println("mismatches after freeing: " + mismatches(objs, base));
        System.out.println("withTags after freeing: " + withTags(objs, base, n));
        System.out.println("reached from the array: " + reached(objs, base, n));
    }
}
