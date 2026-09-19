// Isolates getfield/putfield cost, and the inherited-vs-own-class delta.
// Every arm has the SAME loop skeleton; only the body differs by one pair
// of field accesses (or, in the control, by two local ops of equal count).
public class FieldShape {
    static class Base { int a; }
    static class Sub extends Base { int pad; }

    static int sinkA;

    // control: same iteration count, same acc arithmetic, no heap access
    static int ctl(int n) {
        int acc = 0; int local = 0;
        for (int i = 0; i < n; i++) { acc += local; local = acc; }
        return acc;
    }
    // one getfield + one putfield, receiver class == declaring class
    static int own(int n, Base b) {
        int acc = 0;
        for (int i = 0; i < n; i++) { acc += b.a; b.a = acc; }
        return acc;
    }
    // one getfield + one putfield, receiver class != declaring class
    static int inh(int n, Sub s) {
        int acc = 0;
        for (int i = 0; i < n; i++) { acc += s.a; s.a = acc; }
        return acc;
    }
    // static field pair, for reference
    static int stat(int n) {
        int acc = 0;
        for (int i = 0; i < n; i++) { acc += sinkA; sinkA = acc; }
        return acc;
    }
    public static void main(String[] a) {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 1000000;
        int rounds = a.length > 1 ? Integer.parseInt(a[1]) : 7;
        Base b = new Base(); Sub s = new Sub();
        double mc=1e18, mo=1e18, mi=1e18, ms=1e18; int sink=0; long t;
        for (int r = 0; r < rounds; r++) {
            // forward order on even rounds, reverse on odd
            if ((r & 1) == 0) {
                t=System.nanoTime(); sink+=ctl(n);  mc=Math.min(mc,(System.nanoTime()-t)/(double)n);
                t=System.nanoTime(); sink+=own(n,b);mo=Math.min(mo,(System.nanoTime()-t)/(double)n);
                t=System.nanoTime(); sink+=inh(n,s);mi=Math.min(mi,(System.nanoTime()-t)/(double)n);
                t=System.nanoTime(); sink+=stat(n); ms=Math.min(ms,(System.nanoTime()-t)/(double)n);
            } else {
                t=System.nanoTime(); sink+=stat(n); ms=Math.min(ms,(System.nanoTime()-t)/(double)n);
                t=System.nanoTime(); sink+=inh(n,s);mi=Math.min(mi,(System.nanoTime()-t)/(double)n);
                t=System.nanoTime(); sink+=own(n,b);mo=Math.min(mo,(System.nanoTime()-t)/(double)n);
                t=System.nanoTime(); sink+=ctl(n);  mc=Math.min(mc,(System.nanoTime()-t)/(double)n);
            }
        }
        System.out.println("min ns/iter  control=" + mc + "  own=" + mo + "  inherited=" + mi + "  static=" + ms);
        System.out.println("per-field-pair over control:  own=" + (mo-mc) + "  inherited=" + (mi-mc) + "  static=" + (ms-mc));
        System.out.println("inherited - own (retarget cost per pair) = " + (mi-mo));
        if (sink == 42) System.out.println("x");
    }
}
