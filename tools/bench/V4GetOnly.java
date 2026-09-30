import java.util.ArrayList;
// V4: ArrayList.get and NOTHING else -- no casts, no accessors. If the
// always/never gap survives here, it is ArrayList.get's own body.
public class V4GetOnly {
    static int step(ArrayList<Object> xs, int acc, int i){
        Object o = xs.get(i & 15);
        return acc * 31 + (o == null ? 0 : 1);
    }
    public static void main(String[] a){
        int reps = a.length>0?Integer.parseInt(a[0]):4_000_000;
        ArrayList<Object> xs = new ArrayList<>();
        for (int i=0;i<16;i++) xs.add(new Object());
        int warm=0; for(int i=0;i<3_000_000;i++) warm=step(xs,warm,i);
        long t0=System.nanoTime(); int acc=0;
        for(int i=0;i<reps;i++) acc=step(xs,acc,i);
        System.out.println("V4 get-only ("+reps+") : "+((System.nanoTime()-t0)/1_000_000L)+" ms  ["+acc+"]");
        if(warm==0x7FFFFFFF) System.out.println(warm);
    }
}
