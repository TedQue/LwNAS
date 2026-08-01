use anyhow::{Context, Result, anyhow};
use chrono::Local;
use image::{DynamicImage, ImageFormat};
use mime_guess;
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use std::fs::Metadata;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use uuid::Uuid;

const GB: u64 = 1024 * 1024 * 1024;
const MB: u64 = 1024 * 1024;
const KB: u64 = 1024;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FileEntryDesc {
    pub name: String,
    pub size: String,
    pub last_modified: String,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RootFileEntryDesc {
    pub entry: FileEntryDesc,
    pub permission: String,
}

const URI_PATH_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'&')
    .add(b'=')
    .add(b'+')
    .add(b'%')
    .add(b'@')
    .add(b'!')
    .add(b'$')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b',')
    .add(b';')
    .add(b':')
    .add(b'[')
    .add(b']')
    .add(b'{')
    .add(b'}')
    .add(b'|')
    .add(b'^')
    .add(b'\\');

pub fn encode_uri(path: &str) -> String {
    utf8_percent_encode(path, URI_PATH_SET).to_string()
}

pub fn decode_uri(path: &str) -> Result<String> {
    let decoded = percent_decode_str(path)
        .decode_utf8()
        .with_context(|| format!("failed to decode uri: {}", path))?;
    Ok(decoded.to_string())
}

// 类似 normalize_path 解析并移除 uri 中的 ./ ../
pub fn normalize_uri_path(uri: &str) -> Result<String> {
    let mut result = String::new();
    let mut start = 0;
    loop {
        if let Some(pos) = &uri[start..].find('/') {
            let end = start + pos + 1;
            let component = &uri[start..end];
            start = end;

            if component == "/" {
                // / 非重复追加
                if !result.ends_with("/") {
                    result.push_str("/");
                }
            } else if component == "./" {
                // ./ 忽略
            } else if component == "../" {
                // ../ 弹出一级
                if let Some(pos) = result.rfind('/') {
                    result.truncate(pos - 1);
                } else {
                    return Err(anyhow!("invalid ../ component"));
                }
            } else {
                result.push_str(component);
            }
        } else {
            result.push_str(&uri[start..]);
            start = uri.len();
        }

        if start >= uri.len() {
            break;
        }
    }
    Ok(result)
}

pub fn get_up_uri_path(path: &str) -> &str {
    let up = if path.ends_with("/") {
        &path[0..(path.len() - 1)]
    } else {
        path
    };

    if let Some(pos) = up.rfind('/') {
        &up[0..(pos + 1)]
    } else {
        up
    }
}

pub fn guess_mime_type<P: AsRef<Path>>(path: P) -> String {
    // mime_guess 无法识别音频
    if let Some(ext) = path.as_ref().extension() {
        if let Some(ext) = ext.to_str() {
            let audios = ["mp3", "wav", "m4a", "flac", "ogg", "aac"];
            if audios.iter().any(|x| x.eq_ignore_ascii_case(ext)) {
                return "audio/mpeg".to_string();
            }
        }
    }

    let mut content_type = mime_guess::from_path(path.as_ref())
        .first_or_octet_stream()
        .as_ref()
        .to_string();

    // 对文本类型添加 utf-8 编码
    if is_text(&content_type) {
        content_type.push_str("; charset=utf-8");
    }

    content_type
}

pub fn make_etag_from_metadata(metadata: &Metadata) -> String {
    let mtime = metadata
        .modified()
        .unwrap_or_else(|_| SystemTime::UNIX_EPOCH);

    let size = metadata.len();

    format!(
        "\"{:x}-{:x}\"",
        mtime
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        size
    )
}

pub fn make_unique_tmp_file_name<P: AsRef<Path>>(base: P) -> PathBuf {
    let mut tmp = base.as_ref().join(&Uuid::new_v4().to_string());
    let _ = tmp.add_extension("tmp");
    tmp
}

fn make_file_name_with<P: AsRef<Path>>(p: P, unique: &str) -> PathBuf {
    let ext = p.as_ref().extension();
    let mut p = p.as_ref().to_path_buf();

    // 移除原扩展名
    p.set_extension("");

    // 追加 unique 标记
    p.add_extension(unique);

    // 恢复原扩展名
    if let Some(ext) = ext {
        p.add_extension(ext);
    }

    p
}

pub fn make_unique_file_name_with_uuid<P: AsRef<Path>>(p: P) -> PathBuf {
    make_file_name_with(p, &Uuid::new_v4().to_string())
}

pub fn make_unique_file_name_with_timestamp<P: AsRef<Path>>(p: P) -> PathBuf {
    make_file_name_with(p, &Local::now().format("%Y%m%d_%H%M%S_%6f").to_string())
}

pub fn is_text(content_type: &str) -> bool {
    content_type.starts_with("text")
}

pub fn is_image(content_type: &str) -> bool {
    content_type.starts_with("image")
}

pub fn is_video(content_type: &str) -> bool {
    content_type.starts_with("video")
}

pub fn is_audio(content_type: &str) -> bool {
    content_type.starts_with("audio")
}

pub fn is_pdf(content_type: &str) -> bool {
    content_type.starts_with("application/pdf")
}

pub fn fmt_human_size(v: u64) -> String {
    if v >= GB {
        format!("{:.2} GB", v as f64 / GB as f64)
    } else if v >= MB {
        format!("{:.2} MB", v as f64 / MB as f64)
    } else if v >= KB {
        format!("{:.2} KB", v as f64 / KB as f64)
    } else {
        format!("{:} B", v)
    }
}

pub fn fmt_permission(writable: bool, deletable: bool) -> String {
    let mut perm = "r".to_string();
    perm.push_str(if writable { "w" } else { "-" });
    perm.push_str(if deletable { "x" } else { "-" });
    perm
}

// Range 闭区间表示 0 开始的字节序号范围,可能的格式:
// bytes=500-999 ==> 从第 501 到第 1000 字节
// bytes=-500 ==> 最后 500 个字节
// bytes=9500- ==> 从 9501 到最后一个字节
// 含有多段范围,如 bytes=0-499, 1000-1499 时返回并集
pub fn parse_range(range: &str, total_size: u64) -> Result<(u64, u64)> {
    // 移除 bytes=,失败则表示格式错误
    let range = range
        .strip_prefix("bytes=")
        .ok_or(anyhow!("invalid range \"{}\"", range))?;

    let mut s = total_size;
    let mut e = 0;

    for token in range.split(',') {
        let (sub_s, sub_e) = parse_single_range(token.trim(), total_size)?;
        if sub_s < s {
            s = sub_s;
        }
        if sub_e > e {
            e = sub_e;
        }
    }
    Ok((s, e))
}

// 解析单个 range 的工具函数,输入 str 已经不含 bytes=
fn parse_single_range(range: &str, total_size: u64) -> Result<(u64, u64)> {
    let p = range
        .find('-')
        .ok_or(anyhow!("invalid range, \"-\" not found"))?;

    let (s, e) = {
        if p == range.len() - 1 {
            // 9500-
            (range[0..p].parse::<u64>()?, total_size - 1)
        } else if p == 0 {
            // -500
            let c = range[1..].parse::<u64>()?;
            (total_size - c, total_size - 1)
        } else {
            // 500-999
            (
                range[0..p].parse::<u64>()?,
                range[(p + 1)..].parse::<u64>()?,
            )
        }
    };

    // 校验结果
    if s >= total_size || e >= total_size || s >= e {
        Err(anyhow!("invalid range: {} - {}", s, e))
    } else {
        Ok((s, e))
    }
}

pub fn generate_thumbnail<P: AsRef<Path>>(path: P, max_size: u32) -> Result<Vec<u8>> {
    let img = image::open(path)?;
    let (width, height) = (img.width(), img.height());

    let f = |img: &DynamicImage, format: ImageFormat| -> Result<Vec<u8>> {
        let mut buffer = Cursor::new(Vec::new());
        img.write_to(&mut buffer, format)?;
        Ok(buffer.into_inner())
    };

    // 如果原图已经小于最大尺寸，直接返回
    if width <= max_size && height <= max_size {
        f(&img, ImageFormat::Png)
    } else {
        f(&img.thumbnail(max_size, max_size), ImageFormat::Png)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok() {
        assert_eq!(parse_range("bytes=9500-", 10000).unwrap(), (9500, 9999));
        assert_eq!(parse_range("bytes=-500", 10000).unwrap(), (9500, 9999));
        assert_eq!(parse_range("bytes=500-1000", 10000).unwrap(), (500, 1000));

        assert_eq!(
            parse_range("bytes=500-1000, 1001-2000", 10000).unwrap(),
            (500, 2000)
        );
        assert_eq!(
            parse_range("bytes=500-1000, 0-2000", 10000).unwrap(),
            (0, 2000)
        );
        assert_eq!(
            parse_range("bytes=200-1000, 500-2000", 10000).unwrap(),
            (200, 2000)
        );
    }

    #[test]
    #[should_panic]
    fn panic1() {
        // 缺少 -
        parse_range("bytes=9500", 10000).unwrap();
    }

    #[test]
    #[should_panic]
    fn panic2() {
        // 非 bytes= 先导
        parse_range("ytes=500-1000", 10000).unwrap();
    }

    #[test]
    #[should_panic]
    fn panic3() {
        // 范围不正确
        parse_range("bytes=0-10000", 10000).unwrap();
    }

    #[test]
    #[should_panic]
    fn panic4() {
        // 范围不正确
        parse_range("bytes=0-0", 10000).unwrap();
    }

    // cargo test make_file_name -- --show-output
    #[test]
    fn make_file_name() {
        println!(
            "{}",
            make_unique_file_name_with_uuid(Path::new("/abc.jpg")).display()
        );
        println!(
            "{}",
            make_unique_file_name_with_uuid(Path::new("/xyz/")).display()
        );
        println!(
            "{}",
            make_unique_file_name_with_timestamp(Path::new("abc/def")).display()
        );
    }

    #[test]
    fn test_normalize_uri_path() {
        for s in [
            "",
            "/a",
            "/a/b",
            "/a/b/",
            "/a/./../b/",
            "a/./b/../c",
            "a/../b",
            "///a/..abc./zip../b/",
            "a/../../b",
        ] {
            println!("{s} => {:?}", normalize_uri_path(s));
        }
    }

    #[test]
    fn test_uri_path_up() {
        assert_eq!(get_up_uri_path("/"), "");
        assert_eq!(get_up_uri_path("/a"), "/");
        assert_eq!(get_up_uri_path("/a/b/"), "/a/");
    }
}
