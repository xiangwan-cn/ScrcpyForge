use std::{env, path::PathBuf};

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let local = manifest.join("../../third_party/opencv-root/usr");
    let (include, lib) = if local.join("include/opencv5").exists() {
        (local.join("include/opencv5"), local.join("lib"))
    } else {
        let include = env::var_os("OPENCV_INCLUDE_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                ["/usr/include/opencv5", "/usr/include/opencv4"]
                    .into_iter()
                    .map(PathBuf::from)
                    .find(|path| path.join("opencv2/core.hpp").is_file())
            })
            .unwrap_or_else(|| PathBuf::from("/usr/include/opencv5"));
        let lib = env::var_os("OPENCV_LIB_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/usr/lib"));
        (include, lib)
    };
    if !include.join("opencv2/core.hpp").is_file() {
        panic!(
            "OpenCV headers not found under {}; set OPENCV_INCLUDE_DIR to the directory containing opencv2/core.hpp",
            include.display()
        );
    }
    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .include(include)
        .file("src/cv_bridge.cpp")
        .compile("forge_cv_bridge");
    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-lib=dylib=opencv_core");
    println!("cargo:rustc-link-lib=dylib=opencv_imgproc");
    if local.exists() && cfg!(target_os = "linux") {
        // Keep release artifacts relocatable. `tools/run-forge-daemon` still
        // sets LD_LIBRARY_PATH for development, while packaged binaries look
        // beside the application for the bundled OpenCV libraries.
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../../third_party/opencv-root/usr/lib");
    }
    println!("cargo:rerun-if-changed=src/cv_bridge.cpp");
    println!("cargo:rerun-if-env-changed=OPENCV_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=OPENCV_LIB_DIR");
}
