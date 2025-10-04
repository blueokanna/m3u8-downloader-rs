# m3u8-downloader-rs

`m3u8_downloader` 是一个基于 Rust 的命令行工具，用于下载 HLS (M3U8) 流并将其转换为 MP4。支持多并发下载、AES-CBC 解密、GPU 加速转码，并可自定义视频/音频码率及保留临时文件。

***

## 主要功能

- 自动检测 Master/Media Playlist 并选择最佳变体流  
- 并发下载 TS 切片，可设置最大并发数  
- 支持 AES-128-CBC 加密切片解密  
- 合并 TS 切片为单个 `.ts` 文件  
- 检测 NVIDIA/AMD GPU 并启用硬件转码，否则使用 CPU  
- 使用 FFmpeg 将 `.ts` 转码为 `.mp4`，可自定义码率  
- 可选保留或删除临时 TS 文件  

***

## 安装与依赖

1. 安装 Rust 工具链  
2. 安装 FFmpeg 并添加到 `PATH` [请参考: https://www.ffmpeg.org/download.html#build-windows]
3. 在项目根目录执行：  
   ```bash
   cargo build --release
   ```
4. 可执行文件位于 `target/release/m3u8_downloader`

***

## 用法示例

```bash
m3u8_downloader \
  --url "https://example.com/stream/master.m3u8" \
  --concurrency 8 \
  --output "video.mp4" \
  --retries 3 \
  --video-bitrate 2000 \
  --audio-bitrate 128 \
  --keep-temp true
```

- `--url`：M3U8 地址或本地文件路径  
- `--concurrency`：最大并发下载任务数（默认 8）  
- `--output`：输出 MP4 文件路径（默认 `output.mp4`）  
- `--retries`：下载切片重试次数（默认 3）  
- `--video-bitrate`：视频码率 (kbps)，0 为自动（默认 0）  
- `--audio-bitrate`：音频码率 (kbps)，0 为自动（默认 0）  
- `--keep-temp`：保留中间 TS 文件（默认 false）  

***

## 代码结构与流程

### 1. 参数解析与日志初始化

- 使用 `clap::Parser` 定义 `Args` 结构体  
- 通过 `env_logger` 和 `log` 初始化日志级别  

### 2. FFmpeg 环境检查

```rust
async fn check_ffmpeg() -> Result<()> { … }
```
- 调用 `ffmpeg -version` 确认 FFmpeg 安装

### 3. 下载并解析 M3U8 播放列表

```rust
async fn download_playlist(url: &str) -> Result<Vec<u8>> { … }
```
- 构建带 HTTP 头的 `reqwest::Client`  
- GET 请求获取字节流  

```rust
let (_, playlist) = parse_playlist(&m3u8_content)?;
```
- 使用 `m3u8_rs` 解析 Master/Media Playlist

### 4. 选择变体流（Master Playlist）

- 根据带宽与分辨率选取最佳流  
- 递归下载对应 Media Playlist  

### 5. 下载与合并 TS 切片

```rust
async fn download_and_merge(
    playlist: MediaPlaylist,
    base_url: Option<Url>,
    args: &Args,
    output_file: &str,
    multi_progress: &MultiProgress,
) -> Result<()> { … }
```
- 创建进度条：下载 & 合并  
- （可选）获取并解析 AES-128-CBC 密钥与 IV  
- 并发下载每个切片，解密后写入临时 `.ts` 文件  
- 按序合并所有 `.ts` 到 `temp_merged.ts`  

### 6. 构建 HTTP 客户端

```rust
fn create_http_client() -> Result<Client> { … }
```
- 设置通用请求头与超时  

### 7. 加速类型检测

```rust
async fn detect_acceleration() -> Result<AccelType> { … }
```
- 调用 `ffmpeg -encoders` 检查 `h264_nvenc` / `h264_amf`  
- 返回 `AccelType::Nvidia`、`AMD` 或 `CPU`

### 8. 转码为 MP4

```rust
async fn convert_to_mp4(
    input_ts: &str,
    args: &Args,
    multi_progress: &MultiProgress,
) -> Result<()> { … }
```
- 根据 `AccelType` 构建 FFmpeg 参数：  
  - **NVIDIA**：`-hwaccel cuda` + `h264_nvenc`  
  - **AMD**：`h264_amf`  
  - **CPU**：`libx264`  
- 可自定义 `-b:v` / `-b:a`  
- 运行 FFmpeg，生成最终 MP4  

***

## 注意事项

- 确保 FFmpeg 版本支持 NVENC/AMF  
- 大文件下载建议增大 `--retries`  
- GPU 转码质量与速度依赖显卡与驱动  

***

## 常见问题

- **下载失败**：检查网络连接及重试次数  
- **解密失败**：确认 M3U8 切片使用 AES-128-CBC  
- **转码缓慢**：启用 GPU 加速或调低分辨率/码率  

***

欢迎在 GitHub 提交 issue 或 pull request！
