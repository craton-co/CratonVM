import java.time.Instant;
/** Single-arm probe: Instant.getNano() only. 3.9ns on HotSpot, ~2200ns on CratonVM. */
public class GetNanoProbe {
    static long sink;
    static void loop(int n, Instant a){ for(int i=0;i<n;i++) sink+=a.getNano(); }
    public static void main(String[] args){
        int n = args.length>0?Integer.parseInt(args[0]):300000;
        Instant a = Instant.ofEpochSecond(1700000000L, 123456789L);
        loop(10000, a);
        long t0=System.nanoTime(); loop(n,a); long d=System.nanoTime()-t0;
        System.out.printf("Instant.getNano %10.1f ns/op  sink=%d%n",(double)d/n,sink);
        System.out.flush();
    }
}
