use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorPos {
    pub uri: String,
    pub line: u64,
    pub character: u64,
}

pub fn extract_cursor(body: &[u8]) -> Option<CursorPos> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let method = v.get("method")?.as_str()?;
    if method != "textDocument/hover" && method != "textDocument/documentHighlight" {
        return None;
    }
    let params = v.get("params")?;
    let uri = params.pointer("/textDocument/uri")?.as_str()?.to_string();
    let line = params.pointer("/position/line")?.as_u64()?;
    let character = params.pointer("/position/character")?.as_u64()?;
    Some(CursorPos {
        uri,
        line,
        character,
    })
}

pub fn is_initialized(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| Some(v.get("method")?.as_str()? == "initialized"))
        .unwrap_or(false)
}
