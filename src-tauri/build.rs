fn main() {
    // ort's prebuilt ONNX Runtime uses `@available(...)`, which compiles to calls into
    // `___isPlatformVersionAtLeast` from clang's compiler-rt. rustc links with
    // -nodefaultlibs, so libclang_rt.osx.a has to be added explicitly.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        if let Ok(out) = std::process::Command::new("clang").arg("--print-resource-dir").output() {
            let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !dir.is_empty() {
                println!("cargo:rustc-link-search=native={dir}/lib/darwin");
                println!("cargo:rustc-link-lib=static=clang_rt.osx");
            }
        }
    }
    tauri_build::build()
}
