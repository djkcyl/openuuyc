fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let windows = target_os == "windows";
    assert!(
        windows || target_os == "linux",
        "OpenUUYC supports Windows and Linux; {target_os} has no platform backend"
    );
    if windows {
        println!("cargo:rerun-if-changed=assets/windows.rc");
        println!("cargo:rerun-if-changed=assets/icon.ico");
        embed_resource::compile_for("assets/windows.rc", ["OpenUUYC"], embed_resource::NONE)
            .manifest_required()
            .expect("compile Windows application icon");
    }
    build_neteq(windows);
}

fn build_neteq(windows: bool) {
    let root = std::path::Path::new("vendor/webrtc_neteq");
    println!("cargo:rerun-if-changed=vendor/webrtc_neteq");
    println!("cargo:rerun-if-changed=src/audio/neteq_bridge.cc");
    let sources = std::fs::read_to_string(root.join("sources.txt")).expect("NetEq source manifest");
    let mut cpp = cc::Build::new();
    let mut c = cc::Build::new();
    for build in [&mut cpp, &mut c] {
        build
            .opt_level(3)
            .include(root)
            .define("NDEBUG", None)
            .define("RTC_DISABLE_LOGGING", None)
            .define("RTC_DISABLE_TRACE_EVENTS", None)
            .define("WEBRTC_APM_DEBUG_DUMP", "0")
            .define("WEBRTC_OPUS_SUPPORT_120MS_PTIME", "1")
            .warnings(false);
        if windows {
            build
                .define("WEBRTC_WIN", None)
                .define("NOMINMAX", None)
                .define("WIN32_LEAN_AND_MEAN", None);
        } else {
            build
                .define("WEBRTC_POSIX", None)
                .define("WEBRTC_LINUX", None);
        }
    }
    cpp.cpp(true).std("c++17");
    if windows {
        cpp.flag_if_supported("/EHsc")
            .flag_if_supported("/Zc:__cplusplus");
    }
    cpp.file("src/audio/neteq_bridge.cc");
    for source in sources
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        if source.ends_with(".cc") {
            cpp.file(root.join(source));
        } else if source.ends_with(".c") {
            c.file(root.join(source));
        }
    }
    cpp.compile("openuuyc_neteq");
    c.compile("openuuyc_neteq_dsp");
    if windows {
        println!("cargo:rustc-link-lib=winmm");
        println!("cargo:rustc-link-lib=ws2_32");
    } else {
        println!("cargo:rustc-link-lib=stdc++");
    }
}
