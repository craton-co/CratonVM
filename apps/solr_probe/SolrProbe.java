import org.apache.solr.common.SolrInputDocument;
import org.apache.solr.client.solrj.beans.DocumentObjectBinder;
import java.util.Collection;
public class SolrProbe {
    public static void main(String[] args) throws Exception {
        // Build a SolrInputDocument — exercises Solr's client-side
        // document construction + field iteration.
        SolrInputDocument doc = new SolrInputDocument();
        doc.addField("id", "probe-1");
        doc.addField("title", "hello solr");
        doc.addField("count", 42);
        if (!doc.containsKey("id")) { System.out.println("FAIL: doc missing id"); System.exit(1); }
        if (!"probe-1".equals(doc.getFieldValue("id"))) {
            System.out.println("FAIL: id mismatch"); System.exit(1);
        }
        Collection<String> names = doc.getFieldNames();
        if (names.size() != 3) {
            System.out.println("FAIL: expected 3 fields, got " + names.size()); System.exit(1);
        }
        System.out.println("SolrInputDocument fields: " + names);

        // Exercise the bean binder reflection.
        DocumentObjectBinder binder = new DocumentObjectBinder();
        System.out.println("Binder: " + binder);

        System.out.println("OK");
        System.exit(0);
    }
}
