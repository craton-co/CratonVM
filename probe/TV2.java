import java.util.*;
public class TV2 {
    public static void main(String[] a){
        try {
            TreeMap<String,Integer> m=new TreeMap<>(); m.put("a",1); m.put("b",2); m.put("c",3);
            Iterator<Integer> v=m.values().iterator();
            while(v.hasNext()){ Integer x=v.next(); if(x.intValue()==2) v.remove(); }
            System.out.println("values.itr.remove OK size="+m.size()+" hasVal2="+m.containsValue(2));
        } catch(Throwable e){ System.out.println("values EXC: "+e); e.printStackTrace(); }
    }
}
