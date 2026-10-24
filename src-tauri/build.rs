fn main() {
    println!("cargo:rerun-if-changed=windows-app-manifest.xml");
    let attributes = tauri_build::Attributes::new().windows_attributes(
        tauri_build::WindowsAttributes::new()
            .app_manifest(include_str!("windows-app-manifest.xml")),
    );
    tauri_build::try_build(attributes).expect("could not build Tauri application resources");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var_os("CARGO_FEATURE_GPU_CUDA").is_some()
    {
        // Keep startup and the packaging loader check usable without a GPU.
        // The NVIDIA driver is installed by the OS/admin, never redistributed.
        println!("cargo:rustc-link-lib=delayimp");
        println!("cargo:rustc-link-arg=/DELAYLOAD:nvcuda.dll");
    }

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        // whisper.cpp's Metal backend uses Objective-C availability checks.
        // Rust links with -nodefaultlibs, so Clang's availability runtime is
        // not added automatically. Discover it from the selected Xcode
        // toolchain rather than hard-coding an Xcode or Clang version.
        println!("cargo:rerun-if-env-changed=DEVELOPER_DIR");
        let output = std::process::Command::new("xcrun")
            .args(["--sdk", "macosx", "clang", "--print-file-name=libclang_rt.osx.a"])
            .output()
            .expect("Xcode command line tools are required for the macOS build");
        assert!(output.status.success(), "could not locate the Clang macOS runtime");
        let path = std::path::PathBuf::from(
            String::from_utf8(output.stdout).expect("Clang runtime path must be UTF-8").trim(),
        );
        assert!(path.is_file(), "Clang macOS runtime does not exist: {}", path.display());
        println!("cargo:rustc-link-search=native={}", path.parent().unwrap().display());
        println!("cargo:rustc-link-lib=static=clang_rt.osx");
    }
}
