fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/pdfaxe.ico");
        res.set("ProductName", "PDFAxe");
        res.set("FileDescription", "PDFAxe — local, private PDF surgery");
        res.set("LegalCopyright", "LXB Studio LLC — AGPL-3.0-or-later");
        if let Err(e) = res.compile() {
            // A missing SDK shouldn't break the build — the exe just ships iconless.
            println!("cargo:warning=couldn't embed icon/resources: {e}");
        }
    }
    println!("cargo:rerun-if-changed=../../assets/pdfaxe.ico");
}
