//! LaTeX formula rendering (`[math]` config).
//!
//! Two passes around the normal render pipeline:
//!
//! - [`inject_markers`] rewrites math delimiters found in *text nodes*
//!   of already-sanitized HTML (`$…$`/`$$…$$`, `\(…\)`/`\[…\]`) into
//!   `<span data-yonder-math …>` placeholders carrying the
//!   (entity-escaped) LaTeX. `code`/`pre`/`a`/`script`/`style`/
//!   `textarea` subtrees and existing placeholders are skipped, so
//!   code samples and link text keep their literal `$…$`. It runs for
//!   every content type — Markdown, plain text and remote HTML — which
//!   also means `\(…\)` only survives where the source format doesn't
//!   eat the backslash (plain text and remote HTML; Markdown's `\(`
//!   escape collapses to `(`).
//! - [`materialize`] replaces each placeholder with an `<img>` of the
//!   rendered formula. The image goes through the normal media
//!   pipeline — a `media_attachments` row (`math_source` marks it for
//!   edit/Update reuse) plus a stored file — so the ActivityPub
//!   `attachment` list carries it for clients without inline-image
//!   support. Renderer failures leave the formula as literal text.
//!
//! The renderer is an administrator-configured command (no shell):
//! `<command>… inline|display`, LaTeX on stdin, PNG on stdout.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use tokio::io::AsyncWriteExt;

use super::render::decode_html_entities;
use crate::config::{Config, MathConfig};
use crate::media::attachments::PREFIX as MEDIA_PREFIX;
use crate::media::storage::MediaStore;
use crate::{Error, Result};

/// Elements whose text never holds post math — code samples, link
/// text (a formula inside an anchor stays literal), and raw-text
/// elements that sanitizing should have removed anyway.
const SKIP_TAGS: &[&str] = &["code", "pre", "a", "script", "style", "textarea"];

/// Elements that never have contents — never pushed on the skip stack.
const VOID_TAGS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// A rendered formula's public image plus the attachment bookkeeping
/// the caller needs.
struct Resolved {
    /// `media_attachments.id`.
    id: i64,
    /// Absolute public URL for the `<img src>`.
    url: String,
    /// Pixel dimensions for the `width`/`height` attributes.
    width: i32,
    height: i32,
    /// `true` when this call created the row (vs. reusing an existing
    /// math attachment on edit/re-ingest).
    is_new: bool,
}

/// `math_source` (`{mode}:{latex}`) → (media id, storage key, width,
/// height) for the edit/Update reuse lookup.
type ReuseMap = HashMap<String, (i64, String, Option<i32>, Option<i32>)>;

/// What [`materialize`] produced.
#[derive(Debug, Default)]
pub struct Materialized {
    /// HTML with placeholders replaced by `<img>` markup (or unwrapped
    /// to plain LaTeX text when rendering failed / the budget ran out).
    pub html: String,
    /// Rows created by this call — `status_id` is NULL; the caller
    /// attaches them (publish claims them via `media_ids`, edit and
    /// remote ingest UPDATE them inside their transaction).
    pub new_ids: Vec<i64>,
    /// Every math row the final HTML references (reused + new) — the
    /// caller detaches `math_source` rows not in this set.
    pub kept_ids: Vec<i64>,
}

// ---------------------------------------------------------------------------
// Marker injection
// ---------------------------------------------------------------------------

/// The placeholder span [`inject_markers`] emits and [`materialize`]
/// consumes. The LaTeX stays entity-escaped — it was already escaped
/// in the sanitized HTML text node it came from.
fn placeholder(mode: &str, src: &str) -> String {
    format!("<span data-yonder-math=\"{mode}\" data-yonder-math-src=\"{src}\">{src}</span>")
}

/// Entries on the skip stack while scanning tags: named skip elements,
/// or `MathSpan` for our own placeholders (keeps the pass idempotent).
#[derive(PartialEq)]
enum Skip {
    Tag(&'static str),
    MathSpan,
}

/// Rewrite math delimiters in the HTML's text nodes into
/// `data-yonder-math` placeholder spans. Runs post-sanitization, so
/// the emitted markup is trusted; delimiters inside skip subtrees and
/// inside existing placeholders are left alone.
pub(crate) fn inject_markers(html: &str, cfg: &MathConfig) -> String {
    if !cfg.enabled || cfg.command.trim().is_empty() {
        return html.to_string();
    }
    if !html.contains('$') && !html.contains("\\(") && !html.contains("\\[") {
        return html.to_string();
    }
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len());
    let mut skip: Vec<Skip> = Vec::new();
    let mut text_start = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if text_start < i {
            let text = &html[text_start..i];
            if skip.is_empty() {
                scan_text(text, cfg, &mut out);
            } else {
                out.push_str(text);
            }
        }
        // `>` can't appear raw inside an attribute value — upstream
        // sanitizing entity-escapes it — so the next `>` ends the tag.
        let Some(rel_end) = html[i..].find('>') else {
            out.push_str(&html[i..]);
            text_start = bytes.len();
            break;
        };
        let tag_end = i + rel_end + 1;
        let tag = &html[i..tag_end];
        handle_tag(tag, &mut skip);
        out.push_str(tag);
        i = tag_end;
        text_start = tag_end;
    }
    if text_start < bytes.len() {
        let text = &html[text_start..];
        if skip.is_empty() {
            scan_text(text, cfg, &mut out);
        } else {
            out.push_str(text);
        }
    }
    out
}

/// Update the skip stack for one raw tag (`<name …>`, `</name>`,
/// comments/declarations).
fn handle_tag(tag: &str, skip: &mut Vec<Skip>) {
    let body = match tag.strip_prefix("<!--") {
        Some(_) => return, // comments never contain post text
        None => match tag.strip_prefix('<').and_then(|t| t.strip_suffix('>')) {
            Some(b) => b.trim_end_matches('/'),
            None => return,
        },
    };
    if body.starts_with('!') || body.starts_with('?') {
        return;
    }
    let (closing, rest) = match body.strip_prefix('/') {
        Some(r) => (true, r),
        None => (false, body),
    };
    let name_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let name = rest[..name_end].to_ascii_lowercase();
    if name.is_empty() {
        return;
    }
    if closing {
        match skip.last() {
            Some(Skip::Tag(t)) if *t == name.as_str() => {
                skip.pop();
            }
            Some(Skip::MathSpan) if name == "span" => {
                skip.pop();
            }
            _ => {}
        }
        return;
    }
    if tag.ends_with("/>") || VOID_TAGS.contains(&name.as_str()) {
        return;
    }
    if let Some(t) = SKIP_TAGS.iter().find(|t| **t == name) {
        skip.push(Skip::Tag(match *t {
            "code" => "code",
            "pre" => "pre",
            "a" => "a",
            "script" => "script",
            "style" => "style",
            _ => "textarea",
        }));
    } else if name == "span" && tag.contains("data-yonder-math=") {
        skip.push(Skip::MathSpan);
    }
}

/// A delimiter match: rendering mode plus the content span (still
/// entity-escaped) and the position just past the closing delimiter.
struct Delim {
    display: bool,
    start: usize,
    end: usize,
    next: usize,
}

/// Scan one text node for math delimiters, appending literal text and
/// placeholder spans to `out`.
fn scan_text(text: &str, cfg: &MathConfig, out: &mut String) {
    let bytes = text.as_bytes();
    let mut pos = 0usize;
    let mut lit_start = 0usize;
    while pos < bytes.len() {
        // Only `$`/`\` can open a formula. Checking the raw byte first
        // keeps `pos` on a char boundary for the slices inside
        // `match_delim` — UTF-8 continuation bytes never equal these
        // ASCII values, so `pos` may sit mid-character safely as long
        // as we never slice there.
        let found = if matches!(bytes[pos], b'$' | b'\\') {
            match_delim(text, pos, cfg)
        } else {
            None
        };
        let Some(d) = found else {
            pos += 1;
            continue;
        };
        out.push_str(&text[lit_start..pos]);
        let content = text[d.start..d.end].trim();
        let mode = if d.display { "display" } else { "inline" };
        out.push_str(&placeholder(mode, content));
        pos = d.next;
        lit_start = pos;
    }
    out.push_str(&text[lit_start..]);
}

/// Try to match a math delimiter starting at `pos`. Returns `None`
/// when no valid formula opens here — the caller advances one byte.
fn match_delim(text: &str, pos: usize, cfg: &MathConfig) -> Option<Delim> {
    let rest = &text[pos..];
    let d = if let Some(stripped) = rest.strip_prefix("$$") {
        if escaped(text, pos) {
            return None;
        }
        let close = stripped.find("$$")? + pos + 2;
        Delim {
            display: true,
            start: pos + 2,
            end: close,
            next: close + 2,
        }
    } else if let Some(stripped) = rest.strip_prefix("\\(") {
        if escaped(text, pos) {
            return None;
        }
        let close = stripped.find("\\)")? + pos + 2;
        Delim {
            display: false,
            start: pos + 2,
            end: close,
            next: close + 2,
        }
    } else if let Some(stripped) = rest.strip_prefix("\\[") {
        if escaped(text, pos) {
            return None;
        }
        let close = stripped.find("\\]")? + pos + 2;
        Delim {
            display: true,
            start: pos + 2,
            end: close,
            next: close + 2,
        }
    } else if rest.starts_with('$') {
        inline_delim(text, pos, cfg)?
    } else {
        return None;
    };
    // Content must be non-empty and within the input cap (measured on
    // the decoded LaTeX — entities inflate the escaped form).
    let content = text[d.start..d.end].trim();
    if content.is_empty() {
        return None;
    }
    if decode_html_entities(content).chars().count() > cfg.max_input_chars {
        return None;
    }
    Some(d)
}

/// `$…$` inline math. MathJax-style guards keep currency text literal:
/// the opener can't be followed by whitespace or `$`, the closer can't
/// be preceded by whitespace, can't be followed by a digit, and can't
/// be `\$`-escaped.
fn inline_delim(text: &str, pos: usize, _cfg: &MathConfig) -> Option<Delim> {
    if escaped(text, pos) {
        return None;
    }
    let bytes = text.as_bytes();
    let next = bytes.get(pos + 1)?;
    if next.is_ascii_whitespace() || *next == b'$' {
        return None;
    }
    let mut j = pos + 1;
    while j < bytes.len() {
        if bytes[j] == b'$'
            && bytes.get(j + 1) != Some(&b'$')
            && !bytes[j - 1].is_ascii_whitespace()
            && !bytes.get(j + 1).is_some_and(|b| b.is_ascii_digit())
            && !escaped(text, j)
        {
            return Some(Delim {
                display: false,
                start: pos + 1,
                end: j,
                next: j + 1,
            });
        }
        j += 1;
    }
    None
}

/// `true` when the byte at `pos` is escaped by an odd-length run of
/// preceding backslashes (`\$` literal, `\\$` math).
fn escaped(text: &str, pos: usize) -> bool {
    let bytes = text.as_bytes();
    let mut n = 0usize;
    let mut i = pos;
    while i > 0 && bytes[i - 1] == b'\\' {
        n += 1;
        i -= 1;
    }
    n % 2 == 1
}

// ---------------------------------------------------------------------------
// Materialization
// ---------------------------------------------------------------------------

static MATH_SPAN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"<span data-yonder-math="(inline|display)" data-yonder-math-src="([^"]*)">([^<]*)</span>"#,
    )
    .unwrap()
});

/// Replace every `data-yonder-math` placeholder in `html` with the
/// rendered formula `<img>` (or unwrap it to delimited LaTeX text when
/// the formula failed or exceeded `max_formulas`).
///
/// `reuse_status_id` lets edits and remote Updates keep the existing
/// math attachments whose `math_source` still matches — identical
/// formulas keep their image URL instead of regenerating.
///
/// The number of rendered formulas is bounded by `math.max_formulas`
/// alone — formula images are inline content, not user uploads, so
/// they don't consume `media.max_attachments_per_post` slots (remote
/// ingest already worked this way).
#[allow(clippy::too_many_arguments)]
pub async fn materialize(
    db: &sqlx::PgPool,
    store: &MediaStore,
    config: &Config,
    base_url: &str,
    account_id: i64,
    reuse_status_id: Option<i64>,
    html: &str,
) -> Result<Materialized> {
    if !config.math.enabled || !html.contains("data-yonder-math") {
        return Ok(Materialized {
            html: html.to_string(),
            ..Default::default()
        });
    }
    struct Found {
        display: bool,
        latex: String,
        inner: String,
        range: std::ops::Range<usize>,
    }
    let mut found = Vec::new();
    for caps in MATH_SPAN_RE.captures_iter(html) {
        let whole = caps.get(0).expect("group 0");
        let latex = decode_html_entities(&caps[2]);
        if latex.trim().is_empty() {
            continue;
        }
        found.push(Found {
            display: &caps[1] == "display",
            latex,
            inner: caps[3].to_string(),
            range: whole.range(),
        });
    }
    if found.is_empty() {
        return Ok(Materialized {
            html: html.to_string(),
            ..Default::default()
        });
    }

    // Distinct formulas in first-seen order — repeats share one
    // render and one attachment row.
    let cfg = &config.math;
    let limit = cfg.max_formulas as usize;
    let mut distinct: Vec<(bool, String)> = Vec::new();
    for f in &found {
        let key = (f.display, f.latex.clone());
        if !distinct.contains(&key) {
            distinct.push(key);
        }
    }
    let render_set: Vec<(bool, String)> = distinct.iter().take(limit).cloned().collect();
    let allowed: HashSet<(bool, String)> = render_set.iter().cloned().collect();

    // Existing math rows for reuse (edit / remote Update). `math_source`
    // is `{mode}:{latex}` — the latex itself may contain ':' so the
    // whole column is the key.
    let mut reuse: ReuseMap = HashMap::new();
    if let Some(sid) = reuse_status_id {
        let rows = sqlx::query_as::<_, (i64, String, String, Option<i32>, Option<i32>)>(
            "SELECT id, math_source, file_path, width, height FROM media_attachments \
             WHERE status_id = $1 AND math_source IS NOT NULL AND file_path IS NOT NULL",
        )
        .bind(sid)
        .fetch_all(db)
        .await?;
        for (id, math_source, file_path, w, h) in rows {
            reuse.insert(math_source, (id, file_path, w, h));
        }
    }

    // Render every allowed distinct formula concurrently — failures
    // degrade to literal text, never fail the post.
    let resolved: Vec<Option<Resolved>> =
        futures_util::future::join_all(render_set.iter().map(|(display, latex)| {
            resolve_one(
                db, store, config, base_url, account_id, *display, latex, &reuse,
            )
        }))
        .await;
    let by_key: HashMap<(bool, String), Option<Resolved>> =
        render_set.iter().cloned().zip(resolved).collect();

    // Rebuild the HTML: resolved formulas get the image markup,
    // everything else unwraps back to the escaped LaTeX text.
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;
    let mut kept: Vec<i64> = Vec::new();
    let mut new_ids: Vec<i64> = Vec::new();
    for f in &found {
        out.push_str(&html[cursor..f.range.start]);
        cursor = f.range.end;
        let key = (f.display, f.latex.clone());
        let img = if allowed.contains(&key) {
            by_key.get(&key).and_then(|r| r.as_ref())
        } else {
            None
        };
        match img {
            Some(r) => {
                let mode = if f.display { "display" } else { "inline" };
                let alt: String = f.latex.chars().take(200).collect();
                // Fallback for clients that strip <img> (e.g. Mastodon):
                // a link to the rendered image carrying the delimited
                // LaTeX source. MathJax-enabled forks may typeset it
                // inline; Yonder hides it since the <img> is the
                // visible formula.
                let delimited = if f.display {
                    format!("\\[{}\\]", html_escape_text(&f.latex))
                } else {
                    format!("\\({}\\)", html_escape_text(&f.latex))
                };
                out.push_str(&format!(
                    "<span data-yonder-math-image=\"{mode}\"><img src=\"{}\" alt=\"{}\" width=\"{}\" height=\"{}\"><a data-yonder-math-alt href=\"{}\">{}</a></span>",
                    html_escape_attr(&r.url),
                    html_escape_attr(&alt),
                    r.width,
                    r.height,
                    html_escape_attr(&r.url),
                    delimited,
                ));
                if !kept.contains(&r.id) {
                    kept.push(r.id);
                }
                if r.is_new && !new_ids.contains(&r.id) {
                    new_ids.push(r.id);
                }
            }
            // Unwrap back to delimited LaTeX so a failed render still
            // reads as a formula (and MathJax forks can typeset it).
            None => {
                let d = if f.display { "$$" } else { "$" };
                out.push_str(&format!("{d}{}{d}", f.inner));
            }
        }
    }
    out.push_str(&html[cursor..]);
    Ok(Materialized {
        html: out,
        new_ids,
        kept_ids: kept,
    })
}

/// Escape a value for a double-quoted HTML attribute — same entity set
/// as [`crate::posts::render`] uses for text.
fn html_escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escape text-node content (`&`, `<`, `>` — quotes are literal there).
fn html_escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render one formula to a stored `media_attachments` row, or reuse
/// the existing row when `math_source` matches. Returns `None` on any
/// failure — the caller leaves the formula as text.
#[allow(clippy::too_many_arguments)]
async fn resolve_one(
    db: &sqlx::PgPool,
    store: &MediaStore,
    config: &Config,
    base_url: &str,
    account_id: i64,
    display: bool,
    latex: &str,
    reuse: &ReuseMap,
) -> Option<Resolved> {
    let mode = if display { "display" } else { "inline" };
    let math_source = format!("{mode}:{latex}");
    if let Some((id, key, w, h)) = reuse.get(&math_source) {
        return Some(Resolved {
            id: *id,
            url: crate::media::public_url(config, base_url, key),
            width: w.unwrap_or(0),
            height: h.unwrap_or(0),
            is_new: false,
        });
    }
    let cfg = config.math.clone();
    let latex = latex.to_string();
    let result = async {
        let png = render_png(&cfg, display, &latex).await?;
        let strip = config.media.strip_exif;
        let img = tokio::task::spawn_blocking(move || {
            crate::media::images::process_image(&png, strip, "math")
        })
        .await
        .map_err(|e| Error::Internal(format!("math image task failed: {e}")))??;
        let key = format!(
            "{MEDIA_PREFIX}/{account_id}/{}.{}",
            uuid::Uuid::new_v4(),
            img.ext
        );
        store.put(&key, &img.bytes, &img.content_type).await?;
        let size = img.bytes.len() as i64;
        let exif_stripped = strip && img.ext != "gif";
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO media_attachments \
                 (account_id, type, file_path, content_type, file_size, description, \
                  blurhash, width, height, exif_stripped, processed, math_source) \
             VALUES ($1,'image',$2,$3,$4,$5,$6,$7,$8,$9,true,$10) RETURNING id",
        )
        .bind(account_id)
        .bind(&key)
        .bind(&img.content_type)
        .bind(size)
        .bind(&latex)
        .bind(&img.blurhash)
        .bind(img.width as i32)
        .bind(img.height as i32)
        .bind(exif_stripped)
        .bind(&math_source)
        .fetch_one(db)
        .await?;
        Ok::<_, Error>((id, key, img.width as i32, img.height as i32))
    }
    .await;
    match result {
        Ok((id, key, w, h)) => Some(Resolved {
            id,
            url: crate::media::public_url(config, base_url, &key),
            width: w,
            height: h,
            is_new: true,
        }),
        Err(e) => {
            tracing::warn!(account_id, latex = %latex.chars().take(80).collect::<String>(), error = %e, "math render failed");
            None
        }
    }
}

/// Invoke the configured renderer: `<command>… inline|display`,
/// LaTeX on stdin, PNG bytes on stdout.
async fn render_png(cfg: &MathConfig, display: bool, latex: &str) -> Result<Vec<u8>> {
    let argv: Vec<&str> = cfg.command.split_whitespace().collect();
    let Some((prog, args)) = argv.split_first() else {
        return Err(Error::Internal("math.command is empty".to_string()));
    };
    let mut cmd = tokio::process::Command::new(prog);
    cmd.args(args)
        .arg(if display { "display" } else { "inline" })
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // A timed-out wait drops the child — kill it rather than
        // leaking a hung renderer.
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| Error::Internal(format!("math renderer `{prog}` failed to spawn: {e}")))?;
    let mut stdin = child.stdin.take().expect("stdin was piped");
    if let Err(e) = stdin.write_all(latex.as_bytes()).await {
        return Err(Error::Internal(format!("math renderer stdin failed: {e}")));
    }
    drop(stdin);
    let out = tokio::time::timeout(
        Duration::from_secs(cfg.timeout_seconds),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| Error::Internal("math renderer timed out".to_string()))?
    .map_err(|e| Error::Internal(format!("math renderer wait failed: {e}")))?;
    if !out.status.success() {
        let stderr: String = String::from_utf8_lossy(&out.stderr)
            .chars()
            .take(500)
            .collect();
        return Err(Error::Internal(format!(
            "math renderer exited {}: {stderr}",
            out.status
        )));
    }
    if out.stdout.is_empty() {
        return Err(Error::Internal(
            "math renderer produced no image".to_string(),
        ));
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> MathConfig {
        MathConfig {
            enabled: true,
            command: "true".to_string(),
            ..Default::default()
        }
    }

    fn markers(html: &str) -> String {
        inject_markers(html, &cfg())
    }

    #[test]
    fn inline_and_display_dollars() {
        let out = markers("<p>inline $x^2$ and display $$y^2$$ done</p>");
        assert!(out.contains("data-yonder-math=\"inline\" data-yonder-math-src=\"x^2\""));
        assert!(out.contains("data-yonder-math=\"display\" data-yonder-math-src=\"y^2\""));
    }

    #[test]
    fn paren_and_bracket_delimiters() {
        let out = markers("<p>\\(a+b\\) and \\[c+d\\]</p>");
        assert!(out.contains("data-yonder-math=\"inline\" data-yonder-math-src=\"a+b\""));
        assert!(out.contains("data-yonder-math=\"display\" data-yonder-math-src=\"c+d\""));
    }

    #[test]
    fn skip_subtrees_and_existing_markers() {
        assert_eq!(
            markers("<p><code>$x$</code> <a href=\"/t\">$y$</a></p>"),
            "<p><code>$x$</code> <a href=\"/t\">$y$</a></p>"
        );
        let once = markers("<p>$z$</p>");
        assert_eq!(markers(&once), once, "second pass must be idempotent");
    }

    #[test]
    fn currency_and_escapes_stay_literal() {
        // `$5 and $10` — opener followed by digit is fine, but the
        // "closing" $ is preceded by a space → no match.
        assert_eq!(
            markers("<p>costs $5 and $10 total</p>"),
            "<p>costs $5 and $10 total</p>"
        );
        // `\$` escaped dollar.
        assert_eq!(
            markers("<p>price \\$x$ here</p>"),
            "<p>price \\$x$ here</p>"
        );
        // `$ x$` — space after opener.
        assert_eq!(markers("<p>$ x$</p>"), "<p>$ x$</p>");
        // Empty display math.
        assert_eq!(markers("<p>$$$$</p>"), "<p>$$$$</p>");
    }

    #[test]
    fn entities_survive_escaping() {
        let out = markers("<p>$a &lt; b$</p>");
        assert!(out.contains("data-yonder-math-src=\"a &lt; b\""));
        assert_eq!(decode_html_entities("a &lt; b"), "a < b");
    }

    #[test]
    fn input_cap() {
        let mut c = cfg();
        c.max_input_chars = 3;
        assert_eq!(inject_markers("<p>$abcd$</p>", &c), "<p>$abcd$</p>");
        assert!(inject_markers("<p>$abc$</p>", &c).contains("data-yonder-math"));
    }

    #[test]
    fn multibyte_text_does_not_panic() {
        // Regression: the scanner advanced byte-wise and sliced at
        // non-char-boundaries — any multi-byte char (€, ’, emoji, CJK)
        // in a post containing a delimiter panicked the request.
        let out = markers("<p>€100 for \u{1f495} and $x^2$ plus ’quotes’ and ＋plus</p>");
        assert!(out.contains("data-yonder-math=\"inline\" data-yonder-math-src=\"x^2\""));
        assert!(out.contains("€100"));
        assert!(out.contains("’quotes’"));
        // Multibyte chars before/inside formulas too.
        let out = markers("<p>café $naïve$ — déjà $x_i$ vu</p>");
        assert_eq!(out.matches("data-yonder-math").count(), 4);
    }

    #[test]
    fn disabled_config_is_passthrough() {
        let mut c = cfg();
        c.enabled = false;
        assert_eq!(inject_markers("<p>$x$</p>", &c), "<p>$x$</p>");
        c.enabled = true;
        c.command.clear();
        assert_eq!(inject_markers("<p>$x$</p>", &c), "<p>$x$</p>");
    }
}
