//! 给 Windows 可执行文件嵌入图标与版本资源。
//!
//! 只在 Windows 上做：资源由 Windows SDK 的 rc.exe 编译，其他平台直接跳过。

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");

    #[cfg(windows)]
    {
        let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "MiBand Heart Rate for OBS");
        res.set("FileDescription", "MiBand Heart Rate for OBS");
        res.set("OriginalFilename", "miband-heart-rate.exe");
        res.set("LegalCopyright", "MIT License");
        res.set("FileVersion", &version);
        res.set("ProductVersion", &version);

        if let Err(err) = res.compile() {
            // 嵌入失败不阻断构建，只是 exe 没有图标/版本信息。
            println!("cargo:warning=嵌入 Windows 资源失败：{err}");
        }
    }
}
