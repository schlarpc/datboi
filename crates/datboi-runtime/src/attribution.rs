//! Component attribution (D54): identity metadata rides IN the artifact
//! as execution-inert custom sections, stamped at build time by the
//! transforms flake lane (`wasm-tools metadata add`), and ENFORCED here
//! at load time — an anonymous component is opaque to reason about, so
//! the hosts refuse to run one.
//!
//! Parsing is a ~60-line hand-rolled walk of the component's top-level
//! sections (custom section id 0: name string + raw payload, per the
//! tool-conventions annotations wasm-metadata writes). Deliberately no
//! `wasm-metadata` dependency: we only READ four known sections, and the
//! parse must stay cheap enough to run on every load.

/// The minimal required set. `name` lives in the `component-name`
/// section; the rest are flat annotation sections whose payload is the
/// raw UTF-8 string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribution {
    pub name: String,
    pub description: String,
    /// Where the source lives (URL).
    pub source: String,
    /// Content-scoped source revision — the flake stamps the git tree
    /// hashes of the two source inputs (`tree:<crate>;guest:<guest-crate>`,
    /// D54/D89), so unrelated commits don't churn bytes.
    pub revision: String,
}

const COMPONENT_PREAMBLE: [u8; 8] = [0x00, 0x61, 0x73, 0x6d, 0x0d, 0x00, 0x01, 0x00];

fn leb_u32(bytes: &[u8], pos: &mut usize) -> Result<u32, String> {
    let mut value: u32 = 0;
    let mut shift = 0;
    loop {
        let byte = *bytes.get(*pos).ok_or("truncated LEB128")?;
        *pos += 1;
        value |= u32::from(byte & 0x7f)
            .checked_shl(shift)
            .ok_or("LEB128 overflow")?;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift > 28 {
            return Err("LEB128 too long".into());
        }
    }
}

fn take<'a>(bytes: &'a [u8], pos: &mut usize, n: usize, what: &str) -> Result<&'a [u8], String> {
    let out = bytes
        .get(*pos..*pos + n)
        .ok_or_else(|| format!("truncated {what}"))?;
    *pos += n;
    Ok(out)
}

/// Extract the component's name from a `component-name` custom section
/// payload: subsection id 0 (component name) wraps a name string.
fn parse_component_name(payload: &[u8]) -> Option<String> {
    let mut pos = 0usize;
    while pos < payload.len() {
        let id = *payload.get(pos)?;
        pos += 1;
        let size = leb_u32(payload, &mut pos).ok()? as usize;
        let sub = payload.get(pos..pos + size)?;
        pos += size;
        if id == 0 {
            let mut sp = 0usize;
            let n = leb_u32(sub, &mut sp).ok()? as usize;
            let raw = sub.get(sp..sp + n)?;
            return String::from_utf8(raw.to_vec()).ok();
        }
    }
    None
}

/// Parse the required attribution out of component bytes.
///
/// # Errors
/// A human-readable list of what's missing or malformed — the loader
/// surfaces this verbatim, so it names the exact gap ("stamp it with
/// `wasm-tools metadata add`" is the fix).
pub fn parse_attribution(bytes: &[u8]) -> Result<Attribution, String> {
    if bytes.len() < 8 || bytes[..8] != COMPONENT_PREAMBLE {
        return Err("not a component binary (bad preamble)".into());
    }
    let mut pos = 8usize;
    let mut name = None;
    let mut description = None;
    let mut source = None;
    let mut revision = None;
    while pos < bytes.len() {
        let id = bytes[pos];
        pos += 1;
        let size = leb_u32(bytes, &mut pos)? as usize;
        let payload = take(bytes, &mut pos, size, "section")?;
        if id != 0 {
            continue; // only custom sections carry attribution
        }
        let mut sp = 0usize;
        let name_len = leb_u32(payload, &mut sp)? as usize;
        let section_name = std::str::from_utf8(take(payload, &mut sp, name_len, "section name")?)
            .map_err(|_| "custom section name is not UTF-8".to_string())?;
        let data = &payload[sp..];
        match section_name {
            "component-name" => name = parse_component_name(data),
            "description" => description = String::from_utf8(data.to_vec()).ok(),
            "source" => source = String::from_utf8(data.to_vec()).ok(),
            "revision" => revision = String::from_utf8(data.to_vec()).ok(),
            _ => {}
        }
    }
    let mut missing = Vec::new();
    if name.is_none() {
        missing.push("name");
    }
    if description.is_none() {
        missing.push("description");
    }
    if source.is_none() {
        missing.push("source");
    }
    if revision.is_none() {
        missing.push("revision");
    }
    if !missing.is_empty() {
        return Err(format!(
            "component missing required attribution metadata [{}] (D54): \
             stamp it at build time with `wasm-tools metadata add`",
            missing.join(", ")
        ));
    }
    Ok(Attribution {
        name: name.expect("checked"),
        description: description.expect("checked"),
        source: source.expect("checked"),
        revision: revision.expect("checked"),
    })
}
