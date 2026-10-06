//! Stamps the build time into the binary, shown in the window title so it's
//! obvious when an older build is still running.

fn main() {
    for path in ["src", "../capture/src", "../soniox/src", "build.rs"] {
        println!("cargo:rerun-if-changed={path}");
    }
    let now = chrono::Local::now().format("%b %-d %H:%M");
    println!("cargo:rustc-env=BUILD_TIME={now}");
}
