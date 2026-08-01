#define OPENSSL_SUPPRESS_DEPRECATED

#include <openssl/core.h>
#include <openssl/crypto.h>
#include <openssl/evp.h>
#include <openssl/params.h>
#include <openssl/sha.h>
#include <stdlib.h>
#include <string.h>

#if defined(_MSC_VER)
#define CSGDB_INTERNAL
#else
#define CSGDB_INTERNAL __attribute__((visibility("hidden")))
#endif

static CRYPTO_ONCE csgdb_hmac_once = CRYPTO_ONCE_STATIC_INIT;
static CRYPTO_ONCE csgdb_aes_once = CRYPTO_ONCE_STATIC_INIT;
static CRYPTO_ONCE csgdb_cleanup_once = CRYPTO_ONCE_STATIC_INIT;
static EVP_MAC *csgdb_hmac_provider = NULL;
static EVP_CIPHER *csgdb_aes_provider = NULL;

typedef union csgdb_digest_context {
    SHA_CTX sha1;
    SHA256_CTX sha256;
    SHA512_CTX sha512;
} csgdb_digest_context;

typedef struct csgdb_hmac_context {
    int algorithm;
    csgdb_digest_context inner;
    csgdb_digest_context outer;
} csgdb_hmac_context;

enum {
    CSGDB_HMAC_SHA1 = 1,
    CSGDB_HMAC_SHA256 = 2,
    CSGDB_HMAC_SHA512 = 3
};

static void csgdb_release_crypto_providers(void) {
    EVP_MAC_free(csgdb_hmac_provider);
    csgdb_hmac_provider = NULL;
    EVP_CIPHER_free(csgdb_aes_provider);
    csgdb_aes_provider = NULL;
}

static void csgdb_initialize_hmac_provider(void) {
    csgdb_hmac_provider = EVP_MAC_fetch(NULL, "HMAC", NULL);
}

static void csgdb_initialize_aes_provider(void) {
    csgdb_aes_provider = EVP_CIPHER_fetch(NULL, "AES-256-CBC", NULL);
}

static void csgdb_register_crypto_cleanup(void) {
    (void)atexit(csgdb_release_crypto_providers);
}

/*
 * The bundled page codec requests the same immutable OpenSSL HMAC provider
 * for every authenticated page. Provider discovery is process-global and
 * thread-safe, so cache one process-lifetime provider reference. A matching
 * free wrapper releases only non-cached fallback results. Mutable operation
 * contexts remain per-page, so key-dependent state does not outlive the codec
 * operation.
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
        csgdb_hmac_provider == NULL) {
        return NULL;
    }
    (void)CRYPTO_THREAD_run_once(&csgdb_cleanup_once, csgdb_register_crypto_cleanup);
    return csgdb_hmac_provider;
}

CSGDB_INTERNAL void _csgdb_internal_evp_mac_free(EVP_MAC *mac) {
    if (mac != csgdb_hmac_provider) {
        EVP_MAC_free(mac);
    }
}

static int csgdb_ascii_equal(const void *value, size_t value_size, const char *expected) {
    const unsigned char *left = value;
    const unsigned char *right = (const unsigned char *)expected;
    size_t expected_size = strlen(expected);
    size_t index;
    if (left == NULL || value_size != expected_size) {
        return 0;
    }
    for (index = 0; index < value_size; index++) {
        unsigned char character = left[index];
        if (character >= 'A' && character <= 'Z') {
            character = (unsigned char)(character + ('a' - 'A'));
        }
        if (character != right[index]) {
            return 0;
        }
    }
    return 1;
}

static int csgdb_hmac_algorithm(const OSSL_PARAM parameters[]) {
    const OSSL_PARAM *parameter = parameters;
    while (parameter != NULL && parameter->key != NULL) {
        if (strcmp(parameter->key, "digest") == 0) {
            if (csgdb_ascii_equal(parameter->data, parameter->data_size, "sha1")) {
                return CSGDB_HMAC_SHA1;
            }
            if (csgdb_ascii_equal(parameter->data, parameter->data_size, "sha256")) {
                return CSGDB_HMAC_SHA256;
            }
            if (csgdb_ascii_equal(parameter->data, parameter->data_size, "sha512")) {
                return CSGDB_HMAC_SHA512;
            }
            return 0;
        }
        parameter++;
    }
    return 0;
}

static size_t csgdb_hmac_block_size(int algorithm) {
    return algorithm == CSGDB_HMAC_SHA512 ? SHA512_CBLOCK : SHA256_CBLOCK;
}

static size_t csgdb_hmac_output_size(int algorithm) {
    switch (algorithm) {
        case CSGDB_HMAC_SHA1:
            return SHA_DIGEST_LENGTH;
        case CSGDB_HMAC_SHA256:
            return SHA256_DIGEST_LENGTH;
        case CSGDB_HMAC_SHA512:
            return SHA512_DIGEST_LENGTH;
        default:
            return 0;
    }
}

static int csgdb_digest_init(int algorithm, csgdb_digest_context *context) {
    switch (algorithm) {
        case CSGDB_HMAC_SHA1:
            return SHA1_Init(&context->sha1);
        case CSGDB_HMAC_SHA256:
            return SHA256_Init(&context->sha256);
        case CSGDB_HMAC_SHA512:
            return SHA512_Init(&context->sha512);
        default:
            return 0;
    }
}

static int csgdb_digest_update(
    int algorithm,
    csgdb_digest_context *context,
    const void *input,
    size_t input_size
) {
    switch (algorithm) {
        case CSGDB_HMAC_SHA1:
            return SHA1_Update(&context->sha1, input, input_size);
        case CSGDB_HMAC_SHA256:
            return SHA256_Update(&context->sha256, input, input_size);
        case CSGDB_HMAC_SHA512:
            return SHA512_Update(&context->sha512, input, input_size);
        default:
            return 0;
    }
}

static int csgdb_digest_final(
    int algorithm,
    csgdb_digest_context *context,
    unsigned char *output
) {
    switch (algorithm) {
        case CSGDB_HMAC_SHA1:
            return SHA1_Final(output, &context->sha1);
        case CSGDB_HMAC_SHA256:
            return SHA256_Final(output, &context->sha256);
        case CSGDB_HMAC_SHA512:
            return SHA512_Final(output, &context->sha512);
        default:
            return 0;
    }
}

/*
 * SQLCipher feeds HMAC one page at a time through the generic OpenSSL 3 MAC
 * dispatcher. Its algorithm and key size are already fixed by the codec, so
 * keep an operation-local HMAC state and call OpenSSL's hardware-accelerated
 * SHA primitives directly. This removes per-page provider parameter parsing
 * and provider locks without changing the HMAC construction or tag bytes.
 */
CSGDB_INTERNAL EVP_MAC_CTX *_csgdb_internal_evp_mac_ctx_new(EVP_MAC *mac) {
    csgdb_hmac_context *context;
    (void)mac;
    context = OPENSSL_zalloc(sizeof(*context));
    return (EVP_MAC_CTX *)context;
}

CSGDB_INTERNAL void _csgdb_internal_evp_mac_ctx_free(EVP_MAC_CTX *raw_context) {
    csgdb_hmac_context *context = (csgdb_hmac_context *)raw_context;
    if (context != NULL) {
        OPENSSL_cleanse(context, sizeof(*context));
        OPENSSL_free(context);
    }
}

CSGDB_INTERNAL int _csgdb_internal_evp_mac_init(
    EVP_MAC_CTX *raw_context,
    const unsigned char *key,
    size_t key_size,
    const OSSL_PARAM parameters[]
) {
    csgdb_hmac_context *context = (csgdb_hmac_context *)raw_context;
    unsigned char key_digest[SHA512_DIGEST_LENGTH];
    unsigned char inner_pad[SHA512_CBLOCK];
    unsigned char outer_pad[SHA512_CBLOCK];
    const unsigned char *effective_key = key;
    size_t effective_key_size = key_size;
    size_t block_size;
    size_t index;
    int result = 0;

    if (context == NULL || key == NULL) {
        return 0;
    }
    OPENSSL_cleanse(context, sizeof(*context));
    context->algorithm = csgdb_hmac_algorithm(parameters);
    block_size = csgdb_hmac_block_size(context->algorithm);
    if (context->algorithm == 0 || block_size > sizeof(inner_pad)) {
        goto cleanup;
    }
    if (effective_key_size > block_size) {
        csgdb_digest_context key_context;
        if (!csgdb_digest_init(context->algorithm, &key_context) ||
            !csgdb_digest_update(context->algorithm, &key_context, key, key_size) ||
            !csgdb_digest_final(context->algorithm, &key_context, key_digest)) {
            OPENSSL_cleanse(&key_context, sizeof(key_context));
            goto cleanup;
        }
        OPENSSL_cleanse(&key_context, sizeof(key_context));
        effective_key = key_digest;
        effective_key_size = csgdb_hmac_output_size(context->algorithm);
    }
    memset(inner_pad, 0x36, block_size);
    memset(outer_pad, 0x5c, block_size);
    for (index = 0; index < effective_key_size; index++) {
        inner_pad[index] ^= effective_key[index];
        outer_pad[index] ^= effective_key[index];
    }
    if (!csgdb_digest_init(context->algorithm, &context->inner) ||
        !csgdb_digest_update(context->algorithm, &context->inner, inner_pad, block_size) ||
        !csgdb_digest_init(context->algorithm, &context->outer) ||
        !csgdb_digest_update(context->algorithm, &context->outer, outer_pad, block_size)) {
        goto cleanup;
    }
    result = 1;

cleanup:
    OPENSSL_cleanse(key_digest, sizeof(key_digest));
    OPENSSL_cleanse(inner_pad, sizeof(inner_pad));
    OPENSSL_cleanse(outer_pad, sizeof(outer_pad));
    return result;
}

CSGDB_INTERNAL int _csgdb_internal_evp_mac_update(
    EVP_MAC_CTX *raw_context,
    const unsigned char *input,
    size_t input_size
) {
    csgdb_hmac_context *context = (csgdb_hmac_context *)raw_context;
    if (context == NULL || (input == NULL && input_size != 0)) {
        return 0;
    }
    return csgdb_digest_update(context->algorithm, &context->inner, input, input_size);
}

CSGDB_INTERNAL int _csgdb_internal_evp_mac_final(
    EVP_MAC_CTX *raw_context,
    unsigned char *output,
    size_t *output_size,
    size_t output_capacity
) {
    csgdb_hmac_context *context = (csgdb_hmac_context *)raw_context;
    unsigned char inner_digest[SHA512_DIGEST_LENGTH];
    size_t digest_size;
    int result = 0;
    if (context == NULL || output_size == NULL) {
        return 0;
    }
    digest_size = csgdb_hmac_output_size(context->algorithm);
    if (digest_size == 0) {
        return 0;
    }
    *output_size = digest_size;
    if (output == NULL) {
        return 1;
    }
    if (output_capacity < digest_size ||
        !csgdb_digest_final(context->algorithm, &context->inner, inner_digest) ||
        !csgdb_digest_update(context->algorithm, &context->outer, inner_digest, digest_size) ||
        !csgdb_digest_final(context->algorithm, &context->outer, output)) {
        goto cleanup;
    }
    result = 1;

cleanup:
    OPENSSL_cleanse(inner_digest, sizeof(inner_digest));
    return result;
}

CSGDB_INTERNAL int _csgdb_internal_hmac_self_test(void) {
    static const unsigned char key[20] = {
        0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b,
        0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b
    };
    static const unsigned char input[] = "Hi There";
    static const unsigned char expected[SHA256_DIGEST_LENGTH] = {
        0xb0, 0x34, 0x4c, 0x61, 0xd8, 0xdb, 0x38, 0x53,
        0x5c, 0xa8, 0xaf, 0xce, 0xaf, 0x0b, 0xf1, 0x2b,
        0x88, 0x1d, 0xc2, 0x00, 0xc9, 0x83, 0x3d, 0xa7,
        0x26, 0xe9, 0x37, 0x6c, 0x2e, 0x32, 0xcf, 0xf7
    };
    OSSL_PARAM parameters[] = {
        { "digest", OSSL_PARAM_UTF8_STRING, "sha256", 6, 0 },
        OSSL_PARAM_END
    };
    unsigned char output[SHA256_DIGEST_LENGTH];
    size_t output_size = 0;
    EVP_MAC_CTX *context = _csgdb_internal_evp_mac_ctx_new(NULL);
    int result = context != NULL &&
        _csgdb_internal_evp_mac_init(context, key, sizeof(key), parameters) &&
        _csgdb_internal_evp_mac_update(context, input, 3) &&
        _csgdb_internal_evp_mac_update(context, input + 3, sizeof(input) - 4) &&
        _csgdb_internal_evp_mac_final(
            context,
            output,
            &output_size,
            sizeof(output)
        ) &&
        output_size == sizeof(expected) &&
        CRYPTO_memcmp(output, expected, sizeof(expected)) == 0;
    _csgdb_internal_evp_mac_ctx_free(context);
    OPENSSL_cleanse(output, sizeof(output));
    return result;
}

CSGDB_INTERNAL int _csgdb_internal_hmac_legacy_self_test(void) {
    static const unsigned char key[20] = {
        0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b,
        0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b
    };
    static const unsigned char input[] = "Hi There";
    static const unsigned char sha1_expected[SHA_DIGEST_LENGTH] = {
        0xb6, 0x17, 0x31, 0x86, 0x55, 0x05, 0x72, 0x64, 0xe2, 0x8b,
        0xc0, 0xb6, 0xfb, 0x37, 0x8c, 0x8e, 0xf1, 0x46, 0xbe, 0x00
    };
    static const unsigned char sha512_expected[SHA512_DIGEST_LENGTH] = {
        0x87, 0xaa, 0x7c, 0xde, 0xa5, 0xef, 0x61, 0x9d,
        0x4f, 0xf0, 0xb4, 0x24, 0x1a, 0x1d, 0x6c, 0xb0,
        0x23, 0x79, 0xf4, 0xe2, 0xce, 0x4e, 0xc2, 0x78,
        0x7a, 0xd0, 0xb3, 0x05, 0x45, 0xe1, 0x7c, 0xde,
        0xda, 0xa8, 0x33, 0xb7, 0xd6, 0xb8, 0xa7, 0x02,
        0x03, 0x8b, 0x27, 0x4e, 0xae, 0xa3, 0xf4, 0xe4,
        0xbe, 0x9d, 0x91, 0x4e, 0xeb, 0x61, 0xf1, 0x70,
        0x2e, 0x69, 0x6c, 0x20, 0x3a, 0x12, 0x68, 0x54
    };
    OSSL_PARAM sha1_parameters[] = {
        { "digest", OSSL_PARAM_UTF8_STRING, "sha1", 4, 0 },
        OSSL_PARAM_END
    };
    OSSL_PARAM sha512_parameters[] = {
        { "digest", OSSL_PARAM_UTF8_STRING, "sha512", 6, 0 },
        OSSL_PARAM_END
    };
    unsigned char output[SHA512_DIGEST_LENGTH];
    size_t output_size = 0;
    EVP_MAC_CTX *context = _csgdb_internal_evp_mac_ctx_new(NULL);
    int result = context != NULL &&
        _csgdb_internal_evp_mac_init(context, key, sizeof(key), sha1_parameters) &&
        _csgdb_internal_evp_mac_update(context, input, sizeof(input) - 1) &&
        _csgdb_internal_evp_mac_final(context, output, &output_size, sizeof(output)) &&
        output_size == sizeof(sha1_expected) &&
        CRYPTO_memcmp(output, sha1_expected, sizeof(sha1_expected)) == 0;

    if (result) {
        _csgdb_internal_evp_mac_ctx_free(context);
        context = _csgdb_internal_evp_mac_ctx_new(NULL);
        output_size = 0;
        result = context != NULL &&
            _csgdb_internal_evp_mac_init(
                context,
                key,
                sizeof(key),
                sha512_parameters
            ) &&
            _csgdb_internal_evp_mac_update(context, input, sizeof(input) - 1) &&
            _csgdb_internal_evp_mac_final(
                context,
                output,
                &output_size,
                sizeof(output)
            ) &&
            output_size == sizeof(sha512_expected) &&
            CRYPTO_memcmp(output, sha512_expected, sizeof(sha512_expected)) == 0;
    }
    _csgdb_internal_evp_mac_ctx_free(context);
    OPENSSL_cleanse(output, sizeof(output));
    return result;
}

/*
 * AES-256-CBC provider discovery is immutable as well. The legacy
 * EVP_aes_256_cbc() descriptor makes OpenSSL fetch the provider implementation
 * again during every page's EVP_CipherInit_ex call. Give the codec the fetched
 * provider descriptor directly and keep it alive for the process lifetime.
 * Cipher contexts, keys and IVs remain operation-local and are still reset and
 * freed normally.
 */
CSGDB_INTERNAL const EVP_CIPHER *_csgdb_internal_evp_aes_256_cbc(void) {
    if (!CRYPTO_THREAD_run_once(&csgdb_aes_once, csgdb_initialize_aes_provider) ||
        csgdb_aes_provider == NULL) {
        return NULL;
    }
    (void)CRYPTO_THREAD_run_once(&csgdb_cleanup_once, csgdb_register_crypto_cleanup);
    return csgdb_aes_provider;
}
