import java.util.ArrayList;
// V2: identical to SpliceCastProbe except the variable is ArrayList, not List,
// so `get` is invokevirtual on a final-ish concrete type instead of
// invokeinterface. Isolates interface dispatch from everything else.
public class V2ArrayListTyped {
    static final class Box { final int v; Box(int v){this.v=v;} int value(){return v;} }
    static Box asBox(Object o){ return (Box) o; }
    static int kindOf(Object o){ return o instanceof Box ? 1 : 0; }
    static int step(ArrayList<Object> xs, int acc, int i){
        Object o = xs.get(i & 15);
        return acc * 31 + asBox(o).value() + kindOf(o);
    }
    public static void main(String[] a){
        int reps = a.length>0?Integer.parseInt(a[0]):4_000_000;
        ArrayList<Object> xs = new ArrayList<>();
        for (int i=0;i<16;i++) xs.add(new Box(i*7+1));
        int warm=0; for(int i=0;i<3_000_000;i++) warm=step(xs,warm,i);
        long t0=System.nanoTime(); int acc=0;
        for(int i=0;i<reps;i++) acc=step(xs,acc,i);
        long ms=(System.nanoTime()-t0)/1_000_000L;
        System.out.println("V2 arraylist-typed ("+reps+") : "+ms+" ms  ["+acc+"]");
        if(warm==0x7FFFFFFF) System.out.println(warm);
    }
}
