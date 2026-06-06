# keycloak-server-spi CredentialModelTest — Jackson "no Creators" for PasswordCredentialData

## Symptom
Module `apps/keycloak/server-spi` (CratonVM 8 fail vs HotSpot 0). Four of them:
```
FAIL canCreatedExtendedCredentialModel(org.keycloak.models.credential.CredentialModelTest) :: (no message)
FAIL canDeserializeMinimalJson(...CredentialModelTest)
     :: com.fasterxml.jackson.databind.exc.InvalidDefinitionException: Cannot construct instance of
        `org.keycloak.models.credential.dto.PasswordCredentialData` (no Creators, like default constructor,
        exist): cannot deserialize from Object value (no delegate- or property-based Creator)
FAIL roudtripToJsonExtendedCredentialModel(...) :: same "no Creators" InvalidDefinitionException
FAIL roundtripToJsonDefaultCredentialModel(...) :: same
```

HotSpot deserializes the same JSON with the same Jackson + classpath fine.

## Why it's a CratonVM bug
Jackson cannot find a usable constructor/creator for
`org.keycloak.models.credential.dto.PasswordCredentialData`. The class **does**
have a Jackson-annotated constructor (`@JsonCreator` with `@JsonProperty`
params) — HotSpot sees it. On CratonVM, Jackson's reflective introspection
(`Class.getDeclaredConstructors()` + parameter annotations / parameter names)
isn't surfacing it, so Jackson concludes "no Creators." This points at a
**reflection gap**: either `getDeclaredConstructors()` omits the constructor, or
constructor **parameter annotations** (`Constructor.getParameterAnnotations()`)
or **parameter names** (`Parameter.getName()` / `-parameters`) aren't returned,
so Jackson can't bind properties.

## Reproduce
```
CP="apps/_test-harness;apps/keycloak/server-spi/target/classes;apps/keycloak/server-spi/target/test-classes;$(cat apps/keycloak/server-spi/cp.txt)"
target/release/java.exe --java-home "C:/Program Files/Java/jdk-25" -Xmx2g -cp "$CP" \
    org.junit.runner.JUnitCore org.keycloak.models.credential.CredentialModelTest
```

## What an agent should try next
1. Probe reflection on `PasswordCredentialData` under CratonVM:
   - `getDeclaredConstructors()` — is the `@JsonCreator` ctor present?
   - on that ctor: `getParameterAnnotations()` (expect `@JsonProperty` on each),
     `getParameters()[i].getAnnotations()`, `getParameters()[i].isNamePresent()`.
2. Whichever returns empty/wrong is the bug. Constructor **parameter
   annotations** are the most likely gap (Jackson uses them to map JSON props).
3. Compare the byte-level RuntimeVisibleParameterAnnotations attribute parsing
   for constructors vs methods in the class reader.

## Note
`canCreatedExtendedCredentialModel` has an empty failure message — capture its
stack separately (run that single method) to confirm it's the same root or a
distinct constructor-reflection path.
