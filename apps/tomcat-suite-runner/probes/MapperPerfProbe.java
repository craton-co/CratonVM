import java.util.Arrays;

import org.apache.catalina.Context;
import org.apache.catalina.Host;
import org.apache.catalina.Wrapper;
import org.apache.catalina.core.StandardContext;
import org.apache.catalina.core.StandardHost;
import org.apache.catalina.core.StandardWrapper;
import org.apache.catalina.mapper.Mapper;
import org.apache.catalina.mapper.MappingData;
import org.apache.catalina.mapper.WrapperMappingInfo;
import org.apache.tomcat.util.buf.MessageBytes;

/**
 * Standalone form of org.apache.catalina.mapper.TestMapperPerformance, so the
 * 10^6-call loop of known-issue tomcat/32.1 can be iterated on without paying
 * for the seven inherited TestMapper cases (and their JUnit machinery) first.
 *
 * Setup is copied verbatim from TestMapper.setUp so the mapper's shape -- which
 * hostname resolves to a context, and how many wrappers hang off it -- matches
 * what the real test measures.
 *
 * Usage: MapperPerfProbe [hostname] [iterations] [rounds] [mode]
 *
 * mode is one of:
 *   full     recycle() + map(), exactly what the test times (default)
 *   recycle  MappingData.recycle() alone -- the loop's other half, which is
 *            ordinary Tomcat bytecode rather than a native shadow
 *   tochars  MessageBytes.toChars() alone, the shadow map() calls twice per
 *            iteration before it does any mapping work
 * Splitting them is how 32.1's per-call cost gets attributed to a side of the
 * native boundary instead of guessed at.
 */
public class MapperPerfProbe {

    private static Mapper mapper;

    public static void main(String[] args) throws Exception {
        String hostName = args.length > 0 ? args[0] : "xxxxxxxxxxx";
        int iters = args.length > 1 ? Integer.parseInt(args[1]) : 1000000;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 3;
        String mode = args.length > 3 ? args[3] : "full";

        setUp();
        for (int r = 0; r < rounds; r++) {
            long ms = timeOne(hostName, iters, mode);
            System.out.println("host=" + hostName + " mode=" + mode + " iters=" + iters + " round=" + r
                    + " took=" + ms + "ms  per-call=" + (ms * 1000.0 / iters) + "us");
        }
    }

    private static long timeOne(String requestedHostName, int iters, String mode) throws Exception {
        MappingData mappingData = new MappingData();
        MessageBytes host = MessageBytes.newInstance();
        host.setString(requestedHostName);
        MessageBytes uri = MessageBytes.newInstance();
        uri.setString("/foo/bar/blah/bobou/foo");
        uri.toChars();
        uri.getCharChunk().setLimit(-1);

        long start = System.currentTimeMillis();
        switch (mode) {
            case "recycle":
                for (int i = 0; i < iters; i++) {
                    mappingData.recycle();
                }
                break;
            case "tochars":
                for (int i = 0; i < iters; i++) {
                    host.toChars();
                    uri.toChars();
                }
                break;
            default:
                for (int i = 0; i < iters; i++) {
                    mappingData.recycle();
                    mapper.map(host, uri, null, mappingData);
                }
                break;
        }
        return System.currentTimeMillis() - start;
    }

    private static Host createHost(String name) {
        Host host = new StandardHost();
        host.setName(name);
        return host;
    }

    private static Context createContext(String name) {
        Context context = new StandardContext();
        context.setName(name);
        return context;
    }

    private static Wrapper createWrapper(String name) {
        Wrapper wrapper = new StandardWrapper();
        wrapper.setName(name);
        return wrapper;
    }

    private static void setUp() throws Exception {
        mapper = new Mapper();

        mapper.addHost("sjbjdvwsbvhrb", new String[0], createHost("blah1"));
        mapper.addHost("sjbjdvwsbvhr/", new String[0], createHost("blah1"));
        mapper.addHost("wekhfewuifweuibf", new String[0], createHost("blah2"));
        mapper.addHost("ylwrehirkuewh", new String[0], createHost("blah3"));
        mapper.addHost("iohgeoihro", new String[0], createHost("blah4"));
        mapper.addHost("fwehoihoihwfeo", new String[0], createHost("blah5"));
        mapper.addHost("owefojiwefoi", new String[0], createHost("blah6"));
        mapper.addHost("iowejoiejfoiew", new String[0], createHost("blah7"));
        mapper.addHost("ohewoihfewoih", new String[0], createHost("blah8"));
        mapper.addHost("fewohfoweoih", new String[0], createHost("blah9"));
        mapper.addHost("ttthtiuhwoih", new String[0], createHost("blah10"));
        mapper.addHost("lkwefjwojweffewoih", new String[0], createHost("blah11"));
        mapper.addHost("zzzuyopjvewpovewjhfewoih", new String[0], createHost("blah12"));
        mapper.addHost("xxxxgqwiwoih", new String[0], createHost("blah13"));
        mapper.addHost("qwigqwiwoih", new String[0], createHost("blah14"));
        mapper.addHost("qwerty.net", new String[0], createHost("blah15"));
        mapper.addHost("*.net", new String[0], createHost("blah16"));
        mapper.addHost("zzz.com", new String[0], createHost("blah17"));
        mapper.addHostAlias("iowejoiejfoiew", "iowejoiejfoiew_alias");

        mapper.setDefaultHostName("ylwrehirkuewh");

        String[] welcomes = new String[2];
        welcomes[0] = "boo/baba";
        welcomes[1] = "bobou";

        Host host = createHost("blah7");
        mapper.addContextVersion("iowejoiejfoiew", host, "", "0", createContext("context0"), new String[0], null, null);
        mapper.addContextVersion("iowejoiejfoiew", host, "/foo", "0", createContext("context1"), new String[0], null,
                null);
        mapper.addContextVersion("iowejoiejfoiew", host, "/foo/bar", "0", createContext("context2"), welcomes, null,
                null);

        mapper.addWrappers("iowejoiejfoiew", "/foo", "0", Arrays.asList(new WrapperMappingInfo[] {
                new WrapperMappingInfo("/", createWrapper("context1-defaultWrapper"), false, false) }));
        mapper.addWrappers("iowejoiejfoiew", "/foo/bar", "0",
                Arrays.asList(new WrapperMappingInfo[] {
                        new WrapperMappingInfo("/fo/*", createWrapper("wrapper0"), false, false),
                        new WrapperMappingInfo("/", createWrapper("wrapper1"), false, false),
                        new WrapperMappingInfo("/blh", createWrapper("wrapper2"), false, false),
                        new WrapperMappingInfo("*.jsp", createWrapper("wrapper3"), false, false),
                        new WrapperMappingInfo("/blah/bou/*", createWrapper("wrapper4"), false, false),
                        new WrapperMappingInfo("/blah/bobou/*", createWrapper("wrapper5"), false, false),
                        new WrapperMappingInfo("*.htm", createWrapper("wrapper6"), false, false) }));

        mapper.addContextVersion("iowejoiejfoiew", host, "/foo/bar/bla", "0", createContext("context3"), new String[0],
                null, Arrays.asList(new WrapperMappingInfo[] {
                        new WrapperMappingInfo("/bobou/*", createWrapper("wrapper7"), false, false) }));

        host = createHost("blah16");
        mapper.addContextVersion("*.net", host, "", "0", createContext("context4"), new String[0], null, null);
        mapper.addWrappers("*.net", "", "0", Arrays.asList(new WrapperMappingInfo[] {
                new WrapperMappingInfo("/", createWrapper("context4-defaultWrapper"), false, false) }));
    }
}
