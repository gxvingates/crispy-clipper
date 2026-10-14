#[path = "src/icon.rs"]
mod icon;

fn main() {
    println!("cargo:rerun-if-changed=src/icon.rs");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("app.ico");
    std::fs::write(&out, icon::ico(&[16, 32, 48, 256])).unwrap();
    let mut res = winresource::WindowsResource::new();
    res.set_icon(out.to_str().unwrap());
    res.set("ProductName", "Crispy Clipper");
    res.set("FileDescription", "Crispy Clipper");
    res.compile().expect("embedding the icon needs the Windows SDK (rc.exe)");
}
