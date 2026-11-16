//! Windows: puts the LocalBeat mark and version details into
//! localbeat-studio.exe, for Explorer, the Start menu and the task bar.

fn main() {
    println!("cargo:rerun-if-changed=assets/localbeat.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/localbeat.ico")
            .set("FileDescription", "LocalBeat Studio")
            .set("ProductName", "LocalBeat Studio");
        if let Err(e) = res.compile() {
            // An exe without an icon is still an exe.
            println!("cargo:warning=no icon in the exe ({e})");
        }
    }
}
