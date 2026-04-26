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

pub fn extract_did_open(body: &[u8]) -> Option<String> {
    extract_doc_uri(body, "textDocument/didOpen")
}

pub fn extract_did_close(body: &[u8]) -> Option<String> {
    extract_doc_uri(body, "textDocument/didClose")
}

fn extract_doc_uri(body: &[u8], expected_method: &str) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    if v.get("method")?.as_str()? != expected_method {
        return None;
    }
    Some(v.pointer("/params/textDocument/uri")?.as_str()?.to_string())
}

pub fn extract_did_open_version(body: &[u8]) -> Option<(String, i64)> {
    extract_doc_uri_and_version(body, "textDocument/didOpen")
}

pub fn extract_did_change(body: &[u8]) -> Option<(String, i64)> {
    extract_doc_uri_and_version(body, "textDocument/didChange")
}

fn extract_doc_uri_and_version(body: &[u8], expected_method: &str) -> Option<(String, i64)> {
    let v: Value = serde_json::from_slice(body).ok()?;
    if v.get("method")?.as_str()? != expected_method {
        return None;
    }
    let uri = v.pointer("/params/textDocument/uri")?.as_str()?.to_string();
    let version = v.pointer("/params/textDocument/version")?.as_i64()?;
    Some((uri, version))
}
