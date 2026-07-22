// Does the (ThreadGroup, Runnable, String) ctor — the one Block A already
// intercepts with populate_real_thread_holder — actually run its target?
// If yes, native <init> override works in real mode and my fix is wrong-shaped.
// If no, the native <init> override isn't consulted for real-bytecode ctors.
public class R0Group {
    static volatile boolean ran = false;
    public static void main(String[] args) throws Exception {
        System.out.println("R0Group START");
        ThreadGroup g = Thread.currentThread().getThreadGroup();
        Thread t = new Thread(g, () -> { ran = true; System.out.println("GROUP TARGET RAN"); }, "gt");
        t.start();
        t.join(10_000);
        System.out.println("R0Group ran=" + ran + " alive=" + t.isAlive());
    }
}
