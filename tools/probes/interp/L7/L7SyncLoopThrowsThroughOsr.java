// Interpreter round i1 wave 7, lane L7 -- a `synchronized` method whose hot
// loop throws OUT of the method after OSR would have entered it.
//
// Since wave 10 (patch P1 of item 1 of
// docs/internal/fixed-bugs/interpreter-L7-osr-optimizing-door-bypasses-osr-admission-FIXED-20260925.md)
// neither OSR tier refuses a `synchronized` method: the OSR'd interpreter
// frame keeps the method monitor. This probe is P1's witness.
// Whatever tier runs the loop, the method's monitor must be released exactly
// once when the exception leaves it: `Thread.holdsLock` must answer false after
// every catch, and a second thread must be able to take the same monitor.
//
// HotSpot 25 prints exactly:
//   inst throw at 150000 sum 75000
//   inst throw at 150003 sum 75002
//   inst throw at 150006 sum 75003
//   inst throws 10 leaked 0 acc 2000000
//   static throw at 150000 sum 75000
//   static throw at 150003 sum 75002
//   static throw at 150006 sum 75003
//   static throws 10 leaked 0 acc 2000000
//   other thread inst done true acc 2000500
//   other thread static done true acc 2000500
//
// Compare `--compatible` with and without `--nojit`, and with
// CRATONVM_BG_COMPILE=0 (the inline OSR doors, optimizing arm first).
public class L7SyncLoopThrowsThroughOsr {
    long acc;
    static long sacc;

    synchronized long spin(int n, int throwAt) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += (i & 1);
            if (i == throwAt) {
                throw new IllegalStateException("at " + i + " sum " + s);
            }
        }
        acc += s;
        return s;
    }

    static synchronized long sspin(int n, int throwAt) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += (i & 1);
            if (i == throwAt) {
                throw new IllegalStateException("at " + i + " sum " + s);
            }
        }
        sacc += s;
        return s;
    }

    public static void main(String[] a) throws Exception {
        final L7SyncLoopThrowsThroughOsr obj = new L7SyncLoopThrowsThroughOsr();
        int throwsSeen = 0;
        int leaked = 0;
        for (int round = 0; round < 30; round++) {
            int throwAt = round % 3 == 0 ? 150000 + round : -1;
            try {
                obj.spin(200000, throwAt);
            } catch (IllegalStateException e) {
                if (throwsSeen < 3) {
                    System.out.println("inst throw " + e.getMessage());
                }
                throwsSeen++;
            }
            if (Thread.holdsLock(obj)) {
                leaked++;
            }
        }
        System.out.println("inst throws " + throwsSeen + " leaked " + leaked + " acc " + obj.acc);

        throwsSeen = 0;
        leaked = 0;
        for (int round = 0; round < 30; round++) {
            int throwAt = round % 3 == 0 ? 150000 + round : -1;
            try {
                sspin(200000, throwAt);
            } catch (IllegalStateException e) {
                if (throwsSeen < 3) {
                    System.out.println("static throw " + e.getMessage());
                }
                throwsSeen++;
            }
            if (Thread.holdsLock(L7SyncLoopThrowsThroughOsr.class)) {
                leaked++;
            }
        }
        System.out.println("static throws " + throwsSeen + " leaked " + leaked + " acc " + sacc);

        Thread t1 = new Thread(() -> obj.spin(1000, -1));
        t1.setDaemon(true); // a leaked monitor must not hang the exit
        t1.start();
        t1.join(10000);
        System.out.println("other thread inst done " + !t1.isAlive() + " acc " + obj.acc);
        Thread t2 = new Thread(() -> sspin(1000, -1));
        t2.setDaemon(true);
        t2.start();
        t2.join(10000);
        System.out.println("other thread static done " + !t2.isAlive() + " acc " + sacc);
    }
}
