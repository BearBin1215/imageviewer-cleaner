//! 把权限清单嵌进可执行文件。
//!
//! 默认用 `app.manifest`（requireAdministrator）；
//! 启用 `dev-no-elevation` feature 时改用 `dev.manifest`（asInvoker），
//! 便于在编辑器里直接调试，不必每次都确认 UAC。

fn main() {
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=dev.rc");
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=dev.manifest");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let dev = std::env::var_os("CARGO_FEATURE_DEV_NO_ELEVATION").is_some();
    let rc = if dev { "dev.rc" } else { "app.rc" };
    let _ = embed_resource::compile(rc, embed_resource::NONE);
}
