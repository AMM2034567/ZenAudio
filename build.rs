// ZenAudio 构建脚本
// 负责编译 Slint UI 文件并嵌入到二进制中

fn main() {
    // 使用 slint-build 编译 .slint 文件
    // 这会将 UI 定义静态编译进 Rust 代码
    slint_build::compile_with_config(
        "ui.slint",
        slint_build::CompilerConfiguration::new()
            .embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftware),
    )
    .expect("Slint UI 编译失败");

    // 重新运行如果 .slint 文件发生变化
    println!("cargo:rerun-if-changed=ui.slint");
}
