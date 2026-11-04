use crate::document::{error, Error, Format, Limits, PageObserver, PageRange, Source};
use encoding_rs::Encoding;
use reqwest::{blocking::Client, redirect::Policy, Url};
use scraper::{ElementRef, Html, Node, Selector};
use std::{
    fs::File,
    io::{Read, Write},
    net::{IpAddr, ToSocketAddrs},
    time::{Duration, Instant},
};

pub(crate) struct Acquired {
    pub requested: String,
    pub final_source: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}
pub(crate) struct Extracted {
    pub text: String,
    pub format: Format,
    pub pages: Vec<PageRange>,
    pub warnings: Vec<String>,
    pub charset: Option<String>,
}

fn read_limited(reader: impl Read, max: usize) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    reader
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(error)?;
    if bytes.len() > max {
        return Err(error("source byte budget exceeded"));
    }
    Ok(bytes)
}

pub(crate) fn acquire(source: Source, limits: &Limits) -> Result<Acquired, Error> {
    match source {
        Source::Local(path) => {
            let file = File::open(&path).map_err(error)?;
            let meta = file.metadata().map_err(error)?;
            if !meta.is_file() {
                return Err(error("source is not a regular file"));
            }
            if meta.len() > limits.max_bytes as u64 {
                return Err(error("source byte budget exceeded"));
            }
            let bytes = read_limited(file, limits.max_bytes)?;
            let mime = if bytes.starts_with(b"%PDF-") {
                "application/pdf"
            } else {
                match path
                    .extension()
                    .and_then(|v| v.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "html" | "htm" => "text/html",
                    "pdf" => "application/pdf",
                    "txt" | "md" | "csv" => "text/plain",
                    _ => return Err(error("unsupported local format; use TXT/MD/CSV/HTML/PDF")),
                }
            };
            Ok(Acquired {
                requested: path.to_string_lossy().into(),
                final_source: path.canonicalize().map_err(error)?.to_string_lossy().into(),
                mime: mime.into(),
                bytes,
            })
        }
        Source::Remote(url) => fetch(&url, limits),
    }
}

fn validate_url(value: &str) -> Result<Url, Error> {
    let mut url = Url::parse(value).map_err(error)?;
    if url.scheme() != "https"
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
    {
        return Err(error(
            "remote source must be public HTTPS/443 without credentials",
        ));
    }
    url.set_fragment(None);
    // Reject literal addresses before DNS (including brackets on IPv6).
    if let Some(host) = url.host_str() {
        if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
            if !public_ip(ip) {
                return Err(error("remote address is not public"));
            }
        }
    }
    Ok(url)
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && (b == 168 || (b == 0 && (c == 0 || c == 2))))
                || (a == 192 && b == 88 && c == 99)
                || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(ip) => {
            // Restrict to ordinary global unicast, excluding transition,
            // documentation and special-purpose ranges.
            let s = ip.segments();
            s[0] & 0xe000 == 0x2000
                && s[0] != 0x2002
                && !(s[0] == 0x2001 && (s[1] <= 0x01ff || s[1] == 0x0db8))
                && !(s[0] == 0x3fff && s[1] <= 0x0fff)
        }
    }
}

fn remaining(start: Instant, budget: Duration) -> Result<Duration, Error> {
    budget
        .checked_sub(start.elapsed())
        .filter(|v| !v.is_zero())
        .ok_or_else(|| error("network deadline exceeded"))
}

fn fetch(requested: &str, limits: &Limits) -> Result<Acquired, Error> {
    let start = Instant::now();
    let mut url = validate_url(requested)?;
    for redirects in 0..=limits.max_redirects {
        let host = url
            .host_str()
            .ok_or_else(|| error("missing host"))?
            .trim_matches(['[', ']'])
            .to_string();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let lookup = host.clone();
        std::thread::spawn(move || {
            let result = (lookup.as_str(), 443)
                .to_socket_addrs()
                .map(|x| x.collect::<Vec<_>>());
            let _ = tx.send(result);
        });
        let addrs = rx
            .recv_timeout(remaining(start, limits.network_timeout)?)
            .map_err(error)?
            .map_err(error)?;
        if addrs.is_empty() || addrs.iter().any(|a| !public_ip(a.ip())) {
            return Err(error("DNS returned empty or non-public addresses"));
        }
        let client = Client::builder()
            .no_proxy()
            .https_only(true)
            .redirect(Policy::none())
            .timeout(remaining(start, limits.network_timeout)?)
            .connect_timeout(Duration::from_secs(10))
            .resolve_to_addrs(&host, &addrs)
            .user_agent("retrieval-engine/0.1 (on-demand public evidence)")
            .build()
            .map_err(error)?;
        let response = client
            .get(url.clone())
            .header("Accept-Encoding", "identity")
            .header("Accept", "text/html,application/pdf,text/plain")
            .send()
            .map_err(error)?;
        if response.status().is_redirection() {
            if redirects == limits.max_redirects {
                return Err(error("redirect budget exceeded"));
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .ok_or_else(|| error("redirect without Location"))?
                .to_str()
                .map_err(error)?;
            url = validate_url(url.join(location).map_err(error)?.as_str())?;
            continue;
        }
        if response.status() != reqwest::StatusCode::OK {
            return Err(error(format!("source HTTP status {}", response.status())));
        }
        if response
            .content_length()
            .is_some_and(|n| n > limits.max_bytes as u64)
        {
            return Err(error("source byte budget exceeded"));
        }
        let mime = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .ok_or_else(|| error("source omitted Content-Type"))?
            .to_str()
            .map_err(error)?
            .to_string();
        if response
            .headers()
            .get(reqwest::header::CONTENT_ENCODING)
            .is_some_and(|v| v != "identity")
        {
            return Err(error("compressed HTTP responses unsupported"));
        }
        let bytes = read_limited(response, limits.max_bytes)?;
        remaining(start, limits.network_timeout)?;
        return Ok(Acquired {
            requested: requested.into(),
            final_source: url.into(),
            mime,
            bytes,
        });
    }
    Err(error("redirect budget exceeded"))
}

pub(crate) fn extract(
    bytes: &[u8],
    mime: &str,
    limits: &Limits,
    observer: &mut PageObserver<'_>,
) -> Result<Extracted, Error> {
    let media = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let format = match media.as_str() {
        "application/pdf" => Format::Pdf,
        "text/html" | "application/xhtml+xml" => Format::Html,
        "text/plain" | "text/markdown" | "text/csv" => Format::Text,
        _ => return Err(error(format!("unsupported media type: {media}"))),
    };
    let mut warnings = Vec::new();
    let mut pages = Vec::new();
    let (text, charset) = if format == Format::Pdf {
        let (text, ranges, notes) = extract_pdf(bytes, limits, observer)?;
        pages = ranges;
        warnings = notes;
        (text, None)
    } else {
        let (decoded, charset) = decode(bytes, mime, format)?;
        let text = if format == Format::Html {
            warnings.push(
                "HTML text normalized; JavaScript/CSS and legal validity are not evaluated".into(),
            );
            let html = Html::parse_document(&decoded);
            let mut out = String::new();
            walk(
                html.root_element(),
                &mut out,
                0,
                limits.max_text_chars.saturating_mul(4),
            )?;
            normalize(&out)
        } else {
            decoded
        };
        (text, Some(charset))
    };
    if text.chars().count() > limits.max_text_chars {
        return Err(error("extracted text budget exceeded"));
    }
    if text.trim().is_empty() {
        return Err(error(if format == Format::Pdf {
            "PDF has no extractable text; OCR required"
        } else {
            "source has no extractable text"
        }));
    }
    if format != Format::Pdf {
        observer(&text, None, 0)?;
    }
    Ok(Extracted {
        text,
        format,
        pages,
        warnings,
        charset,
    })
}

fn charset_parameter(value: &str) -> Option<String> {
    value.split(';').skip(1).find_map(|item| {
        let (key, val) = item.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| val.trim().trim_matches(['\'', '"']).to_string())
    })
}
fn decode(bytes: &[u8], mime: &str, format: Format) -> Result<(String, String), Error> {
    let bom = Encoding::for_bom(bytes);
    let mut label = charset_parameter(mime);
    if label.is_none() && format == Format::Html {
        let prefix = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
        let doc = Html::parse_document(&prefix);
        let selector = Selector::parse("meta").map_err(error)?;
        for meta in doc.select(&selector) {
            if let Some(charset) = meta.value().attr("charset") {
                label = Some(charset.into());
                break;
            }
            if meta
                .value()
                .attr("http-equiv")
                .is_some_and(|x| x.eq_ignore_ascii_case("content-type"))
            {
                label = meta.value().attr("content").and_then(charset_parameter);
                if label.is_some() {
                    break;
                }
            }
        }
    }
    let (encoding, skip) = if let Some(bom) = bom {
        bom
    } else if let Some(label) = label {
        (
            Encoding::for_label(label.as_bytes())
                .ok_or_else(|| error(format!("unsupported charset: {label}")))?,
            0,
        )
    } else if format == Format::Html && std::str::from_utf8(bytes).is_err() {
        (encoding_rs::WINDOWS_1252, 0)
    } else {
        (encoding_rs::UTF_8, 0)
    };
    let (value, errors) = encoding.decode_without_bom_handling(&bytes[skip..]);
    if errors {
        return Err(error("source contains invalid encoded text"));
    }
    Ok((value.into_owned(), encoding.name().into()))
}

fn normalize(text: &str) -> String {
    text.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn walk(element: ElementRef<'_>, out: &mut String, depth: usize, max: usize) -> Result<(), Error> {
    if depth > 256 {
        return Err(error("HTML nesting budget exceeded"));
    }
    let name = element.value().name();
    if matches!(name, "script" | "style" | "head" | "noscript" | "template")
        || element.value().attr("hidden").is_some()
        || element
            .value()
            .attr("aria-hidden")
            .is_some_and(|x| x.eq_ignore_ascii_case("true"))
    {
        return Ok(());
    }
    let block = matches!(
        name,
        "p" | "div"
            | "section"
            | "article"
            | "br"
            | "hr"
            | "li"
            | "tr"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "pre"
            | "blockquote"
            | "table"
            | "header"
            | "footer"
    );
    let deleted = matches!(name, "del" | "s" | "strike");
    if block {
        out.push('\n');
    }
    if matches!(name, "td" | "th") {
        out.push(' ');
    }
    if deleted {
        out.push_str("[deleted]");
    }
    for child in element.children() {
        if let Node::Text(value) = child.value() {
            out.push_str(value);
        } else if let Some(element) = ElementRef::wrap(child) {
            walk(element, out, depth + 1, max)?;
        }
        if out.len() > max {
            return Err(error("HTML output budget exceeded"));
        }
    }
    if deleted {
        out.push_str("[/deleted]");
    }
    if block {
        out.push('\n');
    }
    Ok(())
}

struct CappedWriter {
    bytes: Vec<u8>,
    max: usize,
}
impl Write for CappedWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if data.len() > self.max.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("PDF text output budget exceeded"));
        }
        self.bytes.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn extract_pdf(
    bytes: &[u8],
    limits: &Limits,
    observer: &mut PageObserver<'_>,
) -> Result<(String, Vec<PageRange>, Vec<String>), Error> {
    // Use the page API directly: extract_text_from_mem_by_pages stops on the first
    // page error, which could otherwise silently turn a corrupt PDF into partial evidence.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let doc = pdf_extract::Document::load_mem(bytes).map_err(error)?;
        if doc.is_encrypted() {
            return Err(error(
                "encrypted PDF unsupported; provide an authorized decrypted source",
            ));
        }
        let all_pages = doc.get_pages();
        if all_pages.len() > limits.max_pages {
            return Err(error("PDF page budget exceeded"));
        }
        let mut text = String::new();
        let mut chars = 0;
        let mut pages = Vec::new();
        let mut warnings =
            vec!["PDF text reading order is heuristic; no OCR or layout fidelity guarantee".into()];
        for number in all_pages.keys() {
            let mut writer = CappedWriter {
                bytes: Vec::new(),
                max: limits
                    .max_text_chars
                    .saturating_sub(chars)
                    .saturating_mul(4),
            };
            let sink: &mut dyn Write = &mut writer;
            pdf_extract::output_doc_page(
                &doc,
                &mut pdf_extract::PlainTextOutput::new(sink),
                *number,
            )
            .map_err(|e| error(format!("PDF page {number} extraction failed: {e}")))?;
            let page = normalize(&String::from_utf8(writer.bytes).map_err(error)?);
            if !pages.is_empty() {
                text.push_str("\n\n");
                chars += 2;
            }
            let start = chars;
            chars += page.chars().count();
            if chars > limits.max_text_chars {
                return Err(error("extracted text budget exceeded"));
            }
            let has_text = !page.is_empty();
            if !has_text {
                warnings.push(format!(
                    "PDF page {number} has no extractable text; OCR may be required"
                ));
            }
            let range = PageRange {
                number: *number,
                start,
                end: chars,
                has_text,
            };
            observer(&page, Some(&range), all_pages.len())?;
            pages.push(range);
            text.push_str(&page);
        }
        Ok((text, pages, warnings))
    }))
    .map_err(|_| error("PDF parser panicked; source rejected"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn address_and_redirect_policy() {
        for addr in [
            "0.0.0.0",
            "100.64.0.1",
            "172.31.2.3",
            "198.18.1.1",
            "192.0.2.1",
            "203.0.113.1",
            "224.0.0.1",
            "::ffff:127.0.0.1",
            "2002:7f00:1::",
            "2001:db8::1",
            "fc00::1",
        ] {
            assert!(!public_ip(addr.parse().unwrap()), "{addr}");
        }
        for addr in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(public_ip(addr.parse().unwrap()));
        }
        let base = validate_url("https://example.com/path").unwrap();
        assert!(validate_url(base.join("https://127.0.0.1/secret").unwrap().as_str()).is_err());
    }
    #[test]
    fn stream_limit_without_content_length() {
        assert!(read_limited(b"12345".as_slice(), 4).is_err());
        assert_eq!(read_limited(b"1234".as_slice(), 4).unwrap(), b"1234");
    }
}
