use std::{env, path::PathBuf};

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let local = manifest.join("../../third_party/opencv-root/usr");
    let (include, lib) = if local.join("include/opencv5").exists() {
        (local.join("include/opencv5"), local.join("lib"))
    } else {
        let include = env::var_os("OPENCV_INCLUDE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/usr/include/opencv5"));
        let lib = env::var_os("OPENCV_LIB_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/usr/lib"));
        (include, lib)
    };
    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .include(include)
        .file("src/cv_bridge.cpp")
        .compile("forge_cv_bridge");
    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-lib=dylib=opencv_core");
    println!("cargo:rustc-link-lib=dylib=opencv_imgproc");
    if local.exists() {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib.display());
    }
    println!("cargo:rerun-if-changed=src/cv_bridge.cpp");
    println!("cargo:rerun-if-env-changed=OPENCV_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=OPENCV_LIB_DIR");
}
