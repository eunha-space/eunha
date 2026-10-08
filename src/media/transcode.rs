//! Server-side video, gifv and audio processing via ffmpeg, mirroring
//! Mastodon's `Paperclip::Transcoder`, `ImageExtractor` and the GIF branch of
//! `LazyThumbnail`: video and gifv are transcoded to MP4 (H.264/AAC,
//! faststart, yuv420p), audio to MP3, a video's small style is its first
//! frame as a PNG, and an audio file's cover art becomes its thumbnail.
//! Requires `ffmpeg` and `ffprobe` on PATH (installed in the runtime image).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::{json, Value};
use tokio::process::Command;

/// What a transcode wrote.
pub struct Transcoded {
    pub bytes: Vec<u8>,
    pub ext: &'static str,
    pub content_type: &'static str,
}

/// A temporary file that is removed when dropped.
struct TempFile(PathBuf);

impl TempFile {
    fn new(ext: &str) -> Self {
        Self(std::env::temp_dir().join(format!(
            "eunha-av-{}-{}.{ext}",
            std::process::id(),
            crate::snowflake::next_id()
        )))
    }

    async fn with(data: &[u8], ext: &str) -> anyhow::Result<Self> {
        let file = Self::new(ext);
        tokio::fs::write(&file.0, data).await?;
        Ok(file)
    }

    fn arg(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// `VideoMetadataExtractor`: what `ffprobe` says about a file.
#[derive(Clone, Debug, Default)]
pub struct VideoMetadata {
    valid: bool,
    /// `[:format][:duration].to_f` and `[:format][:bit_rate].to_i`, when
    /// there is a format.
    pub duration: Option<f64>,
    pub bitrate: Option<i64>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub colorspace: Option<String>,
    /// Swapped when the stream is turned a quarter.
    pub width: Option<i64>,
    pub height: Option<i64>,
    /// `avg_frame_rate`, or `r_frame_rate` when it is `0/0`, reduced.
    pub frame_rate: Option<(i64, i64)>,
    pub r_frame_rate: Option<(i64, i64)>,
}

impl VideoMetadata {
    pub fn valid(&self) -> bool {
        self.valid
    }

    /// `movie.frame_rate.floor`.
    pub fn frame_rate_floor(&self) -> Option<i64> {
        self.frame_rate.map(|(n, d)| n.div_euclid(d))
    }

    /// `MediaAttachment#video_metadata`: `{}` when the file could not be
    /// read; otherwise what is known of width, height, frame rate (as Ruby
    /// writes a `Rational`, `"30/1"`), duration and bitrate.
    pub fn meta(&self) -> Value {
        if !self.valid {
            return json!({});
        }
        let mut o = serde_json::Map::new();
        if let Some(w) = self.width {
            o.insert("width".into(), w.into());
        }
        if let Some(h) = self.height {
            o.insert("height".into(), h.into());
        }
        if let Some((n, d)) = self.frame_rate {
            o.insert("frame_rate".into(), format!("{n}/{d}").into());
        }
        if let Some(d) = self.duration {
            o.insert("duration".into(), d.into());
        }
        if let Some(b) = self.bitrate {
            o.insert("bitrate".into(), b.into());
        }
        Value::Object(o)
    }
}

/// `Rational(raw)`: `None` for a zero denominator.
fn parse_rational(s: &str) -> Option<(i64, i64)> {
    let (n, d) = s.split_once('/').unwrap_or((s, "1"));
    let n: i64 = n.trim().parse().ok()?;
    let d: i64 = d.trim().parse().ok()?;
    if d == 0 {
        return None;
    }
    let g = gcd(n.abs(), d.abs()).max(1);
    let sign = if d < 0 { -1 } else { 1 };
    Some((sign * n / g, sign * d / g))
}

fn gcd(a: i64, b: i64) -> i64 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

/// [`VideoMetadata`] for `data`.
pub async fn probe(data: &[u8]) -> VideoMetadata {
    match TempFile::with(data, "src").await {
        Ok(file) => probe_path(&file.0).await,
        Err(_) => VideoMetadata::default(),
    }
}

async fn probe_path(src: &Path) -> VideoMetadata {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            "-show_error",
        ])
        .arg(src)
        .stdin(Stdio::null())
        .output()
        .await;
    let Ok(output) = output else {
        return VideoMetadata::default();
    };
    if !output.status.success() {
        return VideoMetadata::default();
    }
    let Ok(json) = serde_json::from_slice::<Value>(&output.stdout) else {
        return VideoMetadata::default();
    };
    parse_probe(&json)
}

fn parse_probe(json: &Value) -> VideoMetadata {
    let mut p = VideoMetadata {
        valid: json.get("error").is_none(),
        ..Default::default()
    };
    if let Some(format) = json.get("format") {
        p.duration = Some(
            format
                .get("duration")
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0),
        );
        p.bitrate = Some(
            format
                .get("bit_rate")
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
        );
    }
    let streams = json
        .get("streams")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let of = |kind: &str| {
        streams
            .iter()
            .find(|s| s.get("codec_type").and_then(Value::as_str) == Some(kind))
    };
    let text = |s: &Value, key: &str| s.get(key).and_then(Value::as_str).map(str::to_owned);
    if let Some(video) = of("video") {
        p.video_codec = text(video, "codec_name");
        p.colorspace = text(video, "pix_fmt");
        p.width = video.get("width").and_then(Value::as_i64);
        p.height = video.get("height").and_then(Value::as_i64);
        p.frame_rate = text(video, "avg_frame_rate").and_then(|r| parse_rational(&r));
        p.r_frame_rate = text(video, "r_frame_rate").and_then(|r| parse_rational(&r));
        if p.frame_rate.is_none() {
            p.frame_rate = p.r_frame_rate;
        }
        let quarter_turn = video
            .get("side_data_list")
            .and_then(Value::as_array)
            .is_some_and(|list| {
                list.iter().any(|x| {
                    x.get("rotation")
                        .and_then(Value::as_f64)
                        .is_some_and(|r| r.abs() == 90.0)
                })
            });
        if quarter_turn {
            std::mem::swap(&mut p.width, &mut p.height);
        }
    }
    if let Some(audio) = of("audio") {
        p.audio_codec = text(audio, "codec_name");
    }
    p
}

/// `VIDEO_PASSTHROUGH_OPTIONS`: H.264 with AAC or no audio, in 4:2:0.
fn eligible_to_passthrough(p: &VideoMetadata) -> bool {
    p.video_codec.as_deref() == Some("h264")
        && matches!(p.audio_codec.as_deref(), Some("aac") | None)
        && matches!(p.colorspace.as_deref(), Some("yuv420p" | "yuvj420p"))
}

/// `Transcoder::BITS_PER_PIXEL`, H.264 "High".
const BITS_PER_PIXEL: f64 = 0.11;

/// The bitrate `Transcoder#make` gives a video it encodes: enough for its
/// size at 30 frames a second, but no more than fits `VIDEO_LIMIT` over its
/// duration beside the audio; and a variable frame rate for one faster than
/// 120 frames a second.
fn rate_options(metadata: &VideoMetadata) -> Vec<String> {
    let mut options = vec![];
    if let (Some(width), Some(height)) = (metadata.width, metadata.height) {
        let size_limit_in_bits = (crate::api::mastodon::media::VIDEO_LIMIT * 8) as f64;
        let desired = (width as f64 * height as f64 * 30.0 * BITS_PER_PIXEL).floor() as i64;
        let duration = metadata.duration.unwrap_or(0.0).max(1.0);
        let maximum = (size_limit_in_bits / duration).floor() as i64 - 192_000;
        let bitrate = desired.min(maximum);
        options.extend([
            "-b:v".to_owned(),
            bitrate.to_string(),
            "-maxrate".to_owned(),
            (bitrate + 192_000).to_string(),
            "-bufsize".to_owned(),
            (bitrate * 5).to_string(),
        ]);
    }
    if metadata.r_frame_rate.is_some_and(|(n, d)| n > 120 * d) {
        options.extend(["-fps_mode".to_owned(), "vfr".to_owned()]);
    }
    options
}

/// Transcode an uploaded video, gifv or audio file to Mastodon's serving
/// format: `VIDEO_FORMAT` (passed through when it already is one, unless it
/// is a converted GIF), or `AUDIO_STYLES[:original]`.
pub async fn transcode(
    src: &[u8],
    metadata: &VideoMetadata,
    audio: bool,
    from_gif: bool,
) -> anyhow::Result<Transcoded> {
    let input = TempFile::with(src, "src").await?;
    let (ext, content_type): (&'static str, &'static str) = if audio {
        ("mp3", "audio/mpeg")
    } else {
        ("mp4", "video/mp4")
    };
    let out = TempFile::new(ext);
    let (src_s, out_s) = (input.arg(), out.arg());

    let rate: Vec<String>;
    let mut args: Vec<&str> = vec!["-nostdin", "-y", "-i", &src_s, "-loglevel", "fatal"];
    if audio {
        args.extend(["-q:a", "2"]);
    } else if !from_gif && eligible_to_passthrough(metadata) {
        args.extend([
            "-map_metadata",
            "-1",
            "-movflags",
            "faststart",
            "-c:v",
            "copy",
            "-c:a",
            "copy",
        ]);
    } else {
        args.extend([
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
            "-frames:v",
            "36000",
        ]);
        rate = rate_options(metadata);
        args.extend(rate.iter().map(String::as_str));
    }
    args.push(&out_s);
    run("ffmpeg", &args).await?;
    Ok(Transcoded {
        bytes: tokio::fs::read(&out.0).await?,
        ext,
        content_type,
    })
}

/// `VIDEO_STYLES[:small]`: the first frame as a PNG, shrunk to fit 640×640.
pub async fn small_frame(src: &[u8]) -> anyhow::Result<Vec<u8>> {
    let input = TempFile::with(src, "src").await?;
    let out = TempFile::new("png");
    run(
        "ffmpeg",
        &[
            "-nostdin",
            "-ss",
            "0",
            "-i",
            &input.arg(),
            "-loglevel",
            "fatal",
            "-vf",
            "scale='min(640, iw):min(640, ih)':force_original_aspect_ratio=decrease",
            "-f",
            "image2",
            "-vframes",
            "1",
            "-y",
            &out.arg(),
        ],
    )
    .await?;
    Ok(tokio::fs::read(&out.0).await?)
}

/// `Paperclip::ImageExtractor`: an audio file's cover art as a PNG, or `None`
/// when it has none.
pub async fn cover_art(src: &[u8]) -> Option<Vec<u8>> {
    let input = TempFile::with(src, "src").await.ok()?;
    let out = TempFile::new("png");
    run(
        "ffmpeg",
        &[
            "-nostdin",
            "-i",
            &input.arg(),
            "-loglevel",
            "fatal",
            "-y",
            &out.arg(),
        ],
    )
    .await
    .ok()?;
    tokio::fs::read(&out.0)
        .await
        .ok()
        .filter(|bytes| !bytes.is_empty())
}

/// The GIF branch of `Paperclip::LazyThumbnail`: `src` through `filter`, at
/// most 60 frames a second and 3,000 frames, in a 32-colour palette.
pub async fn gif(src: &[u8], filter: &str) -> anyhow::Result<Vec<u8>> {
    let input = TempFile::with(src, "gif").await?;
    let out = TempFile::new("gif");
    let filter = format!(
        "{filter},split[a][b];[a]palettegen=max_colors=32[p];[b][p]paletteuse=dither=bayer"
    );
    run(
        "ffmpeg",
        &[
            "-nostdin",
            "-i",
            &input.arg(),
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
            &out.arg(),
        ],
    )
    .await?;
    Ok(tokio::fs::read(&out.0).await?)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_rates_are_reduced_rationals() {
        assert_eq!(parse_rational("60/2"), Some((30, 1)));
        assert_eq!(parse_rational("30000/1001"), Some((30000, 1001)));
        assert_eq!(parse_rational("0/0"), None);
    }

    #[test]
    fn video_metadata_reads_as_mastodon_stores_it() {
        let probe = parse_probe(&json!({
            "format": { "duration": "5.5", "bit_rate": "1200" },
            "streams": [
                { "codec_type": "video", "codec_name": "h264", "pix_fmt": "yuv420p",
                  "width": 1920, "height": 1080, "avg_frame_rate": "0/0",
                  "r_frame_rate": "60/1",
                  "side_data_list": [{ "rotation": -90 }] },
                { "codec_type": "audio", "codec_name": "aac" }
            ]
        }));
        assert_eq!(
            probe.meta(),
            json!({
                "width": 1080, "height": 1920, "frame_rate": "60/1",
                "duration": 5.5, "bitrate": 1200
            })
        );
        assert!(eligible_to_passthrough(&probe));
        assert_eq!(probe.frame_rate_floor(), Some(60));
    }

    #[test]
    fn an_encoded_video_gets_transcoders_bitrate() {
        let probe = parse_probe(&json!({
            "format": { "duration": "10.0", "bit_rate": "1" },
            "streams": [{ "codec_type": "video", "codec_name": "vp9",
                          "width": 1280, "height": 720,
                          "avg_frame_rate": "240/1", "r_frame_rate": "240/1" }]
        }));
        // 1280 × 720 × 30 × 0.11, under the limit's 83,047,219 b/s.
        assert_eq!(
            rate_options(&probe),
            [
                "-b:v",
                "3041280",
                "-maxrate",
                "3233280",
                "-bufsize",
                "15206400",
                "-fps_mode",
                "vfr"
            ]
        );
    }

    #[test]
    fn an_audio_file_has_no_dimensions() {
        let probe = parse_probe(&json!({
            "format": { "duration": "3.0", "bit_rate": "128000" },
            "streams": [{ "codec_type": "audio", "codec_name": "mp3" }]
        }));
        assert_eq!(probe.meta(), json!({ "duration": 3.0, "bitrate": 128000 }));
    }
}
