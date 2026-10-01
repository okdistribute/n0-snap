fn main() {
    println!("cargo:rerun-if-changed=native/ios.m");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("ios") {
        cc::Build::new()
            .file("native/ios.m")
            .flag("-fobjc-arc")
            .flag("-fblocks")
            .compile("snap_ios");
        for framework in [
            "UIKit",
            "AVFoundation",
            "PhotosUI",
            "UniformTypeIdentifiers",
            "SafariServices",
        ] {
            println!("cargo:rustc-link-lib=framework={framework}");
        }
    }
}
