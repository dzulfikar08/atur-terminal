//! Compiles the Objective-C part of the macOS capture (`ScreenCaptureKit` and the
//! pasteboard are Objective-C APIs) and links the frameworks the program uses.
//! On other systems there is nothing to build.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/mac/capture.m");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    cc::Build::new()
        .file("src/mac/capture.m")
        .flag("-fobjc-arc")
        .flag("-Wno-unguarded-availability-new")
        .compile("atur_capture");
    for framework in [
        "AppKit",
        "ApplicationServices",
        "Carbon",
        "CoreFoundation",
        "CoreGraphics",
        "CoreMedia",
        "CoreVideo",
        "Foundation",
        "ScreenCaptureKit",
        "VideoToolbox",
    ] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
}
