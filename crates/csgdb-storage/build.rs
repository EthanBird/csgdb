fn main() {
    let mut build = cc::Build::new();
    build.file("src/crypto_provider_cache.c");
    if let Some(include) = std::env::var_os("DEP_OPENSSL_INCLUDE") {
        build.include(include);
    }
    build
        .warnings(true)
        .extra_warnings(true)
        .compile("csgdb_crypto_provider_cache");
    println!("cargo:rerun-if-changed=src/crypto_provider_cache.c");
}
