use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=cih.exe.manifest");

    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
        || std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
    {
        return;
    }

    let manifest = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR")
            .expect("Cargo must provide CARGO_MANIFEST_DIR to build scripts"),
    )
    .join("cih.exe.manifest");

    // Merge the portable product manifest with MSVC's generated UAC manifest
    // and embed the result as RT_MANIFEST resource 1 in cih.exe.
    println!("cargo:rustc-link-arg-bin=cih=/MANIFEST:EMBED,ID=1");
    println!(
        "cargo:rustc-link-arg-bin=cih=/MANIFESTINPUT:{}",
        manifest.display()
    );
}
