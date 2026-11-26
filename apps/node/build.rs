fn main() {
    println!("cargo:rerun-if-changed=native/macos/av.m");
    println!("cargo:rerun-if-changed=native/macos/ui.m");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os != "macos" {
        return;
    }
    let out = std::env::var("OUT_DIR").unwrap();
    let sdk = std::process::Command::new("xcrun")
        .args(["--sdk", "macosx", "--show-sdk-path"])
        .output()
        .expect("xcrun");
    let sdk = String::from_utf8(sdk.stdout).unwrap();
    let sdk = sdk.trim();
    let mut objs = Vec::new();
    for (src, obj_name) in [("native/macos/av.m", "av.o"), ("native/macos/ui.m", "ui.o")] {
        let obj = format!("{out}/{obj_name}");
        let status = std::process::Command::new("clang")
            .args([
                "-fobjc-arc",
                "-fmodules",
                "-mmacosx-version-min=12.3",
                "-isysroot",
                sdk,
                "-c",
                src,
                "-o",
                &obj,
            ])
            .status()
            .expect("clang");
        if !status.success() {
            panic!("编译 macOS 原生适配器失败: {src}");
        }
        objs.push(obj);
    }
    let lib = format!("{out}/libjrpav.a");
    let mut ar = std::process::Command::new("ar");
    ar.arg("rcs").arg(&lib);
    for obj in &objs {
        ar.arg(obj);
    }
    if !ar.status().expect("ar").success() {
        panic!("打包原生适配器失败");
    }
    println!("cargo:rustc-link-search=native={out}");
    println!("cargo:rustc-link-lib=static=jrpav");
    for framework in ["ScreenCaptureKit", "VideoToolbox", "AVFoundation", "AppKit", "CoreMedia", "CoreVideo", "CoreGraphics", "CoreImage", "Foundation"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    println!("cargo:rustc-link-lib=objc");
}
