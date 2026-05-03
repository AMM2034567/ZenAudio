// ZenAudio 主程序入口
// 整合 Slint UI、音频引擎和元数据解析

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;

use anyhow::{Context, Result};
use audio::{AudioEngine, PlayerState};
use lofty::Accessor;
use slint::{ComponentHandle, SharedString, VecModel};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

// 引入 Slint UI（静态编译进二进制）
slint::include_modules!();

/// 播放列表项
#[derive(Clone)]
struct PlaylistItem {
    path: PathBuf,
    title: String,
    artist: String,
    duration: String,
}

/// 应用主状态
struct AppState {
    audio_engine: Option<Arc<AudioEngine>>,
    playlist: Vec<PlaylistItem>,
    current_index: usize,
    is_loaded: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // 初始化日志（调试用）
    #[cfg(debug_assertions)]
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    println!("Zenaudio 启动中...");

    // 创建 Slint 主窗口
    let main_window = MainWindow::new().context("无法创建 Slint 窗口")?;

    // 创建应用状态
    let state = Arc::new(Mutex::new(AppState {
        audio_engine: None,
        playlist: Vec::new(),
        current_index: 0,
        is_loaded: false,
    }));

    // 克隆状态供回调使用
    let state_clone = Arc::clone(&state);
    
    // 处理"添加文件"按钮点击
    let add_file_handler = {
        let main_window_weak = main_window.as_weak();
        let state = Arc::clone(&state);
        move || {
            // 在实际应用中，这里会打开文件选择对话框
            // 由于 Slint 的文件对话框需要平台特定支持，这里演示直接加载测试文件
            println!("添加文件请求收到");
            
            // 注意：实际使用中应该弹出文件选择器
            // 这里仅作为演示，假设用户通过命令行参数或其他方式提供文件路径
        }
    };

    // 处理播放按钮
    let play_handler = {
        let state = Arc::clone(&state);
        move || {
            tokio::spawn(async move {
                if let Some(ref engine) = state.lock().await.audio_engine {
                    if let Err(e) = engine.play() {
                        eprintln!("播放失败：{:?}", e);
                    }
                }
            });
        }
    };

    // 处理暂停按钮
    let pause_handler = {
        let state = Arc::clone(&state);
        move || {
            tokio::spawn(async move {
                if let Some(ref engine) = state.lock().await.audio_engine {
                    if let Err(e) = engine.pause() {
                        eprintln!("暂停失败：{:?}", e);
                    }
                }
            });
        }
    };

    // 处理停止按钮
    let stop_handler = {
        let state = Arc::clone(&state);
        move || {
            tokio::spawn(async move {
                if let Some(ref engine) = state.lock().await.audio_engine {
                    if let Err(e) = engine.stop() {
                        eprintln!("停止失败：{:?}", e);
                    }
                }
            });
        }
    };

    // 设置回调处理器
    main_window.on_add_file(add_file_handler);
    main_window.on_play(play_handler);
    main_window.on_pause(pause_handler);
    main_window.on_stop(stop_handler);

    // 初始化音频引擎
    match AudioEngine::new().await {
        Ok(engine) => {
            let arc_engine = Arc::new(engine);
            state.lock().await.audio_engine = Some(Arc::clone(&arc_engine));
            println!("音频引擎初始化成功");
        }
        Err(e) => {
            eprintln!("音频引擎初始化失败：{:?}", e);
            // 即使音频引擎失败，UI 仍可显示
        }
    }

    // 启动进度更新循环
    let ui_handle = main_window.as_weak();
    let state_for_timer = Arc::clone(&state);
    
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            
            if let Some(window) = ui_handle.upgrade() {
                let state_guard = state_for_timer.lock().await;
                
                if let Some(ref engine) = state_guard.audio_engine {
                    // 获取当前播放位置
                    let position_ms = engine.get_position_ms();
                    let duration_ms = engine.get_duration_ms();
                    
                    // 计算进度
                    let progress = if duration_ms > 0 {
                        position_ms as f32 / duration_ms as f32
                    } else {
                        0.0
                    };
                    
                    // 格式化时间显示
                    let progress_text = format_time(position_ms) + " / " + &format_time(duration_ms);
                    
                    // 更新 UI
                    window.set_progress(progress);
                    window.set_progress_text(SharedString::from(progress_text));
                    
                    // 更新播放状态
                    let is_playing = engine.get_state() == PlayerState::Playing;
                    window.set_is_playing(is_playing);
                    window.set_has_file(engine.has_file());
                }
            }
        }
    });

    // 如果有命令行参数，尝试加载第一个参数作为音频文件
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        let file_path = PathBuf::from(&args[1]);
        if file_path.exists() {
            // 克隆必要的引用
            let state_clone = Arc::clone(&state);
            let window_clone = main_window.clone_strong();
            
            // 异步加载文件
            tokio::spawn(async move {
                load_and_play_file(&file_path, &state_clone, &window_clone).await;
            });
        }
    }

    // 运行 Slint 事件循环
    main_window.run()?;

    Ok(())
}

/// 加载并播放音频文件
async fn load_and_play_file(
    path: &PathBuf,
    state: &Arc<Mutex<AppState>>,
    window: &MainWindow,
) {
    println!("正在加载文件：{:?}", path);

    // 解析元数据
    let (title, artist, duration_str) = match parse_metadata(path) {
        Ok(meta) => meta,
        Err(e) => {
            eprintln!("解析元数据失败：{:?}", e);
            ("未知歌曲".to_string(), "未知艺术家".to_string(), "0:00".to_string())
        }
    };

    // 提取专辑封面（如果存在）
    // 注意：Slint 的图片加载需要特殊处理，这里简化处理
    
    // 更新 UI 显示
    window.set_song_title(SharedString::from(title.clone()));
    window.set_artist_name(SharedString::from(artist.clone()));

    // 加载到音频引擎
    if let Some(ref engine) = state.lock().await.audio_engine {
        if let Err(e) = engine.load_file(path.clone()) {
            eprintln!("加载音频文件失败：{:?}", e);
            return;
        }

        // 开始播放
        if let Err(e) = engine.play() {
            eprintln!("播放失败：{:?}", e);
        }
    }

    // 添加到播放列表
    let mut state_guard = state.lock().await;
    state_guard.playlist.push(PlaylistItem {
        path: path.clone(),
        title,
        artist,
        duration: duration_str,
    });

    // 更新播放列表 UI
    // 注意：这里需要动态更新 Slint 的模型，实际实现可能需要更复杂的处理
}

/// 解析音频文件的元数据
fn parse_metadata(path: &PathBuf) -> Result<(String, String, String)> {
    let tagged_file = lofty::read_from_path(path)
        .with_context(|| format!("无法读取文件 {:?}", path))?;

    let tag = tagged_file.primary_tag()
        .or_else(|| tagged_file.first_tag())
        .context("文件中没有标签信息")?;

    let title = tag.title()
        .unwrap_or("未知歌曲")
        .to_string();
    
    let artist = tag.artist()
        .unwrap_or("未知艺术家")
        .to_string();

    // 计算时长（需要从音频属性获取）
    let duration_str = if let Some(props) = tagged_file.properties() {
        let duration_secs = props.duration().as_secs();
        let mins = duration_secs / 60;
        let secs = duration_secs % 60;
        format!("{}:{:02}", mins, secs)
    } else {
        "0:00".to_string()
    };

    Ok((title, artist, duration_str))
}

/// 格式化毫秒时间为 MM:SS 格式
fn format_time(ms: u64) -> String {
    let total_secs = ms / 1000;
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{}:{:02}", mins, secs)
}

/// 从文件路径提取文件名（不含扩展名）
fn extract_filename(path: &PathBuf) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("未知歌曲")
        .to_string()
}

// 注意：关于专辑封面显示的补充说明
// 
// 在 Slint 中显示动态图片需要将图片数据转换为 Slint 的 Image 类型。
// 这通常涉及以下步骤：
// 1. 使用 Lofty 提取封面图片数据（JPEG/PNG 字节）
// 2. 使用 image crate 解码字节
// 3. 转换为 Slint::Image
//
// 由于这涉及到较多的平台特定代码和图片处理，
// 在这个核心架构示例中我们简化了这部分实现。
// 完整实现可以参考 Slint 文档中的图片加载示例。
