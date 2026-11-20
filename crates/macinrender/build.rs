use std::env;
use std::fs;
use std::path::{Path, PathBuf};

mod native_link;

/// ADM Core's checkout, for the headers and the Objective-C bridge the player
/// compiles itself. Cargo, not this script, fetches it: the islands' `CMake`
/// tree records it as its home directory, next to the link manifest that
/// `mradm-islands-sys` hands its dependants.
fn core_source(manifest: &Path) -> PathBuf {
    for directory in manifest.ancestors().skip(1) {
        let Ok(cache) = fs::read_to_string(directory.join("CMakeCache.txt")) else {
            continue;
        };
        let home = cache
            .lines()
            .find_map(|line| line.strip_prefix("CMAKE_HOME_DIRECTORY:INTERNAL="))
            .unwrap_or_else(|| panic!("{} names no source", directory.display()));
        // The islands' own project is `src/`; a top-level test tree is the root.
        let home = Path::new(home);
        return [home, home.parent().unwrap_or(home)]
            .into_iter()
            .find(|root| root.join("include/adm/c_api.h").is_file())
            .unwrap_or_else(|| panic!("{} is not an ADM Core checkout", home.display()))
            .to_path_buf();
    }
    panic!(
        "No CMake tree holds the islands' link manifest {}",
        manifest.display()
    );
}

fn main() {
    println!("cargo:rerun-if-changed=native");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=native_link.rs");
    println!("cargo:rustc-check-cfg=cfg(native_macinrender)");
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    if os != "macos" && os != "windows" {
        return;
    }
    if os == "macos" {
        compile_atmos_assist();
    }
    println!("cargo:rustc-cfg=native_macinrender");
    // `mradm-islands-sys` links the islands and codecs; check that what it
    // links is static before anything else depends on it.
    let manifest = PathBuf::from(
        env::var_os("DEP_MRADM_ISLANDS_MANIFEST").expect("mradm-islands-sys gave no link manifest"),
    );
    native_link::audit(&manifest, os == "windows");
    let source = core_source(&manifest);
    // Packaging records where the archives were built and which commit.
    println!("cargo:lib_dir={}", manifest.parent().unwrap().display());
    println!("cargo:source_dir={}", source.display());
    println!("cargo:linkage=static");
    let include = source.join("include");
    let bridge = source.join("gui/native");
    cc::Build::new()
        .file("native/abi_probe.c")
        .std("c11")
        .include(&include)
        .include(&bridge)
        .compile("macinrender_abi_probe");
    if os == "macos" {
        let motion = bridge.join("mr_headmotion.m");
        println!("cargo:rerun-if-changed={}", motion.display());
        cc::Build::new()
            .file(motion)
            .include(&bridge)
            .flag("-fobjc-arc")
            .flag("-fblocks")
            .flag("-mmacosx-version-min=14.0")
            .compile("macindecode_headmotion");
        for framework in ["CoreMotion", "Foundation"] {
            println!("cargo:rustc-link-lib=framework={framework}");
        }
    }
}

fn compile_atmos_assist() {
    println!("cargo:rerun-if-changed=../../assets/audio/atmos-assist.m4a");
    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .flag("-fobjc-arc")
        .flag("-fblocks")
        .flag("-mmacosx-version-min=14.0")
        .file("native/atmos_assist.mm")
        .compile("macindecode_atmos_assist");
    for framework in [
        "AVFoundation",
        "Foundation",
        "MediaToolbox",
        "CoreMedia",
        "AudioToolbox",
        "CoreAudio",
    ] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
}
