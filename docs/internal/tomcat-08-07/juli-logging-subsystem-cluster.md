# JULI logging subsystem cluster

**Status: resolved and archived on 2026-07-15.**

## Final resolution

The remaining `TestPerWebappJuliIntegration` residual was a combination of
three real-JDK JUL bridge defects:

- JULI loggers were cached globally by name rather than by the thread context
  class loader, so independent webapps could not retain distinct root levels
  and handlers.
- The inherited `java.util.logging.Handler` no-op native suppressed concrete
  `org.apache.juli.FileHandler` publication and formatting. The bridge now
  leaves concrete handler bytecode intact and retains level/formatter state.
- The Formatter bridge treated the real `LogRecord` sequence-number slot as a
  message. It now uses `LogRecord.getMessage()`, so `OneLineFormatter` receives
  the materialized message and `FileHandler` writes it.

The JUL bridge now scopes Tomcat logger and root-handler state by context class
loader, roots and relocates that state safely, and fans records out through the
configured per-webapp handler list.

## Verification

Fresh Windows real-JDK 25 CratonVM release run, using the uniquely named
`cratonvm-tomcat-juli-residual-closure-20260715-014.exe`:

```text
org.apache.juli.TestPerWebappJuliIntegration
OK (2 tests)

org.apache.juli.TestAsyncFileHandlerOverflow
org.apache.juli.TestFileHandler
org.apache.juli.TestPerWebappJuliIntegration
org.apache.juli.TestThreadNameCache
OK (10 tests)
```

This document is archived because the complete original four-class cluster and
the reopened per-webapp handler-isolation residual now pass.
