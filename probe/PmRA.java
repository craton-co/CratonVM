import org.apache.catalina.util.ParameterMap;
public class PmRA {
    public static void main(String[] a){
        ParameterMap<String,String[]> pm = new ParameterMap<>();
        pm.put("param2", new String[]{"2"});
        pm.setLocked(true);
        try { pm.replaceAll((x,y)->new String[]{"z"}); System.out.println("replaceAll NO THROW"); }
        catch(Throwable t){ System.out.println("replaceAll threw "+t.getClass().getName()); }
    }
}
