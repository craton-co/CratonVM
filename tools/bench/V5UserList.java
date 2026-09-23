// V5: a user-defined container with ArrayList.get's SHAPE -- a bounds check
// then an array load -- but no JDK internals and no native shadow anywhere.
// Separates "the optimizing tier is bad at this shape" from "the optimizing
// tier is bad at ArrayList.get specifically".
public class V5UserList {
    static final class MyList {
        final Object[] a; final int n;
        MyList(Object[] a){ this.a=a; this.n=a.length; }
        Object get(int i){
            if (i < 0 || i >= n) throw new IndexOutOfBoundsException("Index: " + i);
            return a[i];
        }
    }
    static int step(MyList xs, int acc, int i){
        Object o = xs.get(i & 15);
        return acc * 31 + (o == null ? 0 : 1);
    }
    public static void main(String[] a){
        int reps = a.length>0?Integer.parseInt(a[0]):4_000_000;
        Object[] raw = new Object[16];
        for (int i=0;i<16;i++) raw[i]=new Object();
        MyList xs = new MyList(raw);
        int warm=0; for(int i=0;i<3_000_000;i++) warm=step(xs,warm,i);
        long t0=System.nanoTime(); int acc=0;
        for(int i=0;i<reps;i++) acc=step(xs,acc,i);
        System.out.println("V5 user-list ("+reps+") : "+((System.nanoTime()-t0)/1_000_000L)+" ms  ["+acc+"]");
        if(warm==0x7FFFFFFF) System.out.println(warm);
    }
}
