fn main() {
    tauri_build::build();

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
