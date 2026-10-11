//! Compiles the Apple capture/encode shim and links the frameworks it uses.
//!
//! Only the Apple targets need a build script: the Windows backend talks to Media Foundation
//! through the `windows` crate and the Linux backend speaks to V4L2/VAAPI itself, so both are
//! plain Rust with no shim to compile. macOS and iOS share one shim, because AVFoundation and
//! VideoToolbox are the same APIs on both; the platform differences live behind
//! `TARGET_OS_IPHONE` in `src/macos/shim.mm`. Targets without a backend get
//! `src/unsupported.rs` and this script does nothing.

use std::env;

/// Frameworks the shim references (`AVFoundation` capture, `VideoToolbox` encoding,
/// `ImageIO`/`CoreGraphics`/`CoreImage` stills). All of them ship with iOS as well.
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
    println!("cargo::rerun-if-changed=build.rs");

    if !matches!(env::var("CARGO_CFG_TARGET_OS").as_deref(), Ok("macos") | Ok("ios")) {
        return;
    }

    println!("cargo::rerun-if-changed=src/macos/shim.mm");
    println!("cargo::rerun-if-changed=src/macos/shim.h");

    // `cc` cross-compiles from macOS to iOS by asking `xcrun` for the iPhoneOS SDK and
    // passing `-target arm64-apple-ios…`, so the same invocation serves both platforms.
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
    // that in on its own. libc++ is part of the OS on iOS too, so the same flag works there.
    println!("cargo::rustc-link-lib=c++");

    for framework in FRAMEWORKS {
        println!("cargo::rustc-link-lib=framework={framework}");
    }
}
