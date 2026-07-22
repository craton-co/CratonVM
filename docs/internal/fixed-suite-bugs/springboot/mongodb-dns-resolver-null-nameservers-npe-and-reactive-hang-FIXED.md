# MongoDB DNS resolver and reactive auto-configuration hang - FIXED

**Resolved: 2026-07-18**

## Root causes and fixes

- Real-JDK `ResolverConfigurationImpl.stringToList` dereferenced the native
  resolver's null `os_searchlist` and `os_nameservers` fields. The resolver
  natives now initialize both to non-null empty strings and provide the
  platform ephemeral-port range.
- JNDI DNS uses a non-blocking `DatagramChannel`. CratonVM now supplies both
  channel factories, connected UDP read/write and wildcard bind behavior,
  selector registration/readiness, and a source `InetSocketAddress` whose
  real-JDK layout makes DNS reply-address equality work.
- `configuresSslWithBundle` exposed a separate lifecycle residual. Spring
  Boot's Mongo reactive customizer awaited a Netty termination promise that
  could remain incomplete after TLS bootstrap. Its automatic Mongo-only event
  loop now uses a daemon thread factory; its destroy hook requests the usual
  zero-quiet-period shutdown without awaiting that stale promise. User-supplied
  Mongo transport settings retain Spring's original path.

## Validation

Azure JDK 25 fixture, final isolated `r26` binary:

| Class or probe | JIT | `--nojit` |
|---|---:|---:|
| `MongoAutoConfigurationTests` | 23/23 | 23/23 |
| `PropertiesMongoConnectionDetailsTests` | 15/15 | 15/15 |
| `MongoReactiveAutoConfigurationTests` | 20/20 | 20/20 |
| Connected UDP DNS probe | `WROTE=27`, `READ=27` | `WROTE=27`, `READ=27` |

The SSL-bundle method was also run independently in both modes and completed
with no remaining non-daemon event-loop hang.
