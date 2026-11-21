fn main() {
    #[cfg(target_os = "macos")]
    {
        use std::path::PathBuf;
        use std::process::Command;

        println!("cargo:rustc-link-lib=framework=Carbon");
        println!("cargo:rustc-link-lib=framework=Cocoa");
        println!("cargo:rustc-link-lib=framework=ApplicationServices");
        println!("cargo:rustc-link-lib=framework=CoreGraphics");
        println!("cargo:rustc-link-lib=framework=ServiceManagement");

        let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
        let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
        let source = manifest.join("src/macos_capture.swift");
        let helper = out_dir.join("function-macos-capture");
        let module_cache = out_dir.join("swift-module-cache");
        println!("cargo:rerun-if-changed={}", source.display());
        // Swift's PCM cache embeds absolute paths, so it becomes invalid if the checkout moves.
        let _ = std::fs::remove_dir_all(&module_cache);
        std::fs::create_dir_all(&module_cache).expect("could not create Swift module cache");
        let status = Command::new("swiftc")
            .args(["-module-cache-path"])
            .arg(&module_cache)
            .args(["-O"])
            .arg(&source)
            .args(["-framework", "ScreenCaptureKit", "-framework", "AppKit", "-framework", "CoreImage", "-framework", "CoreMedia", "-framework", "ApplicationServices", "-o"])
            .arg(&helper)
            .status()
            .expect("swiftc is required to build the macOS window capture helper");
        assert!(status.success(), "failed to build the ScreenCaptureKit helper");
        println!("cargo:rustc-env=FUNCTION_MACOS_CAPTURE_HELPER={}", helper.display());

        if let Some(target_dir) = out_dir.ancestors().nth(3) {
            let helper_dir = target_dir.join("Function.app/Contents/Helpers");
            let _ = std::fs::create_dir_all(&helper_dir);
            let _ = std::fs::copy(&helper, helper_dir.join("function-macos-capture"));
        }
    }
}
