// Disambiguate: does the child thread body EXECUTE at all? It prints directly
// from inside the thread. If "CHILD RAN" appears, threads run and the problem
// is cross-thread visibility; if it never appears, Thread.start is a no-op.
public class R0Print {
    public static void main(String[] args) throws Exception {
        System.out.println("R0P START main-thread=" + Thread.currentThread().getName());
        Thread t = new Thread(() -> {
            System.out.println("CHILD RAN on thread=" + Thread.currentThread().getName());
        }, "child");
        t.start();
        t.join(10_000);
        System.out.println("R0P after join, child alive=" + t.isAlive());
        System.out.flush();
    }
}
