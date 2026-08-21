import java.util.*; import java.util.concurrent.*;
public class ChmOrderConfoundKeyOrder {
    static void str(String tag){ ConcurrentHashMap<String,String> m=new ConcurrentHashMap<>();
        for(int i=0;i<4;i++) m.put("k"+i,"v"+i);
        System.out.println(tag+" String keys  size="+m.size()); }
    static void intk(String tag){ ConcurrentHashMap<Integer,String> m=new ConcurrentHashMap<>();
        for(int i=0;i<4;i++) m.put(i,"v"+i);
        System.out.println(tag+" Integer keys size="+m.size()); }
    public static void main(String[] a){
        if(a.length>0&&a[0].equals("intfirst")){ intk("[1st]"); str("[2nd]"); }
        else { str("[1st]"); intk("[2nd]"); }
    }
}
