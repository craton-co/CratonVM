/* SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 *
 * embed_helpers.c — acceptance harness for the optional C convenience layer
 * (cratonvm_helpers.h: the varargs-like invoke macros + value constructors) and
 * the descriptor-disambiguated field resolver (cratonvm_field_index_desc).
 *
 * Unlike embed_smoke.c / embed_flat.c (which re-declare the ABI to build
 * standalone), this harness deliberately #includes the PUBLIC headers
 * cratonvm.h + cratonvm_helpers.h, so it doubles as proof those headers compile
 * cleanly from C and match the shipped ABI.
 *
 * Not compiled by cargo. Build (from repo root after `cargo build -p libcratonvm`):
 *
 *   Windows (MSVC):
 *     cl /nologo /I libcratonvm\include /Fe:embed_helpers.exe \
 *        libcratonvm\examples\embed_helpers.c target\release\libcratonvm.dll.lib
 *     copy target\release\libcratonvm.dll .
 *     embed_helpers.exe
 *
 *   Linux (shared):
 *     cc -I libcratonvm/include libcratonvm/examples/embed_helpers.c \
 *        -L target/release -llibcratonvm -ldl -lpthread -o embed_helpers
 *     LD_LIBRARY_PATH=target/release ./embed_helpers
 *
 * The reproducible build+run wrapper is scripts/build-libcratonvm.ps1.
 */

#include "cratonvm.h"
#include "cratonvm_helpers.h"

#include <stdio.h>
#include <string.h>

int main(void) {
    CratonVm *vm = cratonvm_create(NULL);
    if (!vm) {
        fprintf(stderr, "cratonvm_create failed\n");
        return 1;
    }

    /* 1. Static call via the varargs-like macro + value constructor:
     *      Integer.toString(1234) -> "1234". */
    CratonValue rv = cratonvm_invoke_static_v(
        vm, "java/lang/Integer", "toString", "(I)Ljava/lang/String;",
        cratonvm_val_int(1234));
    if (cratonvm_is_error(rv)) {
        fprintf(stderr, "Integer.toString failed: %s\n", cratonvm_last_error(vm));
        return 1;
    }
    char *s1 = cratonvm_string_utf8(vm, cratonvm_as_object(rv));
    if (!s1 || strcmp(s1, "1234") != 0) {
        fprintf(stderr, "Integer.toString(1234) = %s (expected 1234)\n", s1 ? s1 : "(null)");
        return 1;
    }
    printf("invoke_static_v Integer.toString(1234) = \"%s\"\n", s1);
    cratonvm_free_string(s1);

    /* 2. Virtual call via the varargs-like macro:
     *      "embed".substring(2) -> "bed". */
    CratonRef s = cratonvm_new_string(vm, "embed");
    if (s == 0) {
        fprintf(stderr, "new_string failed: %s\n", cratonvm_last_error(vm));
        return 1;
    }
    CratonValue sub = cratonvm_invoke_virtual_v(
        vm, s, "substring", "(I)Ljava/lang/String;", cratonvm_val_int(2));
    if (cratonvm_is_error(sub)) {
        fprintf(stderr, "String.substring failed: %s\n", cratonvm_last_error(vm));
        return 1;
    }
    char *s2 = cratonvm_string_utf8(vm, cratonvm_as_object(sub));
    if (!s2 || strcmp(s2, "bed") != 0) {
        fprintf(stderr, "\"embed\".substring(2) = %s (expected bed)\n", s2 ? s2 : "(null)");
        return 1;
    }
    printf("invoke_virtual_v \"embed\".substring(2) = \"%s\"\n", s2);
    cratonvm_free_string(s2);

    /* 3. Primitive return read via cratonvm_as_int:
     *      "embed".charAt(0) -> 'e'. */
    CratonValue ch = cratonvm_invoke_virtual_v(vm, s, "charAt", "(I)C", cratonvm_val_int(0));
    if (cratonvm_is_error(ch) || cratonvm_as_int(ch) != 'e') {
        fprintf(stderr, "\"embed\".charAt(0) = %d (expected %d)\n",
                cratonvm_as_int(ch), 'e');
        return 1;
    }
    printf("invoke_virtual_v \"embed\".charAt(0) = '%c'\n", (char)cratonvm_as_int(ch));

    /* 4. Descriptor-disambiguated field resolution on String.hash:
     *      correct descriptor "I" resolves; wrong descriptor "J" does not;
     *      null descriptor is name-only (same slot). */
    CratonClass scls = 0;
    if (cratonvm_object_class(vm, s, &scls) != 0) {
        fprintf(stderr, "object_class failed: %s\n", cratonvm_last_error(vm));
        return 1;
    }
    cratonvm_jint idx_i = -1, idx_n = -1, idx_j = -1;
    if (cratonvm_field_index_desc(vm, scls, "hash", "I", &idx_i) != 0) {
        fprintf(stderr, "field_index_desc(hash, I) failed: %s\n", cratonvm_last_error(vm));
        return 1;
    }
    if (cratonvm_field_index_desc(vm, scls, "hash", NULL, &idx_n) != 0 || idx_n != idx_i) {
        fprintf(stderr, "field_index_desc(hash, NULL) mismatch (%d vs %d)\n", idx_n, idx_i);
        return 1;
    }
    if (cratonvm_field_index_desc(vm, scls, "hash", "J", &idx_j) == 0) {
        fprintf(stderr, "field_index_desc(hash, J) unexpectedly matched a long field\n");
        return 1;
    }
    printf("field_index_desc String.hash: I->slot %d, NULL->slot %d, J->no-match (ok)\n",
           idx_i, idx_n);

    if (cratonvm_release_ref(vm, cratonvm_as_object(rv)) != 0 ||
        cratonvm_release_ref(vm, cratonvm_as_object(sub)) != 0 ||
        cratonvm_release_ref(vm, s) != 0) {
        fprintf(stderr, "release_ref failed: %s\n", cratonvm_last_error(vm));
        return 1;
    }

    cratonvm_destroy(vm);
    printf("embed_helpers: OK\n");
    return 0;
}
