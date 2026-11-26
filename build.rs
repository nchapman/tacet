use std::process::Command;

fn main() {
    // ort-sys's prebuilt macOS static library references `___isPlatformVersionAtLeast`
    // from Clang's compiler-rt archive, which rustc's `-nodefaultlibs` link line
    // omits under LTO release builds. Link `clang_rt.osx` explicitly on macOS.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
        && let Ok(out) = Command::new("clang").arg("--print-runtime-dir").output()
        && out.status.success()
    {
        let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !dir.is_empty() {
            println!("cargo:rustc-link-search=native={dir}");
            println!("cargo:rustc-link-lib=static=clang_rt.osx");
        }
    }
}
