import jdk.jfr.*;
import jdk.jfr.internal.*;
import java.util.*;
public class Sbr5JfrTypes {
    public static void main(String[] a) throws Exception {
        FlightRecorder.getFlightRecorder();
        MetadataRepository mr = MetadataRepository.getInstance();
        List<Type> types = new ArrayList<>(mr.getVisibleTypes());
        Collections.sort(types);
        for (Type t : types) {
            StringBuilder sb = new StringBuilder();
            sb.append(t.getId()).append(' ').append(t.getName()).append(" super=").append(t.getSuperType()).append(" simple=").append(t.isSimpleType());
            System.out.println(sb);
            for (ValueDescriptor v : t.getFields()) {
                System.out.println("    " + v.getName() + " : " + v.getTypeName() + " tid=" + v.getTypeId() + (v.isArray()?" []":"") + (PrivateAccess.getInstance().isConstantPool(v)?" CP":""));
            }
        }
    }
}
