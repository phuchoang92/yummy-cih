//! lbug links against OpenSSL (`-lssl -lcrypto`) but does not emit a search
//! path for it. macOS ships no system OpenSSL, so point the linker at the
//! Homebrew install (or an explicit `OPENSSL_LIB_DIR`). Linux distros keep
//! libssl in the default linker path, so this is a no-op there.

fn main() {
    println!("cargo:rerun-if-env-changed=OPENSSL_LIB_DIR");
    if let Ok(dir) = std::env::var("OPENSSL_LIB_DIR") {
        println!("cargo:rustc-link-search=native={dir}");
        return;
    }
    #[cfg(target_os = "macos")]
    {
        for prefix in ["/opt/homebrew/opt/openssl@3", "/usr/local/opt/openssl@3"] {
            let lib = std::path::Path::new(prefix).join("lib");
            if lib.exists() {
                println!("cargo:rustc-link-search=native={}", lib.display());
                return;
            }
        }
        println!(
            "cargo:warning=cih-ladybug: no OpenSSL found (lbug links -lssl). \
             Install Homebrew openssl@3 or set OPENSSL_LIB_DIR, or the final \
             link will fail with 'library ssl not found'."
        );
    }
}
