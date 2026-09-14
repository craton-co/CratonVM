import java.util.*; import java.util.concurrent.*;
public class ChmOrderConfoundDoorOrder {
    public static void main(String[] a){
        ConcurrentHashMap<String,String> m=new ConcurrentHashMap<>();
        m.putIfAbsent("only","x");
        Map<String,String> asMap=m;
        if(a.length>0&&a[0].equals("mapfirst")){
            System.out.println("[1st] via Map  containsKey="+asMap.containsKey("only")+" keySet="+asMap.keySet());
            System.out.println("[2nd] via CHM  containsKey="+m.containsKey("only")+" keySet="+m.keySet());
        } else {
            System.out.println("[1st] via CHM  containsKey="+m.containsKey("only")+" keySet="+m.keySet());
            System.out.println("[2nd] via Map  containsKey="+asMap.containsKey("only")+" keySet="+asMap.keySet());
        }
    }
}
