import java.util.*;

public class TreeView {
    static int pass=0, fail=0;
    static void chk(String n, boolean c){ if(c) pass++; else { fail++; System.out.println("FAIL: "+n); } }
    static TreeMap<String,Integer> mk(){ TreeMap<String,Integer> m=new TreeMap<>(); m.put("a",1); m.put("b",2); m.put("c",3); return m; }

    public static void main(String[] a){
        try {
            TreeMap<String,Integer> m=mk();
            Iterator<Map.Entry<String,Integer>> e=m.entrySet().iterator();
            while(e.hasNext()){ if(e.next().getKey().equals("b")) e.remove(); }
            chk("entrySet.itr.remove", m.size()==2 && !m.containsKey("b"));
        } catch(Throwable t){ fail++; System.out.println("EXC entrySet.itr.remove: "+t.getClass().getName()+":"+t.getMessage()); }

        try {
            TreeMap<String,Integer> m=mk();
            Iterator<Integer> v=m.values().iterator();
            while(v.hasNext()){ if(v.next().intValue()==2) v.remove(); }
            chk("values.itr.remove", m.size()==2 && !m.containsValue(2));
        } catch(Throwable t){ fail++; System.out.println("EXC values.itr.remove: "+t.getClass().getName()+":"+t.getMessage()); }

        try {
            TreeMap<String,Integer> m=mk();
            m.entrySet().removeIf(new java.util.function.Predicate<Map.Entry<String,Integer>>(){
                public boolean test(Map.Entry<String,Integer> en){ return en.getValue()==3; }});
            chk("entrySet.removeIf", m.size()==2 && !m.containsKey("c"));
        } catch(Throwable t){ fail++; System.out.println("EXC entrySet.removeIf: "+t.getClass().getName()+":"+t.getMessage()); }

        try {
            TreeMap<String,Integer> m=mk();
            m.keySet().remove("c");
            chk("keySet.remove", m.size()==2 && !m.containsKey("c"));
        } catch(Throwable t){ fail++; System.out.println("EXC keySet.remove: "+t.getClass().getName()+":"+t.getMessage()); }

        try {
            TreeMap<String,Integer> m=mk();
            Iterator<String> k=m.keySet().iterator();
            while(k.hasNext()){ if(k.next().equals("a")) k.remove(); }
            chk("keySet.itr.remove", m.size()==2 && !m.containsKey("a"));
        } catch(Throwable t){ fail++; System.out.println("EXC keySet.itr.remove: "+t.getClass().getName()+":"+t.getMessage()); }

        try {
            TreeMap<String,Integer> m=new TreeMap<>(); m.put("c",3); m.put("a",1); m.put("b",2);
            StringBuilder sb=new StringBuilder(); for(String s: m.keySet()) sb.append(s);
            chk("order.keySet", sb.toString().equals("abc"));
            StringBuilder sb2=new StringBuilder(); for(Integer v: m.values()) sb2.append(v);
            chk("order.values", sb2.toString().equals("123"));
        } catch(Throwable t){ fail++; System.out.println("EXC order: "+t.getClass().getName()+":"+t.getMessage()); }

        System.out.println("PASS="+pass+" FAIL="+fail);
    }
}
