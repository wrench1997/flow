//! A single byte range, as requested by media demuxers when seeking.
pub fn range(value: Option<&str>, len: u64) -> Result<(u64, u64, bool), ()> {
    let Some(value) = value else {
        return Ok((0, len, false));
    };
    let spec = value.strip_prefix("bytes=").ok_or(())?;
    if spec.contains(',') || len == 0 {
        return Err(());
    }
    let (start, end) = spec.split_once('-').ok_or(())?;
    if start.is_empty() {
        let count = end.parse::<u64>().map_err(|_| ())?.min(len);
        if count == 0 {
            return Err(());
        }
        return Ok((len - count, count, true));
    }
    let start = start.parse::<u64>().map_err(|_| ())?;
    let end = if end.is_empty() {
        len - 1
    } else {
        end.parse::<u64>().map_err(|_| ())?.min(len - 1)
    };
    if start >= len || end < start {
        return Err(());
    }
    Ok((start, end - start + 1, true))
}

pub fn playable(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|x| x.to_str())
        .is_some_and(|x| {
            matches!(
                x.to_ascii_lowercase().as_str(),
                "mp4"
                    | "mkv"
                    | "avi"
                    | "mov"
                    | "webm"
                    | "m4v"
                    | "ts"
                    | "m2ts"
                    | "wmv"
                    | "flv"
                    | "mpg"
                    | "mpeg"
                    | "mp3"
                    | "flac"
                    | "wav"
                    | "m4a"
                    | "aac"
                    | "ogg"
                    | "opus"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn media_ranges_cover_seek_tail_and_invalid_requests() {
        assert_eq!(range(None, 100), Ok((0, 100, false)));
        assert_eq!(range(Some("bytes=20-39"), 100), Ok((20, 20, true)));
        assert_eq!(range(Some("bytes=90-"), 100), Ok((90, 10, true)));
        assert_eq!(range(Some("bytes=-16"), 100), Ok((84, 16, true)));
        assert_eq!(range(Some("bytes=90-999"), 100), Ok((90, 10, true)));
        for value in [
            "bytes=100-",
            "bytes=20-10",
            "bytes=-0",
            "bytes=0-1,5-6",
            "items=0-1",
        ] {
            assert!(range(Some(value), 100).is_err());
        }
        assert_eq!(range(None, 0), Ok((0, 0, false)));
        assert!(range(Some("bytes=0-"), 0).is_err());
    }
}
