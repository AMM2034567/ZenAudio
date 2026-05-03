# ZenAudio

一个使用 Rust 开发的现代桌面音乐播放器，具有健壮的架构和高质量的音频处理能力。

## 特性

- **GUI**: 使用 Slint 构建的现代化界面
- **音频解码**: Symphonia 支持主流音频格式 (MP3, FLAC, WAV, OGG, AAC 等)
- **音频输出**: CPAL 跨平台音频输出
- **重采样**: Rubato 高质量采样率转换
- **无锁缓冲**: ringbuf 实现的生产者 - 消费者模型
- **异步运行时**: Tokio 处理文件 IO 和控制逻辑
- **元数据解析**: Lofty 读取 ID3 标签和专辑封面

## 技术架构

```
┌─────────────────────────────────────────────────────────────┐
│                         Slint UI                             │
│  (播放控制、进度条、音量、播放列表、专辑封面显示)              │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                      AudioEngine                             │
│  ┌──────────────┐    ┌──────────────┐    ┌──────────────┐   │
│  │   Symphonia  │───▶│    Rubato    │───▶│   ringbuf    │   │
│  │   (解码)     │    │  (重采样)    │    │  (无锁缓冲)  │   │
│  └──────────────┘    └──────────────┘    └──────────────┘   │
│                                                │             │
│                                                ▼             │
│                                      ┌──────────────┐       │
│                                      │     CPAL     │       │
│                                      │   (输出)     │       │
│                                      └──────────────┘       │
└─────────────────────────────────────────────────────────────┘
```

## 构建要求

### Linux (Ubuntu/Debian)

```bash
sudo apt-get update
sudo apt-get install -y libasound2-dev pkg-config clang
```

### Windows

- Visual Studio Build Tools 2019 或更高版本
- Rust MSVC 工具链

### macOS

```bash
brew install pkg-config
```

## 编译

### Debug 构建

```bash
cargo build
```

### Release 构建（优化）

```bash
cargo build --release
```

Release 构建启用了以下优化：
- LTO (Link Time Optimization)
- 单代码生成单元
- 符号剥离
- 最高优化级别 (opt-level = 3)

## 使用方法

```bash
# 直接运行（不带参数）
./target/release/zenaudio

# 启动时加载音频文件
./target/release/zenaudio /path/to/music.mp3
```

## CI/CD

本项目配置了 GitHub Actions 自动构建：

- **Linux**: Ubuntu latest, 安装 ALSA 依赖
- **Windows**: Windows latest, MSVC 工具链
- **macOS**: (可选启用)

推送 `v*` 标签时会自动创建 GitHub Release 并上传构建产物。

## 项目结构

```
zenaudio/
├── Cargo.toml          # 项目配置和依赖
├── build.rs            # Slint UI 编译脚本
├── ui.slint            # Slint 界面定义
├── README.md           # 项目文档
├── .gitignore          # Git 忽略规则
├── .github/
│   └── workflows/
│       └── build.yml   # GitHub Actions 配置
└── src/
    ├── main.rs         # 程序入口和 UI 逻辑
    └── audio.rs        # 音频引擎核心实现
```

## 许可证

MIT License

## 贡献

欢迎提交 Issue 和 Pull Request！
