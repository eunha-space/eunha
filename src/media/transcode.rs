//! Server-side video/gifv/audio processing via ffmpeg, mirroring Mastodon:
//! video and gifv are transcoded to MP4 (H.264/AAC, faststart, yuv420p), audio
//! to MP3, and a poster frame is extracted for the thumbnail. Requires `ffmpeg`
//! and `ffprobe` on PATH (installed in the runtime image).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::Value;
use tokio::process::Command;

pub struct Transcoded {
    pub bytes: Vec<u8>,
    pub ext: &'static str,
    pub content_type: &'static str,
    /// `original` metadata (width/height/duration/frame_rate/bitrate) for file_meta.
    pub meta: Value,
}

fn unique_temp(ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "eunha-av-{}-{}.{ext}",
        std::process::id(),
        crate::snowflake::next_id()
    ))
}

/// Transcode an uploaded video/gifv/audio file to Mastodon's serving format.
pub async fn transcode(src: &[u8], media_type: &str) -> anyhow::Result<Transcoded> {
    let src_path = unique_temp("src");
    tokio::fs::write(&src_path, src).await?;
    let res = transcode_inner(&src_path, media_type).await;
    let _ = tokio::fs::remove_file(&src_path).await;
    res
}

async fn transcode_inner(src: &Path, media_type: &str) -> anyhow::Result<Transcoded> {
    let probe = ffprobe(src).await.unwrap_or_default();
    let (ext, content_type): (&'static str, &'static str) = match media_type {
        "audio" => ("mp3", "audio/mpeg"),
        _ => ("mp4", "video/mp4"),
    };
    let out = unique_temp(ext);
    let src_s = src.to_string_lossy().to_string();
    let out_s = out.to_string_lossy().to_string();

    let mut args: Vec<&str> = vec!["-y", "-i", &src_s, "-loglevel", "fatal"];
    match media_type {
        "audio" => args.extend([
            "-vn",
            "-c:a",
            "libmp3lame",
            "-q:a",
            "2",
            "-map_metadata",
            "-1",
        ]),
        "gifv" => args.extend([
            "-movflags",
            "faststart",
            "-pix_fmt",
            "yuv420p",
            "-vf",
            "crop=floor(iw/2)*2:floor(ih/2)*2",
            "-c:v",
            "h264",
            "-an",
        ]),
        // video
        _ if video_passthrough(&probe) => args.extend([
            "-movflags",
            "faststart",
            "-map_metadata",
            "-1",
            "-c:v",
            "copy",
            "-c:a",
            "copy",
        ]),
        _ => args.extend([
            "-preset",
            "veryfast",
            "-movflags",
            "faststart",
            "-pix_fmt",
            "yuv420p",
            "-vf",
            "crop=floor(iw/2)*2:floor(ih/2)*2",
            "-c:v",
            "h264",
            "-c:a",
            "aac",
            "-b:a",
            "192k",
            "-map_metadata",
            "-1",
        ]),
    }
    args.push(&out_s);

    run("ffmpeg", &args).await?;
    let bytes = tokio::fs::read(&out).await?;
    let _ = tokio::fs::remove_file(&out).await;
    Ok(Transcoded {
        bytes,
        ext,
        content_type,
        meta: probe.original_meta(),
    })
}

/// The GIF branch of `Paperclip::LazyThumbnail`: `src` through `filter`, at
/// most 60 frames a second and 3,000 frames, in a 32-colour palette.
pub async fn gif(src: &[u8], filter: &str) -> anyhow::Result<Vec<u8>> {
    let src_path = unique_temp("gif");
    tokio::fs::write(&src_path, src).await?;
    let out = unique_temp("gif");
    let (src_s, out_s) = (
        src_path.to_string_lossy().to_string(),
        out.to_string_lossy().to_string(),
    );
    let filter = format!(
        "{filter},split[a][b];[a]palettegen=max_colors=32[p];[b][p]paletteuse=dither=bayer"
    );
    let res = run(
        "ffmpeg",
        &[
            "-nostdin",
            "-i",
            &src_s,
            "-map_metadata",
            "-1",
            "-fpsmax",
            "60",
            "-frames:v",
            "3000",
            "-filter_complex",
            &filter,
            "-loglevel",
            "fatal",
            "-y",
            &out_s,
        ],
    )
    .await;
    let _ = tokio::fs::remove_file(&src_path).await;
    let bytes = match res {
        Ok(()) => tokio::fs::read(&out).await.map_err(Into::into),
        Err(e) => Err(e),
    };
    let _ = tokio::fs::remove_file(&out).await;
    bytes
}

/// Extract the first frame as a PNG, for thumbnail/blurhash generation.
pub async fn extract_frame(src: &[u8]) -> anyhow::Result<Vec<u8>> {
    let src_path = unique_temp("src");
    tokio::fs::write(&src_path, src).await?;
    let out = unique_temp("png");
    let src_s = src_path.to_string_lossy().to_string();
    let out_s = out.to_string_lossy().to_string();
    let res = run(
        "ffmpeg",
        &[
            "-y",
            "-ss",
            "0",
            "-i",
            &src_s,
            "-loglevel",
            "fatal",
            "-frames:v",
            "1",
            "-f",
            "image2",
            "-c:v",
            "png",
            &out_s,
        ],
    )
    .await;
    let _ = tokio::fs::remove_file(&src_path).await;
    res?;
    let bytes = tokio::fs::read(&out).await?;
    let _ = tokio::fs::remove_file(&out).await;
    Ok(bytes)
}

async fn run(cmd: &str, args: &[&str]) -> anyhow::Result<()> {
    let output = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "{cmd} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

#[derive(Default)]
struct Probe {
    width: Option<i64>,
    height: Option<i64>,
    duration: Option<f64>,
    frame_rate: Option<f64>,
    bitrate: Option<i64>,
    vcodec: Option<String>,
    acodec: Option<String>,
    pix_fmt: Option<String>,
}

impl Probe {
    fn original_meta(&self) -> Value {
        let mut o = serde_json::Map::new();
        if let Some(w) = self.width {
            o.insert("width".into(), w.into());
        }
        if let Some(h) = self.height {
            o.insert("height".into(), h.into());
        }
        if let (Some(w), Some(h)) = (self.width, self.height) {
            o.insert("size".into(), format!("{w}x{h}").into());
            if h != 0 {
                o.insert("aspect".into(), (w as f64 / h as f64).into());
            }
        }
        if let Some(d) = self.duration {
            o.insert("duration".into(), d.into());
        }
        if let Some(f) = self.frame_rate {
            o.insert("frame_rate".into(), f.into());
        }
        if let Some(b) = self.bitrate {
            o.insert("bitrate".into(), b.into());
        }
        Value::Object(o)
    }
}

fn video_passthrough(p: &Probe) -> bool {
    p.vcodec.as_deref() == Some("h264")
        && matches!(p.acodec.as_deref(), Some("aac") | None)
        && p.pix_fmt
            .as_deref()
            .is_some_and(|f| f.starts_with("yuv420"))
}

fn parse_rational(s: &str) -> Option<f64> {
    let (n, d) = s.split_once('/')?;
    let n: f64 = n.parse().ok()?;
    let d: f64 = d.parse().ok()?;
    if d == 0.0 {
        None
    } else {
        Some(n / d)
    }
}

async fn ffprobe(src: &Path) -> anyhow::Result<Probe> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(src)
        .stdin(Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!("ffprobe failed: {}", output.status);
    }
    let json: Value = serde_json::from_slice(&output.stdout)?;
    let mut p = Probe::default();
    if let Some(streams) = json.get("streams").and_then(|v| v.as_array()) {
        for s in streams {
            match s.get("codec_type").and_then(|v| v.as_str()) {
                Some("video") => {
                    p.width = s.get("width").and_then(Value::as_i64);
                    p.height = s.get("height").and_then(Value::as_i64);
                    p.vcodec = s
                        .get("codec_name")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned);
                    p.pix_fmt = s.get("pix_fmt").and_then(|v| v.as_str()).map(str::to_owned);
                    p.frame_rate = s
                        .get("avg_frame_rate")
                        .and_then(|v| v.as_str())
                        .and_then(parse_rational);
                }
                Some("audio") => {
                    p.acodec = s
                        .get("codec_name")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned);
                }
                _ => {}
            }
        }
    }
    if let Some(fmt) = json.get("format") {
        p.duration = fmt
            .get("duration")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok());
        p.bitrate = fmt
            .get("bit_rate")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok());
    }
    Ok(p)
}
