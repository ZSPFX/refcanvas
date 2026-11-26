//! link the windows icon resource

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/icons/refcanvas.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let icon = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/icons/refcanvas.ico");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));

    // quoted path keeps spaces working
    let rc = out.join("refcanvas.rc");
    std::fs::write(&rc, format!("APP_ICON ICON \"{}\"\n", icon.display()))
        .expect("write resource script");

    let res = out.join("refcanvas.res");
    for tool in ["llvm-rc", "rc"] {
        let status = Command::new(tool)
            .args(["/nologo", "/fo"])
            .arg(&res)
            .arg(&rc)
            .status();
        match status {
            Ok(s) if s.success() => {
                // link the compiled resource into binaries only
                println!("cargo:rustc-link-arg-bins={}", res.display());
                return;
            }
            Ok(s) => eprintln!("refcanvas: {tool} failed ({s}), trying the next tool"),
            Err(_) => {} // not installed
        }
    }

    // cosmetic only; never fail the build over the icon
    println!("cargo:warning=no working rc.exe/llvm-rc found, the .exe has no icon");
}
