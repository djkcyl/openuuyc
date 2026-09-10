fn main() {
    build_neteq();
    println!("cargo:rerun-if-changed=vendor/speexdsp");
    cc::Build::new()
        .opt_level(3)
        .file("vendor/speexdsp/libspeexdsp/resample.c")
        .include("vendor/speexdsp/include")
        .define("FLOATING_POINT", "1")
        .define("EXPORT", "")
        .flag_if_supported("-fwrapv")
        .warnings(false)
        .compile("openuuyc_speexdsp");
}

fn build_neteq() {
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
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
            build
                .define("WEBRTC_WIN", None)
                .define("NOMINMAX", None)
                .define("WIN32_LEAN_AND_MEAN", None);
        } else {
            build.define("WEBRTC_POSIX", None);
            if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
                build.define("WEBRTC_MAC", None);
            }
        }
    }
    cpp.cpp(true)
        .std("c++17")
        .flag_if_supported("/EHsc")
        .flag_if_supported("/Zc:__cplusplus");
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
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-lib=winmm");
        println!("cargo:rustc-link-lib=ws2_32");
    }
}
