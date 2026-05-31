import java.util.*;
public class TsOnly {
    public static void main(String[] a){
        TreeSet<String> s = new TreeSet<>(Arrays.asList("a","b","c"));
        Iterator<String> it = s.iterator();
        while(it.hasNext()){ if(it.next().equals("b")) it.remove(); }
        System.out.println("TreeSet itr.remove size="+s.size()+" contains(b)="+s.contains("b"));
    }
}
