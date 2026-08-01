#include <openssl/crypto.h>
#include <openssl/evp.h>
#include <stdlib.h>
#include <string.h>

#if defined(_MSC_VER)
#define CSGDB_INTERNAL
#else
#define CSGDB_INTERNAL __attribute__((visibility("hidden")))
#endif

static CRYPTO_ONCE csgdb_hmac_once = CRYPTO_ONCE_STATIC_INIT;
static EVP_MAC *csgdb_hmac_provider = NULL;

static void csgdb_release_hmac_provider(void) {
    EVP_MAC_free(csgdb_hmac_provider);
    csgdb_hmac_provider = NULL;
}

static void csgdb_initialize_hmac_provider(void) {
    csgdb_hmac_provider = EVP_MAC_fetch(NULL, "HMAC", NULL);
    if (csgdb_hmac_provider != NULL) {
        (void)atexit(csgdb_release_hmac_provider);
    }
}

/*
 * The bundled page codec requests the same immutable OpenSSL HMAC provider
 * for every authenticated page. Provider discovery is process-global and
 * thread-safe, so cache the fetched provider and return a balanced reference
 * to each caller. Mutable operation contexts remain per-page and are released
 * immediately, so key-dependent state does not outlive the codec operation.
 */
CSGDB_INTERNAL EVP_MAC *_csgdb_internal_evp_mac_fetch(
    OSSL_LIB_CTX *library_context,
    const char *algorithm,
    const char *properties
) {
    if (library_context != NULL || properties != NULL || algorithm == NULL ||
        strcmp(algorithm, "HMAC") != 0) {
        return EVP_MAC_fetch(library_context, algorithm, properties);
    }
    if (!CRYPTO_THREAD_run_once(&csgdb_hmac_once, csgdb_initialize_hmac_provider) ||
        csgdb_hmac_provider == NULL || EVP_MAC_up_ref(csgdb_hmac_provider) != 1) {
        return NULL;
    }
    return csgdb_hmac_provider;
}
