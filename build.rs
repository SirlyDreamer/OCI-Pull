fn main() {
    println!("cargo:rerun-if-changed=assets/brand/app.rc");
    println!("cargo:rerun-if-changed=assets/brand/logo.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("assets/brand/app.rc", embed_resource::NONE)
            .manifest_required()
            .expect("failed to embed the OCI Pull application icon");
    }
}
