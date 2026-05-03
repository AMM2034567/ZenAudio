// ZenAudio 音频引擎模块
// 负责音频解码、重采样、缓冲和输出

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, SupportedBufferSize};
use ringbuf::{HeapRb, Rb};
use rubato::{Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction};
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::{Hint, Probe};
use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

/// 音频播放器状态
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlayerState {
    Stopped,
    Playing,
    Paused,
}

/// 音频引擎配置
#[derive(Clone)]
pub struct AudioEngineConfig {
    /// 环状缓冲区大小（秒）
    pub buffer_duration_secs: f32,
    /// 重采样质量 (0.0 - 1.0)
    pub resampling_quality: f32,
}

impl Default for AudioEngineConfig {
    fn default() -> Self {
        Self {
            buffer_duration_secs: 0.5, // 500ms 缓冲
            resampling_quality: 0.95,  // 高质量重采样
        }
    }
}

/// 内部消息类型，用于控制音频解码线程
#[derive(Debug)]
enum DecoderMessage {
    LoadFile(PathBuf),
    Play,
    Pause,
    Stop,
    Seek(u64), // 毫秒
    SetVolume(f32),
}

/// 音频引擎主结构体
/// 
/// 架构说明：
/// 1. 解码线程：后台运行，从文件读取 -> Symphonia 解码 -> Rubato 重采样 -> RingBuf
/// 2. CPAL 回调：仅从 RingBuf 读取数据，无锁无阻塞
/// 3. 控制接口：通过 mpsc 通道发送命令到解码线程
pub struct AudioEngine {
    /// 发送到解码线程的控制通道
    tx: mpsc::UnboundedSender<DecoderMessage>,
    /// 播放状态（原子操作，无锁）
    state: Arc<AtomicU64>,
    /// 当前音量 (0.0 - 1.0)
    volume: Arc<AtomicU64>,
    /// 当前播放位置（毫秒）
    position_ms: Arc<AtomicU64>,
    /// 总时长（毫秒）
    duration_ms: Arc<AtomicU64>,
    /// 是否已加载文件
    has_file: Arc<AtomicBool>,
    /// CPAL 流（可选，因为可能在初始化时失败）
    _stream: Option<Stream>,
}

impl AudioEngine {
    /// 创建新的音频引擎实例
    pub async fn new() -> Result<Self> {
        let config = AudioEngineConfig::default();
        
        // 创建控制通道
        let (tx, rx) = mpsc::unbounded_channel::<DecoderMessage>();
        
        // 创建共享状态
        let state = Arc::new(AtomicU64::new(PlayerState::Stopped as u64));
        let volume = Arc::new(AtomicU64::new((1.0 * 1000.0) as u64)); // 用整数避免浮点原子操作问题
        let position_ms = Arc::new(AtomicU64::new(0));
        let duration_ms = Arc::new(AtomicU64::new(0));
        let has_file = Arc::new(AtomicBool::new(false));
        
        // 获取默认音频输出设备
        let host = cpal::default_host();
        let device = host.default_output_device()
            .context("未找到默认音频输出设备")?;
        
        // 获取设备支持的配置
        let supported_config = device.default_output_config()
            .context("无法获取设备默认配置")?;
        
        let target_sample_rate = supported_config.sample_rate().0;
        let channels = supported_config.channels().count() as u16;
        let sample_format = supported_config.sample_format();
        
        // 计算环状缓冲区大小
        // 对于立体声 32-bit float，每秒需要 8 * sample_rate 字节
        let buffer_size_frames = (target_sample_rate as f32 * config.buffer_duration_secs) as usize;
        let buffer_capacity = buffer_size_frames * channels as usize;
        
        // 创建环状缓冲区 (使用 HeapRb 支持动态分配)
        let rb = Arc::new(HeapRb::<f32>::new(buffer_capacity));
        let producer = rb.producer();
        let consumer = rb.consumer();
        
        // 克隆共享状态供解码线程使用
        let decoder_state = Arc::clone(&state);
        let decoder_volume = Arc::clone(&volume);
        let decoder_position = Arc::clone(&position_ms);
        let decoder_duration = Arc::clone(&duration_ms);
        let decoder_has_file = Arc::clone(&has_file);
        
        // 启动解码线程（使用 Tokio 的 spawn_blocking）
        let decoder_handle = tokio::task::spawn_blocking(move || {
            run_decoder_thread(
                rx,
                producer,
                decoder_state,
                decoder_volume,
                decoder_position,
                decoder_duration,
                decoder_has_file,
                target_sample_rate,
                channels,
            )
        });
        
        // 克隆共享状态供 CPAL 回调使用
        let callback_consumer = consumer;
        let callback_state = Arc::clone(&state);
        let callback_volume = Arc::clone(&volume);
        
        // 构建 CPAL 输出流
        let stream = build_cpal_stream(
            &device,
            &supported_config,
            callback_consumer,
            callback_state,
            callback_volume,
        )?;
        
        Ok(Self {
            tx,
            state,
            volume,
            position_ms,
            duration_ms,
            has_file,
            _stream: Some(stream),
        })
    }
    
    /// 加载音频文件
    pub fn load_file(&self, path: PathBuf) -> Result<()> {
        self.tx.send(DecoderMessage::LoadFile(path))?;
        Ok(())
    }
    
    /// 开始播放
    pub fn play(&self) -> Result<()> {
        if !self.has_file.load(Ordering::Relaxed) {
            return Ok(()); // 没有加载文件时忽略播放请求
        }
        self.tx.send(DecoderMessage::Play)?;
        self.state.store(PlayerState::Playing as u64, Ordering::Relaxed);
        Ok(())
    }
    
    /// 暂停播放
    pub fn pause(&self) -> Result<()> {
        self.tx.send(DecoderMessage::Pause)?;
        self.state.store(PlayerState::Paused as u64, Ordering::Relaxed);
        Ok(())
    }
    
    /// 停止播放
    pub fn stop(&self) -> Result<()> {
        self.tx.send(DecoderMessage::Stop)?;
        self.state.store(PlayerState::Stopped as u64, Ordering::Relaxed);
        self.position_ms.store(0, Ordering::Relaxed);
        Ok(())
    }
    
    /// 跳转到指定位置（毫秒）
    pub fn seek(&self, position_ms: u64) -> Result<()> {
        if !self.has_file.load(Ordering::Relaxed) {
            return Ok(());
        }
        self.tx.send(DecoderMessage::Seek(position_ms))?;
        Ok(())
    }
    
    /// 设置音量 (0.0 - 1.0)
    pub fn set_volume(&self, volume: f32) -> Result<()> {
        let clamped = volume.clamp(0.0, 1.0);
        self.volume.store((clamped * 1000.0) as u64, Ordering::Relaxed);
        self.tx.send(DecoderMessage::SetVolume(clamped))?;
        Ok(())
    }
    
    /// 获取当前播放状态
    pub fn get_state(&self) -> PlayerState {
        match self.state.load(Ordering::Relaxed) {
            0 => PlayerState::Stopped,
            1 => PlayerState::Playing,
            2 => PlayerState::Paused,
            _ => PlayerState::Stopped,
        }
    }
    
    /// 获取当前播放位置（毫秒）
    pub fn get_position_ms(&self) -> u64 {
        self.position_ms.load(Ordering::Relaxed)
    }
    
    /// 获取总时长（毫秒）
    pub fn get_duration_ms(&self) -> u64 {
        self.duration_ms.load(Ordering::Relaxed)
    }
    
    /// 检查是否已加载文件
    pub fn has_file(&self) -> bool {
        self.has_file.load(Ordering::Relaxed)
    }
}

/// 构建 CPAL 音频输出流
/// 
/// 关键设计：回调函数中仅从 ringbuf 读取数据，不做任何阻塞或加锁操作
/// 如果缓冲区为空，则填充静音数据
fn build_cpal_stream(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    mut consumer: ringbuf::Consumer<f32, HeapRb<f32>>,
    state: Arc<AtomicU64>,
    volume: Arc<AtomicU64>,
) -> Result<Stream> {
    let sample_format = config.sample_format();
    let num_channels = config.channels().count();
    
    // 获取当前音量
    let current_volume = volume.load(Ordering::Relaxed) as f32 / 1000.0;
    
    let stream = device.build_output_stream(
        &config.into(),
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            // 只在播放状态下输出声音
            if state.load(Ordering::Relaxed) != PlayerState::Playing as u64 {
                // 填充静音
                data.fill(0.0);
                return;
            }
            
            // 从 ringbuf 读取数据
            // pop_slice 是无锁操作，返回实际读取的数量
            let read_count = consumer.pop_slice(data);
            
            // 如果读取的数据不足，剩余部分填充静音
            if read_count < data.len() {
                data[read_count..].fill(0.0);
            }
            
            // 应用音量增益
            let vol = current_volume;
            for sample in data.iter_mut() {
                *sample *= vol;
            }
        },
        |err| {
            eprintln!("CPAL 音频流错误：{:?}", err);
        },
        None,
    )?;
    
    stream.play()?;
    Ok(stream)
}

/// 解码器线程主循环
/// 
/// 工作流程：
/// 1. 等待加载文件命令
/// 2. 使用 Symphonia 解码音频帧
/// 3. 使用 Rubato 进行采样率转换（如果需要）
/// 4. 将重采样后的数据推入 ringbuf
#[allow(clippy::too_many_arguments)]
fn run_decoder_thread(
    mut rx: mpsc::UnboundedReceiver<DecoderMessage>,
    mut producer: ringbuf::Producer<f32, HeapRb<f32>>,
    state: Arc<AtomicU64>,
    volume: Arc<AtomicU64>,
    position_ms: Arc<AtomicU64>,
    duration_ms: Arc<AtomicU64>,
    has_file: Arc<AtomicBool>,
    target_sample_rate: u32,
    num_channels: u16,
) {
    use symphonia::core::audio::{AudioBufferRef, Signal};
    
    let mut format_reader: Option<Box<dyn FormatReader>> = None;
    let mut decoder: Option<Box<dyn symphonia::core::codecs::Decoder>> = None;
    let mut resampler: Option<SincFixedIn<f32>> = None;
    let mut current_track_id: u32 = 0;
    
    // 临时缓冲区，用于存储解码后的样本
    let mut decode_buffer: Vec<f32> = Vec::new();
    
    loop {
        // 非阻塞地接收命令
        if let Ok(msg) = rx.try_recv() {
            match msg {
                DecoderMessage::LoadFile(path) => {
                    // 加载新文件
                    match load_audio_file(&path) {
                        Ok((reader, track_id)) => {
                            format_reader = Some(reader);
                            current_track_id = track_id;
                            
                            // 初始化解码器
                            if let Some(ref mut reader) = format_reader {
                                if let Some(track) = reader.tracks().iter().find(|t| t.id == track_id) {
                                    let codec_params = &track.codec_params;
                                    
                                    // 获取源采样率
                                    let source_sample_rate = codec_params.sample_rate.unwrap_or(44100);
                                    
                                    // 初始化重采样器（如果采样率不匹配）
                                    if source_sample_rate != target_sample_rate {
                                        resampler = create_resampler(
                                            source_sample_rate,
                                            target_sample_rate,
                                            num_channels as usize,
                                        );
                                    } else {
                                        resampler = None;
                                    }
                                    
                                    // 创建解码器
                                    let decoder_opts = DecoderOptions::default();
                                    decoder = symphonia::core::codecs::CODEC_REGISTRY
                                        .get(&codec_params.codec)
                                        .and_then(|c| c.make_decoder(codec_params, decoder_opts).ok());
                                    
                                    // 计算总时长
                                    if let Some(timebase) = codec_params.time_base {
                                        if let Some(n_frames) = codec_params.n_frames {
                                            let duration = timebase.calc_time(n_frames).seconds;
                                            duration_ms.store((duration * 1000.0) as u64, Ordering::Relaxed);
                                        }
                                    }
                                    
                                    has_file.store(true, Ordering::Relaxed);
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("加载文件失败 {:?}: {}", path, e);
                            has_file.store(false, Ordering::Relaxed);
                        }
                    }
                }
                
                DecoderMessage::Play => {
                    state.store(PlayerState::Playing as u64, Ordering::Relaxed);
                }
                
                DecoderMessage::Pause => {
                    state.store(PlayerState::Paused as u64, Ordering::Relaxed);
                }
                
                DecoderMessage::Stop => {
                    state.store(PlayerState::Stopped as u64, Ordering::Relaxed);
                    position_ms.store(0, Ordering::Relaxed);
                    
                    // 清空缓冲区
                    let buf_len = producer.buf().len();
                    let temp: Vec<f32> = vec![0.0; buf_len];
                    let _ = producer.push_slice(&temp);
                }
                
                DecoderMessage::Seek(ms) => {
                    if let Some(ref mut reader) = format_reader {
                        // 转换为对称尼亚的时间单位
                        if let Some(track) = reader.tracks().iter().find(|t| t.id == current_track_id) {
                            if let Some(timebase) = track.codec_params.time_base {
                                let time = timebase.calc_time((ms as f64) / 1000.0);
                                
                                // 执行跳转
                                match reader.seek(
                                    SeekMode::Coarse,
                                    SeekTo::Time { time, track_id: Some(current_track_id) },
                                ) {
                                    Ok(seeked_to) => {
                                        // 更新精确位置
                                        if let Some(ts) = seeked_to.required_ts {
                                            let actual_time = timebase.calc_time(ts);
                                            position_ms.store((actual_time.seconds * 1000.0) as u64, Ordering::Relaxed);
                                        }
                                        
                                        // 清空缓冲区以避免旧数据
                                        let buf_len = producer.buf().len();
                                        let temp: Vec<f32> = vec![0.0; buf_len];
                                        let _ = producer.push_slice(&temp);
                                    }
                                    Err(e) => {
                                        eprintln!("跳转失败：{:?}", e);
                                    }
                                }
                            }
                        }
                    }
                }
                
                DecoderMessage::SetVolume(_) => {
                    // 音量在回调中直接读取原子值，这里只需确认收到命令
                }
            }
        }
        
        // 如果正在播放且有格式读取器，继续解码
        if state.load(Ordering::Relaxed) == PlayerState::Playing as u64 {
            if let (Some(ref mut reader), Some(ref mut decoder)) = (&mut format_reader, &mut decoder) {
                // 读取下一帧
                match reader.next_packet() {
                    Ok(packet) => {
                        // 只解码当前音轨
                        if packet.track_id() != current_track_id {
                            continue;
                        }
                        
                        // 解码音频帧
                        match decoder.decode(&packet) {
                            Ok(decoded) => {
                                // 更新播放位置
                                if let Some(timebase) = reader.default_track().unwrap().codec_params.time_base {
                                    let ts = packet.ts();
                                    let time = timebase.calc_time(ts);
                                    position_ms.store((time.seconds * 1000.0) as u64, Ordering::Relaxed);
                                }
                                
                                // 处理音频数据
                                process_audio_frame(
                                    &decoded,
                                    &mut decode_buffer,
                                    &mut producer,
                                    &mut resampler,
                                    target_sample_rate,
                                );
                            }
                            Err(e) => {
                                // 解码错误通常是可恢复的，继续下一帧
                                eprintln!("解码错误：{:?}", e);
                            }
                        }
                    }
                    Err(symphonia::core::errors::Error::IoError(_)) => {
                        // 文件结束，停止播放
                        state.store(PlayerState::Stopped as u64, Ordering::Relaxed);
                        has_file.store(false, Ordering::Relaxed);
                    }
                    Err(_) => {
                        // 其他错误，继续尝试
                    }
                }
            }
        }
        
        // 短暂休眠以避免忙等
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// 加载音频文件
fn load_audio_file(path: &PathBuf) -> Result<(Box<dyn FormatReader>, u32)> {
    let file = File::open(path)
        .with_context(|| format!("无法打开文件 {:?}", path))?;
    
    let mss = MediaSourceStream::new(Box::new(file));
    
    // 创建探测提示
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    
    // 探测文件格式
    let probe = Probe::default();
    let format = probe.format(&mss, hint, &FormatOptions::default())
        .with_context(|| format!("无法识别音频格式 {:?}", path))?;
    
    // 找到第一个有音频的音轨
    let track_id = format.reader.tracks()
        .iter()
        .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        .map(|t| t.id)
        .context("文件中没有音频轨道")?;
    
    Ok((format.reader, track_id))
}

/// 创建 Rubato 重采样器
fn create_resampler(
    source_rate: u32,
    target_rate: u32,
    channels: usize,
) -> Option<SincFixedIn<f32>> {
    // 计算重采样比率
    let ratio = target_rate as f64 / source_rate as f64;
    
    // 重采样参数 - 高质量设置
    let params = SincInterpolationParameters {
        sinc_len: 256,           // 较长的滤波器长度，提高质量
        f_cutoff: 0.95,          // 截止频率
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 256, // 高过采样率
        window: WindowFunction::BlackmanHarris2, // 优质窗函数
    };
    
    match SincFixedIn::<f32>::new(
        ratio,
        0.95, // 过渡带宽度
        params,
        1024, // 输入块大小
        channels,
    ) {
        Ok(resampler) => Some(resampler),
        Err(e) => {
            eprintln!("创建重采样器失败：{:?}", e);
            None
        }
    }
}

/// 处理音频帧：解码 -> 重采样 -> 写入缓冲区
#[allow(clippy::too_many_arguments)]
fn process_audio_frame(
    decoded: &AudioBufferRef,
    decode_buffer: &mut Vec<f32>,
    producer: &mut ringbuf::Producer<f32, HeapRb<f32>>,
    resampler: &mut Option<SincFixedIn<f32>>,
    target_sample_rate: u32,
) {
    // 将解码后的音频转换为交错格式的 f32 向量
    decode_buffer.clear();
    
    match decoded {
        AudioBufferRef::F32(buf) => {
            let frames = buf.frames();
            let channels = buf.spec().channels.count();
            
            for i in 0..frames {
                for ch in 0..channels {
                    decode_buffer.push(buf.chan(ch)[i]);
                }
            }
        }
        AudioBufferRef::S16(buf) => {
            let frames = buf.frames();
            let channels = buf.spec().channels.count();
            
            for i in 0..frames {
                for ch in 0..channels {
                    decode_buffer.push(buf.chan(ch)[i] as f32 / 32768.0);
                }
            }
        }
        AudioBufferRef::S32(buf) => {
            let frames = buf.frames();
            let channels = buf.spec().channels.count();
            
            for i in 0..frames {
                for ch in 0..channels {
                    decode_buffer.push(buf.chan(ch)[i] as f32 / 2147483648.0);
                }
            }
        }
        AudioBufferRef::F64(buf) => {
            let frames = buf.frames();
            let channels = buf.spec().channels.count();
            
            for i in 0..frames {
                for ch in 0..channels {
                    decode_buffer.push(buf.chan(ch)[i] as f32);
                }
            }
        }
        _ => return, // 不支持的格式
    }
    
    // 如果需要重采样
    if let Some(ref mut resampler) = resampler {
        // Rubato 期望每个通道的数据是分开的
        let channels = decoded.spec().channels.count();
        let input_frames = decoded.frames();
        
        // 准备输入数据（按通道分离）
        let mut input_buffers: Vec<Vec<f32>> = vec![Vec::with_capacity(input_frames); channels];
        for (ch, buffer) in input_buffers.iter_mut().enumerate().take(channels) {
            match decoded {
                AudioBufferRef::F32(buf) => buffer.extend_from_slice(buf.chan(ch)),
                AudioBufferRef::S16(buf) => buffer.extend(buf.chan(ch).iter().map(|&s| s as f32 / 32768.0)),
                AudioBufferRef::S32(buf) => buffer.extend(buf.chan(ch).iter().map(|&s| s as f32 / 2147483648.0)),
                AudioBufferRef::F64(buf) => buffer.extend(buf.chan(ch).iter().map(|&s| s as f32)),
                _ => continue,
            }
        }
        
        // 执行重采样
        match resampler.process(&input_buffers, None) {
            Ok(output_buffers) => {
                // 将输出重新交错并写入 ringbuf
                let output_frames = output_buffers[0].len();
                for i in 0..output_frames {
                    for ch in 0..channels {
                        // 检查缓冲区是否有空间
                        if producer.is_full() {
                            // 如果满了，稍微等待
                            std::thread::sleep(std::time::Duration::from_micros(100));
                        }
                        if !producer.is_full() {
                            let _ = producer.push(output_buffers[ch][i]);
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("重采样错误：{:?}", e);
            }
        }
    } else {
        // 无需重采样，直接写入
        for &sample in decode_buffer.iter() {
            if producer.is_full() {
                // 如果缓冲区满了，丢弃最旧的数据或等待
                // 这里选择丢弃最旧的数据以保持实时性
                let _ = producer.pop();
            }
            let _ = producer.push(sample);
        }
    }
}
