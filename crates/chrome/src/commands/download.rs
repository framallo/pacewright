use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::cdp::client::CdpClient;
use crate::cdp::types::EvaluateResult;

pub struct DownloadResult {
    pub path: String,
    pub bytes: usize,
    pub mime: String,
}

/// Error returned by [`run_native`] when the target isn't downloadable **yet** — a same-origin
/// preflight saw a 4xx/5xx (e.g. a render that hasn't finished). Distinct from a hard failure so
/// callers (the recipe `download` verb) can map it to a *retryable* outcome and poll again later.
#[derive(Debug)]
pub struct NotReady {
    pub status: i64,
}
impl std::fmt::Display for NotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "not ready: HTTP {} (target not downloadable yet)",
            self.status
        )
    }
}
impl std::error::Error for NotReady {}

/// Download `url` via a **native browser download** (CDP `Browser.setDownloadBehavior` + a real
/// navigation), so a cross-origin *signed redirect* — which an in-page `fetch` can't follow because
/// of CORS — is handled by Chrome itself, cookies/session intact. The bytes never pass through JS.
///
/// A same-origin preflight (`fetch(..., {redirect:'manual'})`) gates readiness: a `4xx`/`5xx` means
/// "not rendered yet" and returns [`NotReady`]; a `2xx` or an opaque redirect proceeds. Completion
/// is detected by polling an isolated per-call download dir until the `.crdownload` becomes final.
pub async fn run_native(
    client: &CdpClient,
    url: &str,
    out: Option<&str>,
    timeout_secs: u64,
) -> Result<DownloadResult, crate::BoxError> {
    // 1. Preflight: is the target downloadable yet? A signed redirect shows up as an opaque redirect
    //    (status 0, type "opaqueredirect"); a not-ready render answers 4xx on the same origin.
    let url_lit = serde_json::to_string(url)?;
    let preflight_js = format!(
        r"(async () => {{
            try {{
                const r = await fetch({url_lit}, {{ credentials: 'include', redirect: 'manual' }});
                return {{ status: r.status, type: r.type }};
            }} catch (e) {{ return {{ status: -1, type: 'error', err: String(e) }}; }}
        }})()"
    );
    let pf: EvaluateResult = client
        .call(
            "Runtime.evaluate",
            json!({ "expression": preflight_js, "returnByValue": true, "awaitPromise": true }),
        )
        .await?;
    if let Some(v) = pf.result.value {
        let status = v
            .get("status")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(-1);
        let kind = v
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        // Opaque redirect (signed URL) or any 2xx → downloadable. A 4xx/5xx → not ready yet.
        let ready = kind == "opaqueredirect" || (200..400).contains(&status);
        if !ready && status >= 400 {
            return Err(Box::new(NotReady { status }));
        }
        // status == -1 (a thrown fetch, e.g. the redirect target refused CORS) is inconclusive; a
        // native download still works, so fall through and let the download itself be the arbiter.
    }

    // 2. An isolated per-call dir so completion detection sees only our file.
    let home = dirs::home_dir().ok_or("Could not determine home directory")?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = home
        .join(".chrome-agent")
        .join("tmp")
        .join(format!("dl-{stamp}"));
    std::fs::create_dir_all(&dir)?;

    // 3. Point Chrome's downloads at that dir. Browser-level is current; fall back to the (deprecated)
    //    page-level command for older builds.
    let behavior = json!({ "behavior": "allow", "downloadPath": dir.to_string_lossy() });
    let set_browser: Result<serde_json::Value, _> = client
        .call("Browser.setDownloadBehavior", behavior.clone())
        .await;
    if set_browser.is_err() {
        let _: serde_json::Value = client
            .call("Page.setDownloadBehavior", behavior)
            .await
            .map_err(|e| format!("cannot set download behavior: {e}"))?;
    }

    // 4. Trigger it with a real navigation. Navigating to an attachment downloads it and aborts the
    //    navigation (net::ERR_ABORTED) while leaving the current page — so we ignore navigate errors.
    let _: Result<serde_json::Value, _> = client.call("Page.navigate", json!({ "url": url })).await;

    // 5. Poll the isolated dir until the `.crdownload` resolves to a finished file.
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let finished = loop {
        if let Some(p) = finished_download(&dir) {
            break p;
        }
        if Instant::now() >= deadline {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(
                format!("download did not complete within {timeout_secs}s for {url}").into(),
            );
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    };

    // 6. Move to the requested path (or leave it under the tmp tree with its real name).
    let bytes = std::fs::metadata(&finished)
        .map(|m| m.len() as usize)
        .unwrap_or(0);
    let dest = match out {
        Some(o) => PathBuf::from(o),
        None => home.join(".chrome-agent").join("tmp").join(
            finished
                .file_name()
                .map(std::ffi::OsStr::to_os_string)
                .unwrap_or_default(),
        ),
    };
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // rename() fails across filesystems; fall back to copy+remove.
    if std::fs::rename(&finished, &dest).is_err() {
        std::fs::copy(&finished, &dest)?;
        let _ = std::fs::remove_file(&finished);
    }
    let _ = std::fs::remove_dir_all(&dir);

    Ok(DownloadResult {
        path: dest.display().to_string(),
        bytes,
        mime: String::new(),
    })
}

/// The finished file in `dir`, if the download has completed: a non-`.crdownload` entry with no
/// `.crdownload` sibling still in flight. Returns `None` while a partial is present or the dir is empty.
fn finished_download(dir: &Path) -> Option<PathBuf> {
    let mut final_file = None;
    let mut has_partial = false;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(std::ffi::OsStr::to_str) == Some("crdownload") {
            has_partial = true;
        } else if path.is_file() {
            final_file = Some(path);
        }
    }
    if has_partial { None } else { final_file }
}

/// Download `url` by fetching it inside the page, so the request inherits the
/// page's cookies/session (auth-preserving). The bytes are returned as base64,
/// decoded, and written to disk.
///
/// Note: click-triggered/browser-native downloads are not handled here — resolve
/// the target href (e.g. `inspect --urls`) and pass it as the URL.
pub async fn run(
    client: &CdpClient,
    url: &str,
    out: Option<&str>,
    timeout_secs: u64,
) -> Result<DownloadResult, crate::BoxError> {
    let url_lit = serde_json::to_string(url)?;
    let js = format!(
        r"(async () => {{
            const res = await fetch({url_lit}, {{ credentials: 'include' }});
            if (!res.ok) throw new Error('HTTP ' + res.status + ' fetching ' + {url_lit});
            const buf = new Uint8Array(await res.arrayBuffer());
            let bin = '';
            const CHUNK = 0x8000;
            for (let i = 0; i < buf.length; i += CHUNK) {{
                bin += String.fromCharCode.apply(null, buf.subarray(i, i + CHUNK));
            }}
            return {{
                data: btoa(bin),
                mime: res.headers.get('content-type') || '',
                cd: res.headers.get('content-disposition') || '',
            }};
        }})()"
    );

    let eval: EvaluateResult = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        client.call(
            "Runtime.evaluate",
            json!({ "expression": js, "returnByValue": true, "awaitPromise": true }),
        ),
    )
    .await
    .map_err(|_| format!("download timed out after {timeout_secs}s fetching {url}"))??;

    if let Some(exc) = eval.exception_details {
        return Err(format!("download failed: {}", exc.text).into());
    }

    let obj = eval.result.value.ok_or("download: page returned no data")?;
    let data = obj
        .get("data")
        .and_then(|v| v.as_str())
        .ok_or("download: missing data")?;
    let mime = obj
        .get("mime")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let cd = obj.get("cd").and_then(|v| v.as_str()).unwrap_or("");

    let bytes = crate::base64::decode(data)?;

    let path = resolve_out_path(out, cd, url)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, &bytes)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    Ok(DownloadResult {
        path: path.display().to_string(),
        bytes: bytes.len(),
        mime,
    })
}

/// Resolve the destination path. `--out` (if given) is honoured verbatim as a
/// user-chosen path; otherwise the name is derived from the Content-Disposition
/// header, then the URL, then a fallback, and placed under `~/.chrome-agent/tmp`.
fn resolve_out_path(
    out: Option<&str>,
    content_disposition: &str,
    url: &str,
) -> Result<PathBuf, crate::BoxError> {
    if let Some(o) = out {
        return Ok(PathBuf::from(o));
    }
    let name = filename_from_content_disposition(content_disposition)
        .unwrap_or_else(|| filename_from_url(url));
    let home = dirs::home_dir().ok_or("Could not determine home directory")?;
    Ok(home.join(".chrome-agent").join("tmp").join(name))
}

/// Derive a filename from a URL's last path segment (query/fragment stripped).
///
/// Falls back to `"download"` when the URL has no path (host-only) or ends in a
/// slash — the host is never used as a filename.
#[must_use]
pub fn filename_from_url(url: &str) -> String {
    let no_query = url.split(['?', '#']).next().unwrap_or(url);
    // Drop the scheme so the host isn't mistaken for a path segment.
    let after_scheme = no_query
        .split_once("://")
        .map_or(no_query, |(_, rest)| rest);
    // Everything after the first '/' is the path; host-only URLs have none.
    let path = after_scheme.split_once('/').map_or("", |(_, p)| p);
    let last = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .trim();
    if last.is_empty() {
        "download".to_string()
    } else {
        sanitize_name(last)
    }
}

/// Extract a filename from a `Content-Disposition` header value.
///
/// Handles `filename="x"`, `filename=x`, and RFC 5987 `filename*=UTF-8''x`
/// (percent-decoding left to the caller's OS since names are typically ASCII).
#[must_use]
pub fn filename_from_content_disposition(header: &str) -> Option<String> {
    let lower = header.to_ascii_lowercase();
    // Prefer the extended form when present.
    if let Some(pos) = lower.find("filename*=") {
        let raw = &header[pos + "filename*=".len()..];
        let value = raw.split(';').next().unwrap_or(raw).trim();
        // filename*=UTF-8''actual%20name.pdf → take the part after the last "''".
        let name = value.rsplit("''").next().unwrap_or(value).trim_matches('"');
        let cleaned = sanitize_name(name);
        if !cleaned.is_empty() {
            return Some(cleaned);
        }
    }
    if let Some(pos) = lower.find("filename=") {
        let raw = &header[pos + "filename=".len()..];
        let value = raw
            .split(';')
            .next()
            .unwrap_or(raw)
            .trim()
            .trim_matches('"');
        let cleaned = sanitize_name(value);
        if !cleaned.is_empty() {
            return Some(cleaned);
        }
    }
    None
}

/// Strip any directory component so a server-supplied name can't traverse paths.
fn sanitize_name(name: &str) -> String {
    Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_filename_basic() {
        assert_eq!(
            filename_from_url("https://x.com/files/report.pdf"),
            "report.pdf"
        );
    }

    #[test]
    fn url_filename_strips_query_and_fragment() {
        assert_eq!(
            filename_from_url("https://x.com/a/b/data.csv?v=2&x=1"),
            "data.csv"
        );
        assert_eq!(filename_from_url("https://x.com/a/img.png#frag"), "img.png");
    }

    #[test]
    fn url_filename_trailing_slash_falls_back() {
        assert_eq!(filename_from_url("https://x.com/"), "download");
        assert_eq!(filename_from_url("https://x.com/dir/"), "dir");
    }

    #[test]
    fn url_filename_cannot_traverse() {
        // A crafted path segment must not escape the download dir.
        let n = filename_from_url("https://x.com/%2e%2e/etc/passwd");
        assert!(!n.contains('/'));
        assert_eq!(n, "passwd");
    }

    #[test]
    fn cd_quoted_filename() {
        assert_eq!(
            filename_from_content_disposition("attachment; filename=\"invoice 2024.pdf\""),
            Some("invoice 2024.pdf".to_string())
        );
    }

    #[test]
    fn cd_unquoted_filename() {
        assert_eq!(
            filename_from_content_disposition("attachment; filename=report.csv"),
            Some("report.csv".to_string())
        );
    }

    #[test]
    fn cd_extended_filename_preferred() {
        assert_eq!(
            filename_from_content_disposition(
                "attachment; filename=\"fallback.bin\"; filename*=UTF-8''real.pdf"
            ),
            Some("real.pdf".to_string())
        );
    }

    #[test]
    fn cd_filename_strips_path() {
        assert_eq!(
            filename_from_content_disposition("attachment; filename=\"../../etc/passwd\""),
            Some("passwd".to_string())
        );
    }

    #[test]
    fn cd_no_filename_returns_none() {
        assert_eq!(filename_from_content_disposition("inline"), None);
        assert_eq!(filename_from_content_disposition(""), None);
    }

    #[test]
    fn cd_key_is_case_insensitive() {
        // Real-world headers vary in case; the key match must not be case-sensitive.
        assert_eq!(
            filename_from_content_disposition("attachment; FileName=report.csv"),
            Some("report.csv".to_string())
        );
        assert_eq!(
            filename_from_content_disposition("attachment; FILENAME*=UTF-8''real.pdf"),
            Some("real.pdf".to_string())
        );
    }

    #[test]
    fn cd_empty_extended_falls_through_to_plain() {
        // filename* present but empty → must fall back to the plain filename=.
        assert_eq!(
            filename_from_content_disposition("attachment; filename*=UTF-8''; filename=plain.bin"),
            Some("plain.bin".to_string())
        );
    }

    #[test]
    fn cd_preserves_percent_escapes_literally() {
        // Contract: no percent-decoding — %2f must NOT become '/', or the
        // path-traversal guarantee would break. It stays a literal segment.
        let n =
            filename_from_content_disposition("attachment; filename*=UTF-8''a%2fb.pdf").unwrap();
        assert_eq!(n, "a%2fb.pdf");
        assert!(!n.contains('/'));
    }

    #[test]
    fn url_filename_host_only_no_slash() {
        // Exercises the split_once('/')→None branch (distinct from trailing-slash).
        assert_eq!(filename_from_url("https://x.com"), "download");
    }

    #[test]
    fn resolve_out_honours_explicit_path() {
        let p = resolve_out_path(Some("/tmp/mine.bin"), "", "https://x/y.pdf").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/mine.bin"));
    }

    #[test]
    fn resolve_out_prefers_cd_over_url() {
        let p = resolve_out_path(
            None,
            "attachment; filename=from-cd.pdf",
            "https://x/from-url.pdf",
        )
        .unwrap();
        assert!(p.ends_with("from-cd.pdf"));
    }

    #[test]
    fn resolve_out_falls_back_to_url() {
        let p = resolve_out_path(None, "inline", "https://x/from-url.pdf").unwrap();
        assert!(p.ends_with("from-url.pdf"));
    }
}
