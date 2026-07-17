//! Dat auto-fetch (D16), descended from the CLI (D96) so `datboi dat
//! fetch` and `POST /v1/dats/fetch` run ONE implementation. Redump
//! serves datfiles over HTTP normally, so a fetch resolves a source,
//! makes one polite request, unwraps a zipped datfile, and hands the
//! bytes to the normal import path — the artifact enters CAS first and
//! import stays a deterministic function of the CAS blob (D15). No-Intro
//! stays a manual drop.

use std::io::Read;
use std::time::Duration;

use crate::CatalogError;

/// A dat fetched over HTTP, ready to run through [`import_dat`](crate::import_dat).
pub struct FetchedDat {
    /// The resolved URL that was fetched.
    pub url: String,
    /// The dat bytes (zip-unwrapped if the server served a zip).
    pub bytes: Vec<u8>,
    /// Provider label the source implies (`redump/...` → `"Redump"`),
    /// used only when the caller specifies no provider override.
    pub provider_default: Option<&'static str>,
}

/// Cap on a fetched body: a dat is never this big; a hostile server
/// might be. Doubles as the zip-unwrap read cap.
const FETCH_LIMIT: u64 = 256 << 20;

/// Resolve a fetch source: a full URL passes through; `redump/<slug>`
/// expands to the stable datfile endpoint (D16). The slug is charset-
/// validated so it cannot smuggle a path segment or a different host.
fn resolve(source: &str) -> Result<(String, Option<&'static str>), CatalogError> {
    if source.starts_with("http://") || source.starts_with("https://") {
        return Ok((source.to_owned(), None));
    }
    if let Some(slug) = source.strip_prefix("redump/") {
        if slug.is_empty() || !slug.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(CatalogError::Fetch(format!("bad redump system slug {slug:?}")));
        }
        return Ok((format!("http://redump.org/datfile/{slug}/"), Some("Redump")));
    }
    Err(CatalogError::Fetch(format!(
        "expected a URL or redump/<system-slug>, got {source:?}"
    )))
}

/// If `bytes` is a zip, extract its sole member (the D35 walker — the
/// same primitive zipped-dat ingest uses); otherwise pass through.
/// Redump serves single-member zips; a direct URL may serve a bare dat.
fn unwrap(bytes: Vec<u8>) -> Result<Vec<u8>, CatalogError> {
    if !datboi_ingest::zip::looks_like_zip(&bytes) {
        return Ok(bytes);
    }
    let mut cursor = std::io::Cursor::new(&bytes);
    match datboi_ingest::zip::read_sole_member(&mut cursor, FETCH_LIMIT)
        .map_err(|e| CatalogError::Fetch(format!("fetched zip: {e}")))?
    {
        Some(member) => Ok(member.bytes),
        // Multi-member (a ROM container, not a datfile) or an unsupported
        // member: not a datfile the fetch path can unwrap.
        None => Err(CatalogError::Fetch(
            "fetched zip is not a single-member datfile".into(),
        )),
    }
}

/// Fetch a dat over HTTP (D16). One polite request: honest User-Agent,
/// 60 s timeout, no retries — a failed fetch degrades to a manual drop.
/// The bytes come back for the caller to run through
/// [`import_dat`](crate::import_dat), so the artifact enters CAS via the
/// same deterministic path a manual drop does (D15).
///
/// # Errors
/// A bad source, an HTTP failure, or a zip that isn't a single-member
/// datfile — all [`CatalogError::Fetch`].
pub fn fetch_dat(source: &str) -> Result<FetchedDat, CatalogError> {
    let (url, provider_default) = resolve(source)?;
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("datboi/", env!("CARGO_PKG_VERSION")))
        .build();
    let response = agent
        .get(&url)
        .call()
        .map_err(|e| CatalogError::Fetch(format!("fetching {url}: {e}")))?;
    let mut body = Vec::new();
    response
        .into_reader()
        .take(FETCH_LIMIT)
        .read_to_end(&mut body)
        .map_err(|e| CatalogError::Fetch(format!("reading fetch body: {e}")))?;
    let bytes = unwrap(body)?;
    Ok(FetchedDat {
        url,
        bytes,
        provider_default,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_urls_and_redump_slugs() {
        assert_eq!(
            resolve("https://example.com/x.dat").unwrap(),
            ("https://example.com/x.dat".to_owned(), None)
        );
        let (url, provider) = resolve("redump/psx").unwrap();
        assert_eq!(url, "http://redump.org/datfile/psx/");
        assert_eq!(provider, Some("Redump"));
    }

    #[test]
    fn rejects_bad_sources() {
        // A slug that could smuggle a path or a different host.
        assert!(resolve("redump/psx/../etc").is_err());
        assert!(resolve("redump/").is_err());
        assert!(resolve("ftp://nope").is_err());
        assert!(resolve("psx").is_err());
    }

    #[test]
    fn unwrap_passes_bare_dat_through() {
        let dat = b"<?xml version=\"1.0\"?><datafile></datafile>".to_vec();
        assert_eq!(unwrap(dat.clone()).unwrap(), dat);
    }
}
