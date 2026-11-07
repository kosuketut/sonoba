// The manifest gives the app today's controls instead of the Windows 95 ones. app.res holds the
// icon, compiled once from app.rc so that building needs no Windows SDK.
fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=app.res");
    println!("cargo:rustc-link-arg-bins={dir}\\app.res");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{dir}\\app.manifest");
}
