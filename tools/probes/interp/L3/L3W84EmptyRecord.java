/*
 * Interpreter round i4, wave 84, lane L3: a record with NO component.
 *
 * javac gives `record Empty() {}` a `Record` attribute with
 * components_count 0. HotSpot's InstanceKlass::is_record is
 * `_record_components != nullptr && is_final() && super == Record` -- the
 * zero-length array counts -- so Class.isRecord() is true and
 * getRecordComponents() is an empty array. CratonVM's `Class::is_record`
 * read "has components", so the class read as a non-record: isRecord()
 * false, getRecordComponents() null, and serialization treated it as an
 * ordinary Serializable class (no canonical-constructor call on read, so the
 * compact constructor's side effect below did not run again).
 *
 * Run (both modes must print the HotSpot lines):
 *   javac -d out L3W84EmptyRecord.java
 *   cratonvm --jdk-only   -cp out L3W84EmptyRecord
 *   cratonvm --compatible -cp out L3W84EmptyRecord
 *   java                  -cp out L3W84EmptyRecord   (HotSpot 25)
 *
 * Expected HotSpot 25 output:
 *   empty: isRecord=true components=0
 *   one: isRecord=true components=1 x:int
 *   plain: isRecord=false components=null
 *   round-trip: Empty[] constructed=2 equal=true
 */
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.io.Serializable;
import java.lang.reflect.RecordComponent;

public class L3W84EmptyRecord {
    static int constructed;

    record Empty() implements Serializable {
        Empty {
            constructed++;
        }
    }

    record One(int x) {}

    static final class Plain {}

    static String describe(Class<?> c) {
        RecordComponent[] rc = c.getRecordComponents();
        StringBuilder sb = new StringBuilder();
        sb.append("isRecord=").append(c.isRecord());
        sb.append(" components=").append(rc == null ? "null" : String.valueOf(rc.length));
        if (rc != null) {
            for (RecordComponent r : rc) {
                sb.append(' ').append(r.getName()).append(':').append(r.getType().getName());
            }
        }
        return sb.toString();
    }

    public static void main(String[] args) throws Exception {
        System.out.println("empty: " + describe(Empty.class));
        System.out.println("one: " + describe(One.class));
        System.out.println("plain: " + describe(Plain.class));

        Empty e = new Empty();
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ObjectOutputStream out = new ObjectOutputStream(bytes)) {
            out.writeObject(e);
        }
        Object back;
        try (ObjectInputStream in = new ObjectInputStream(new ByteArrayInputStream(bytes.toByteArray()))) {
            back = in.readObject();
        }
        System.out.println("round-trip: " + back + " constructed=" + constructed + " equal=" + e.equals(back));
    }
}
