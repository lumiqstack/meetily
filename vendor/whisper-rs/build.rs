use std::env;

fn main() {
    println!("cargo:rerun-if-changed=native");
    cc::Build::new().cpp(true).file("native/guard.cpp").include("native")
        .flag_if_supported("-std=c++11").flag_if_supported("/EHsc")
        .compile("meetily_whisper_guard");
    let whisper_cpp_version = env::var("DEP_WHISPER_WHISPER_CPP_VERSION").unwrap_or_else(|e| {
        if env::var("DOCS_RS").is_ok() {
            // not sure why but this fails on docs.rs
            // return a default string
            "0.0.0-fake".to_string()
        } else {
            panic!("Failed to find upstream whisper.cpp version: your build environment is messed up. {}", e);
        }
    });
    println!(
        "cargo:rustc-env=WHISPER_CPP_VERSION={}",
        whisper_cpp_version
    );
}
