//! Gives the Windows executable its icon. Elsewhere, and if the resource
//! compiler is missing, the build simply goes on without it.

fn main() {
    println!("cargo:rerun-if-changed=../../assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/icon.ico");
        res.set("ProductName", "MalasDoh");
        res.set("FileDescription", "MalasDoh SSH client");
        if let Err(e) = res.compile() {
            println!("cargo:warning=no icon in the .exe: {e}");
        }
    }
}
