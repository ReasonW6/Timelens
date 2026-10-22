fn main() {
    embed_resource::compile("app.rc", version_macros())
        .manifest_required()
        .expect("failed to embed the Timelens collector manifest");
}

/// The package version for the VERSIONINFO resource in `app.rc`.
fn version_macros() -> [String; 4] {
    let part = |name: &str| std::env::var(name).expect("cargo sets the package version");
    [
        format!("TIMELENS_VERSION_MAJOR={}", part("CARGO_PKG_VERSION_MAJOR")),
        format!("TIMELENS_VERSION_MINOR={}", part("CARGO_PKG_VERSION_MINOR")),
        format!("TIMELENS_VERSION_PATCH={}", part("CARGO_PKG_VERSION_PATCH")),
        format!("TIMELENS_VERSION=\"{}\"", part("CARGO_PKG_VERSION")),
    ]
}
