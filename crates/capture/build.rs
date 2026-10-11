//! Compiles the macOS capture/encode shim and links the frameworks it uses.
//!
//! The shim exists only on macOS; every other target gets the stub backend in
//! `src/unsupported.rs` and this script does nothing.

use std::env;

/// Frameworks the shim references (`AVFoundation` capture, `VideoToolbox` encoding,
/// `ImageIO`/`CoreGraphics`/`CoreImage` stills).
const FRAMEWORKS: [&str; 8] = [
    "AVFoundation",
    "CoreMedia",
    "CoreVideo",
    "VideoToolbox",
    "CoreImage",
    "CoreGraphics",
    "ImageIO",
    "Foundation",
];

fn main() {
    println!("cargo::rerun-if-changed=src/macos/shim.mm");
    println!("cargo::rerun-if-changed=src/macos/shim.h");
    println!("cargo::rerun-if-changed=build.rs");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    cc::Build::new()
        .file("src/macos/shim.mm")
        // The extension already selects Objective-C++, but `-x` makes it explicit for any
        // driver Cargo hands the file to.
        .flag("-x")
        .flag("objective-c++")
        .flag("-std=c++17")
        .flag("-fobjc-arc")
        .warnings(true)
        .compile("scwc_mac_shim");

    // The shim uses `std::string`, `std::vector` and friends, whose out-of-line members
    // live in libc++; rustc links the final binary with the C driver, which does not pull
    // that in on its own.
    println!("cargo::rustc-link-lib=c++");

    for framework in FRAMEWORKS {
        println!("cargo::rustc-link-lib=framework={framework}");
    }
}
