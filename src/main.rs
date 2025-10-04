use aes::Aes128;
use anyhow::{Context, Result, bail};
use block_modes::block_padding::Pkcs7;
use block_modes::{BlockMode, Cbc};
use clap::Parser;
use env_logger::Env;
use futures::stream::{self, StreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use log::{error, info, warn};
use m3u8_rs::{Playlist, parse_playlist};
use reqwest::{Client, header};
use std::{fs::File, io::Write, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use tokio::{fs, process::Command, sync::Mutex};
use url::Url;

type Aes128Cbc = Cbc<Aes128, Pkcs7>;
enum AccelType {
    Nvidia,
    AMD,
    CPU,
}

#[derive(Parser)]
#[command(name = "m3u8_downloader")]
#[clap(
    name = "hls2mp4",
    version = "1.0",
    about = "Download HLS and convert to MP4 with GPU"
)]
struct Args {
    /// M3U8 文件 URL
    #[arg(long)]
    url: String,

    /// 最大并发下载任务数
    #[arg(long, default_value = "8")]
    concurrency: usize,

    /// 输出文件路径（MP4格式）
    #[arg(long, default_value = "output.mp4")]
    output: PathBuf,

    /// 重试次数
    #[arg(long, default_value = "3")]
    retries: u8,

    /// 视频码率 (kbps)，0为自动选择
    #[arg(long, default_value = "0")]
    video_bitrate: u32,

    /// 音频码率 (kbps)，0为自动选择
    #[arg(long, default_value = "0")]
    audio_bitrate: u32,

    /// 是否保留临时TS文件
    #[arg(long, default_value = "false")]
    keep_temp: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();
    log::set_max_level(log::LevelFilter::Info);
    let args = Args::parse();

    // 创建多进度条管理器
    let multi_progress = MultiProgress::new();

    // 检查 FFmpeg
    let check_pb = multi_progress.add(ProgressBar::new_spinner());
    check_pb.set_style(
        ProgressStyle::with_template("{spinner:.green} {msg}")?
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    check_pb.set_message("检查 FFmpeg 环境...");
    check_pb.enable_steady_tick(Duration::from_millis(100));

    check_ffmpeg().await?;
    check_pb.finish_with_message("✅ FFmpeg 环境检查完成");

    info!("开始处理 M3U8 URL: {}", args.url);

    // 下载播放列表进度
    let download_pb = multi_progress.add(ProgressBar::new_spinner());
    download_pb.set_style(
        ProgressStyle::with_template("{spinner:.blue} {msg}")?
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    download_pb.set_message("下载 M3U8 播放列表...");
    download_pb.enable_steady_tick(Duration::from_millis(100));

    let m3u8_content = if args.url.starts_with("http") {
        download_playlist(&args.url).await?
    } else {
        fs::read(args.url.clone())
            .await
            .with_context(|| format!("无法读取文件: {}", args.url))?
    };

    let (_, playlist) =
        parse_playlist(&m3u8_content).map_err(|e| anyhow::anyhow!("解析 M3U8 失败: {:?}", e))?;

    download_pb.finish_with_message("✅ M3U8 播放列表解析完成");

    let base_url = if args.url.starts_with("http") {
        let mut url = Url::parse(&args.url)?;
        url.set_query(None);
        let mut path = url.path().to_string();
        if let Some(pos) = path.rfind('/') {
            path.truncate(pos + 1);
        }
        url.set_path(&path);
        Some(url)
    } else {
        None
    };

    // 处理不同类型的播放列表
    let temp_ts = "temp_merged.ts";
    match playlist {
        Playlist::MasterPlaylist(master) => {
            info!(
                "检测到 Master Playlist，共 {} 个变体流",
                master.variants.len()
            );
            let best = master
                .variants
                .iter()
                .max_by_key(|v| {
                    let resolution_score = v
                        .resolution
                        .as_ref()
                        .map(|r| r.width * r.height)
                        .unwrap_or(0);
                    (resolution_score, v.bandwidth)
                })
                .ok_or_else(|| anyhow::anyhow!("未找到可用变体流"))?;

            info!(
                "选择最佳流: 带宽 {} kbps, 分辨率 {:?}",
                best.bandwidth,
                best.resolution
                    .as_ref()
                    .map(|r| format!("{}x{}", r.width, r.height))
            );

            let media_url = if let Some(base) = &base_url {
                base.join(&best.uri)?
            } else {
                bail!("Master Playlist 需要网络 URL")
            };

            // 延长 media_content 的生命周期
            let media_content = download_playlist(media_url.as_str()).await?;
            let (_, media_pl) = parse_playlist(&media_content)
                .map_err(|e| anyhow::anyhow!("解析 m3u8 失败: {:?}", e))?;
            let media_pl = media_pl.clone();

            if let Playlist::MediaPlaylist(mp) = media_pl {
                download_and_merge(mp, base_url, &args, temp_ts, &multi_progress).await?;
            }
        }
        Playlist::MediaPlaylist(mp) => {
            info!("检测到 Media Playlist，共 {} 个切片", mp.segments.len());
            download_and_merge(mp, base_url, &args, temp_ts, &multi_progress).await?;
        }
    }

    convert_to_mp4(temp_ts, &args, &multi_progress).await?;

    if !args.keep_temp {
        let _ = fs::remove_file(temp_ts).await;
    }

    Ok(())
}

async fn download_playlist(url: &str) -> Result<Vec<u8>> {
    let mut headers = header::HeaderMap::new();
    headers.insert(header::USER_AGENT, header::HeaderValue::from_static(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
    ));
    headers.insert(header::ACCEPT, header::HeaderValue::from_static("*/*"));
    headers.insert(
        header::ACCEPT_LANGUAGE,
        header::HeaderValue::from_static("zh-CN,zh;q=0.9,en;q=0.8"),
    );

    if let Ok(parsed_url) = Url::parse(url) {
        if let Some(domain) = parsed_url.domain() {
            let referer = format!("https://{}/", domain);
            headers.insert(header::REFERER, header::HeaderValue::from_str(&referer)?);
        }
    }

    let client = Client::builder()
        .default_headers(headers)
        .timeout(Duration::from_secs(30))
        .build()?;

    let response = client.get(url).send().await?;

    if !response.status().is_success() {
        bail!("下载播放列表失败: HTTP {}", response.status());
    }

    let content = response.bytes().await?.to_vec();
    Ok(content)
}

async fn check_ffmpeg() -> Result<()> {
    let output = Command::new("ffmpeg")
        .arg("-version")
        .output()
        .await
        .context("FFmpeg 未找到，请确保已安装 FFmpeg 并添加到 PATH")?;

    if !output.status.success() {
        bail!("FFmpeg 执行失败");
    }

    Ok(())
}

async fn download_and_merge(
    playlist: m3u8_rs::MediaPlaylist,
    base_url: Option<Url>,
    args: &Args,
    output_file: &str,
    multi_progress: &MultiProgress,
) -> Result<()> {
    let segments = playlist.segments;
    let total = segments.len();

    // 创建下载进度条
    let download_pb = multi_progress.add(ProgressBar::new(total as u64));
    download_pb.set_style(
        ProgressStyle::with_template(
            "{msg} [{elapsed_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} ({percent}%) {eta}",
        )?
        .progress_chars("##-"),
    );
    download_pb.set_message("🔽 下载视频切片");

    // 处理加密密钥
    let key = segments
        .first()
        .and_then(|s| s.key.clone())
        .map(|k| {
            let key_url = if let Some(base) = &base_url {
                base.join(&k.uri.unwrap())?
            } else {
                Url::parse(&k.uri.unwrap())?
            };

            let bytes = futures::executor::block_on(async {
                let client = create_http_client().unwrap();
                client
                    .get(key_url)
                    .send()
                    .await?
                    .error_for_status()?
                    .bytes()
                    .await
            })?;

            let iv =
                hex::decode(k.iv.unwrap().trim_start_matches("0x")).context("IV hex 解析失败")?;

            Ok::<_, anyhow::Error>((bytes.to_vec(), iv))
        })
        .transpose()?;

    let sem = Arc::new(Semaphore::new(args.concurrency));
    let client = Arc::new(create_http_client()?);
    let completed = Arc::new(Mutex::new(0u64));

    let tasks = stream::iter(segments.into_iter().enumerate())
        .map(|(idx, seg)| {
            let seg_url = if let Some(base) = &base_url {
                base.join(&seg.uri).unwrap().to_string()
            } else {
                seg.uri.clone()
            };

            let client = client.clone();
            let sem = sem.clone();
            let key = key.clone();
            let retries = args.retries;
            let pb = download_pb.clone();
            let completed = completed.clone();

            tokio::spawn(async move {
                let _permit = sem.acquire().await;

                for attempt in 1..=retries {
                    match client.get(&seg_url).send().await {
                        Ok(resp) if resp.status().is_success() => {
                            let data = resp.bytes().await?;
                            let buf = if let Some((ref k, ref iv)) = key {
                                let cipher = Aes128Cbc::new_from_slices(k, iv)?;
                                cipher.decrypt_vec(&data)?
                            } else {
                                data.to_vec()
                            };

                            let tmp = format!("seg_{:05}.ts", idx);
                            fs::write(&tmp, &buf).await?;

                            // 更新进度条
                            let mut count = completed.lock().await;
                            *count += 1;
                            pb.set_position(*count);
                            pb.set_message(format!("🔽 下载视频切片 [{}/{}]", *count, total));

                            return Ok::<(), anyhow::Error>(());
                        }
                        Ok(r) => {
                            pb.set_message(format!("⚠️ 重试中... ({}/{})", attempt, retries));
                            warn!("第{}次尝试失败: {} HTTP {}", attempt, seg_url, r.status());
                        }
                        Err(e) => {
                            pb.set_message(format!("⚠️ 重试中... ({}/{})", attempt, retries));
                            warn!("第{}次请求错误: {} - {}", attempt, seg_url, e);
                        }
                    }
                    if attempt < retries {
                        tokio::time::sleep(Duration::from_millis(2000)).await;
                    }
                }
                bail!("重试{}次后仍无法下载: {}", retries, seg_url)
            })
        })
        .buffer_unordered(args.concurrency)
        .collect::<Vec<_>>()
        .await;

    for task in tasks {
        task??;
    }

    download_pb.finish_with_message("✅ 视频切片下载完成");
    let merge_pb = multi_progress.add(ProgressBar::new(total as u64));
    merge_pb.set_style(
        ProgressStyle::with_template(
            "{msg} [{elapsed_precise}] {bar:40.green} {pos:>7}/{len:7} ({percent}%)",
        )?
        .progress_chars("##-"),
    );
    merge_pb.set_message("🔗 合并视频切片");

    let mut output = File::create(output_file)?;
    for i in 0..total {
        let tmp = format!("seg_{:05}.ts", i);
        let chunk = fs::read(&tmp).await?;
        output.write_all(&chunk)?;
        let _ = fs::remove_file(&tmp).await;
        merge_pb.inc(1);
        merge_pb.set_message(format!("🔗 合并视频切片 [{}/{}]", i + 1, total));
    }

    merge_pb.finish_with_message("✅ 视频切片合并完成");
    Ok(())
}

fn create_http_client() -> Result<Client> {
    let mut headers = header::HeaderMap::new();
    headers.insert(
        header::USER_AGENT,
        header::HeaderValue::from_static(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
        ),
    );
    headers.insert(header::ACCEPT, header::HeaderValue::from_static("*/*"));

    Ok(Client::builder()
        .default_headers(headers)
        .timeout(Duration::from_secs(30))
        .build()?)
}

async fn detect_acceleration() -> Result<AccelType> {
    let output = Command::new("ffmpeg")
        .args(&["-hide_banner", "-encoders"])
        .output()
        .await
        .context("检测编码器失败")?;
    let list = String::from_utf8_lossy(&output.stdout);
    if list.contains("h264_nvenc") {
        Ok(AccelType::Nvidia)
    } else if list.contains("h264_amf") {
        Ok(AccelType::AMD)
    } else {
        Ok(AccelType::CPU)
    }
}

async fn convert_to_mp4(input_ts: &str, args: &Args, multi_progress: &MultiProgress) -> Result<()> {
    let convert_pb = multi_progress.add(ProgressBar::new_spinner());
    convert_pb.set_style(
        ProgressStyle::with_template("{spinner:.yellow} {msg}")?
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    convert_pb.set_message("开始转码为 MP4 的格式...");
    convert_pb.enable_steady_tick(Duration::from_millis(120));

    let accel = detect_acceleration().await?;
    let mut ffmpeg_args = vec!["-hide_banner", "-loglevel", "info"];
    match accel {
        AccelType::Nvidia => {
            info!("检测到 NVIDIA GPU，可用 NVENC 加速");
            ffmpeg_args.extend(&["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"]);
            ffmpeg_args.extend(&["-c:v", "h264_cuvid"]);
            ffmpeg_args.extend(&["-i", input_ts]);
            ffmpeg_args.extend(&["-c:a", "aac", "-b:a", "320k"]);
            ffmpeg_args.extend(&["-c:v", "h264_nvenc", "-preset", "p3", "-rc", "vbr"]);
        }
        AccelType::AMD => {
            info!("检测到 AMD GPU，可用 AMF 加速");
            ffmpeg_args.extend(&["-i", input_ts]);
            ffmpeg_args.extend(&["-c:a", "aac", "-b:a", "320k"]);
            ffmpeg_args.extend(&["-c:v", "h264_amf", "-rc", "vbr"]);
        }
        AccelType::CPU => {
            info!("未检测到支持的 GPU，使用 CPU (libx264)");
            ffmpeg_args.extend(&["-i", input_ts]);
            ffmpeg_args.extend(&["-c:a", "aac", "-b:a", "256k"]);
            ffmpeg_args.extend(&["-c:v", "libx264", "-preset", "medium"]);
        }
    }

    let video_bitrate_str;
    if args.video_bitrate > 0 {
        video_bitrate_str = format!("{}k", args.video_bitrate);
        ffmpeg_args.extend_from_slice(&["-b:v", &video_bitrate_str]);
    }

    let audio_bitrate_str;
    if args.audio_bitrate > 0 {
        audio_bitrate_str = format!("{}k", args.audio_bitrate);
        ffmpeg_args.extend_from_slice(&["-b:a", &audio_bitrate_str]);
    } else {
        ffmpeg_args.extend_from_slice(&["-b:a", "256k"]);
    }

    let output_path = args
        .output
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("输出路径包含无效字符"))?;
    ffmpeg_args.push(output_path);

    let output = Command::new("ffmpeg")
        .args(&ffmpeg_args)
        .output()
        .await
        .context("FFmpeg 转码失败")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        convert_pb.finish_with_message("❌ MP4 转码失败");
        error!("FFmpeg 错误输出:\n{}", stderr);
        bail!("MP4 转码失败");
    }

    convert_pb.finish_with_message("✅ MP4 转码完成");
    info!("🎉 下载完成，输出文件: {:?}", args.output);
    Ok(())
}
