//! Embeds the Windows application manifest: Common Controls v6 (themed
//! controls in the shortcuts window) and per-monitor DPI awareness. Works
//! with both the MSVC and GNU toolchains without rc.exe or windres.

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_manifest::embed_manifest(embed_manifest::new_manifest("BrightnessTray")).expect("unable to embed manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
