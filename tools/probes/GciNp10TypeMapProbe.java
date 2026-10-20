// GC round gci, wave i10, lane np (2026-10-13): the interpreter's frame root
// scan against HotSpot's INTERPRETER oop maps, which are type maps
// (`GenerateOopMap`), not liveness maps. Page:
// docs/known-issues/gc/gci-i10np-interpreted-frames-drop-liveness-dead-reference-locals-hotspot-keeps-20261013.md
//
// Three cases, each a method entered ONCE (interpreted on HotSpot, default or
// -Xint) that parks in `Thread.join` while a collector thread runs two
// System.gc(). Unlike GciOb5ParkedDeadLocalProbe there is no compiled spinner,
// so on a JIT build of this VM nothing arms the conservative-locals rule and the
// interpreter's own frame scan decides; the collector polls with sleep(1), so
// its loop stays cold too.
//
//   dead-typed-local  a scoped-out `byte[]` local, dead after its last read but
//                     a reference on the only path to the join: HotSpot keeps
//                     it (`kept`).
//   join-conflict     the array was stored on ONE branch of an `if`; at the
//                     join the slot merges a reference with an unwritten slot,
//                     a conflict GenerateOopMap does not report as an oop:
//                     HotSpot drops it (`cleared`), expected by reading
//                     `CellTypeState::merge`; the -Xint row confirms it.
//   dead-param        a reference PARAMETER read once, then dead: a parameter
//                     is reference-typed from the entry, so HotSpot keeps it
//                     (`kept`).
//
// stdout (HotSpot 25, -XX:+UseSerialGC -Xmx64m, default and -Xint), expected:
//
//   dead-typed-local=kept
//   join-conflict=cleared
//   dead-param=kept
//   done
//
// Nothing timing-dependent is printed. Diagnostics: none.
//
//   javac -d out GciNp10TypeMapProbe.java
//   java -XX:+UseSerialGC -Xmx64m -Xint -cp out GciNp10TypeMapProbe
//   cratonvm --java-home $JDK -XX:+UseGenerationalGC -Xmx64m --nojit -cp out GciNp10TypeMapProbe
import java.lang.ref.WeakReference;

public class GciNp10TypeMapProbe {
    static WeakReference<byte[]> ref;
    static volatile long sink;
    static Thread mainThread;

    public static void main(String[] args) throws Exception {
        mainThread = Thread.currentThread();
        System.out.println("dead-typed-local=" + verdict(deadTypedLocal()));
        System.out.println("join-conflict=" + verdict(joinConflict(args.length == 0)));
        System.out.println("dead-param=" + verdict(deadParam(make())));
        System.out.println("done");
    }

    static String verdict(boolean cleared) {
        return cleared ? "cleared" : "kept";
    }

    static Thread collector() {
        return new Thread(GciNp10TypeMapProbe::collect, "np10-collector");
    }

    static boolean deadTypedLocal() throws Exception {
        byte[] dead = new byte[1 << 20];
        dead[0] = 7;
        ref = new WeakReference<>(dead);
        sink += dead.length; // the last read of `dead`
        Thread gc = collector();
        gc.start();
        gc.join(); // parked: `dead` is liveness-dead and reference-typed
        return ref.get() == null;
    }

    static boolean joinConflict(boolean store) throws Exception {
        Thread gc = collector();
        if (store) {
            byte[] a = new byte[1 << 20];
            a[0] = 7;
            ref = new WeakReference<>(a);
            sink += a.length;
        }
        gc.start();
        gc.join(); // parked: `a`'s slot is a reference on one path only
        return ref.get() == null;
    }

    static byte[] make() {
        byte[] b = new byte[1 << 20];
        b[0] = 7;
        ref = new WeakReference<>(b);
        return b;
    }

    static boolean deadParam(byte[] p) throws Exception {
        sink += p.length; // the last read of `p`
        Thread gc = collector();
        gc.start();
        gc.join(); // parked: `p` is liveness-dead and a reference parameter
        return ref.get() == null;
    }

    static void collect() {
        try {
            while (mainThread.getState() != Thread.State.WAITING) {
                Thread.sleep(1);
            }
        } catch (InterruptedException e) {
            throw new RuntimeException(e);
        }
        System.gc();
        System.gc();
    }
}
