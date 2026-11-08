// On Windows the manifest gives the app today's controls instead of the Windows 95 ones, and
// app.res holds the icon, compiled once from app.rc so that building needs no Windows SDK. A Mac
// build needs neither: its icon and settings live in the app bundle that package-mac.sh makes.
fn main() {
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=app.res");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-arg-bins={dir}\\app.res");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{dir}\\app.manifest");
}
