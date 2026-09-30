// Prices ONE interpreted backward branch: two loops with identical total
// body-bytecode counts but 8x different back-edge counts.
public class BackEdge {
    static int tight(int n) {           // n back edges, n bodies
        int acc = 0;
        for (int i = 0; i < n; i++) { acc += i; }
        return acc;
    }
    static int unrolled(int n) {        // n/8 back edges, n bodies
        int acc = 0;
        for (int i = 0; i < n; i += 8) {
            acc += i; acc += i+1; acc += i+2; acc += i+3;
            acc += i+4; acc += i+5; acc += i+6; acc += i+7;
        }
        return acc;
    }
    public static void main(String[] a) {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 8000000;
        int rounds = a.length > 1 ? Integer.parseInt(a[1]) : 9;
        double mt = 1e18, mu = 1e18; int sink = 0; long t;
        for (int r = 0; r < rounds; r++) {
            if ((r & 1) == 0) {
                t=System.nanoTime(); sink+=tight(n);    mt=Math.min(mt,(System.nanoTime()-t)/(double)n);
                t=System.nanoTime(); sink+=unrolled(n); mu=Math.min(mu,(System.nanoTime()-t)/(double)n);
            } else {
                t=System.nanoTime(); sink+=unrolled(n); mu=Math.min(mu,(System.nanoTime()-t)/(double)n);
                t=System.nanoTime(); sink+=tight(n);    mt=Math.min(mt,(System.nanoTime()-t)/(double)n);
            }
        }
        // tight has 1 back edge per element; unrolled has 1/8.
        double perBackEdge = (mt - mu) / (1.0 - 1.0/8.0);
        System.out.println("tight=" + mt + " ns/elem   unrolled=" + mu + " ns/elem");
        System.out.println("cost of one interpreted BACK EDGE ~= " + perBackEdge + " ns");
        if (sink == 42) System.out.println("x");
    }
}
