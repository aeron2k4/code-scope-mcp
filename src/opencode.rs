//! OpenCode user configuration: the MCP server entry in `opencode.json(c)`.
//!
//! The file may contain comments and trailing commas, so it is edited through
//! a CST rather than parsed and serialised: everything we do not own stays as
//! the user wrote it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstRootNode};

use crate::atomic::write_atomic;

pub const SERVER: &str = "astlens";

/// OpenCode's two global files, in the order it reads them.
const GLOBAL_FILES: [&str; 2] = ["opencode.json", "opencode.jsonc"];

/// OpenCode reads both `opencode.json` and `opencode.jsonc`, and each
/// document's non-empty `mcp.servers` replaces the map read before it. The
/// entry therefore goes into the last file that has servers, since only that
/// map takes effect; with none, into an existing file (`opencode.json`
/// first), and otherwise into a new `opencode.json`. `dir` is the OpenCode
/// config directory.
pub fn config_path(dir: &Path) -> PathBuf {
    let [json, jsonc] = global_configs(dir);
    if has_servers(&jsonc) || (!json.exists() && jsonc.exists()) {
        jsonc
    } else {
        json
    }
}

/// Both global files, existing or not, in the order OpenCode reads them.
pub fn global_configs(dir: &Path) -> [PathBuf; 2] {
    GLOBAL_FILES.map(|name| dir.join(name))
}

/// Whether `path` holds a non-empty `mcp.servers` map.
fn has_servers(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(root) = CstRootNode::parse(&text, &ParseOptions::default()) else {
        return false;
    };
    root.object_value()
        .and_then(|object| object.object_value("mcp"))
        .and_then(|mcp| mcp.object_value("servers"))
        .is_some_and(|servers| !servers.properties().is_empty())
}

fn read(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn parse(path: &Path, text: &str) -> Result<CstRootNode> {
    CstRootNode::parse(text, &ParseOptions::default())
        .map_err(|err| anyhow!("{} is not valid JSONC: {err}", path.display()))
}

fn write(path: &Path, original: Option<&str>, edited: &str) -> Result<()> {
    if original == Some(edited) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    if original.is_some() {
        let backup = path.with_extension(format!(
            "{}.bak",
            path.extension()
                .map(|extension| extension.to_string_lossy())
                .unwrap_or_default()
        ));
        fs::copy(path, &backup).with_context(|| format!("cannot write {}", backup.display()))?;
    }
    write_atomic(path, edited).with_context(|| format!("cannot write {}", path.display()))
}

fn finish(original: &str, root: &CstRootNode) -> String {
    let mut text = root.to_string();
    if original.ends_with('\n') && !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// Add or replace `mcp.servers.astlens`, leaving other servers and comments
/// alone. The server runs in OpenCode's workspace directory, so it needs no
/// root argument; `codemode: false` exposes its tools directly.
pub fn add_server(config: &Path, command: &str) -> Result<()> {
    let original = read(config)?;
    let text = original.as_deref().unwrap_or("{\n}\n");
    let root = parse(config, text)?;
    let object = root
        .object_value_or_create()
        .ok_or_else(|| anyhow!("{} is not a JSON object", config.display()))?;
    let mcp = object
        .object_value_or_create("mcp")
        .ok_or_else(|| anyhow!("{}: 'mcp' is not an object", config.display()))?;
    let servers = mcp
        .object_value_or_create("servers")
        .ok_or_else(|| anyhow!("{}: 'mcp.servers' is not an object", config.display()))?;
    let server = vec![
        ("type".to_string(), CstInputValue::from("local")),
        (
            "command".to_string(),
            CstInputValue::Array(vec![
                CstInputValue::from(command),
                CstInputValue::from("--serve"),
            ]),
        ),
        ("codemode".to_string(), CstInputValue::from(false)),
    ];
    let value = CstInputValue::Object(server);
    match servers.get(SERVER) {
        Some(property) => property.set_value(value),
        None => {
            servers.append(SERVER, value);
        }
    }
    write(config, original.as_deref(), &finish(text, &root))
}

/// Remove only `mcp.servers.astlens`. A clean or absent config is not an
/// error, so uninstall stays idempotent.
pub fn remove_server(config: &Path) -> Result<()> {
    let Some(original) = read(config)? else {
        return Ok(());
    };
    let root = parse(config, &original)?;
    let Some(object) = root.object_value() else {
        return Ok(());
    };
    let Some(mcp) = object.object_value("mcp") else {
        return Ok(());
    };
    let Some(servers) = mcp.object_value("servers") else {
        return Ok(());
    };
    let Some(property) = servers.get(SERVER) else {
        return Ok(());
    };
    property.remove();
    write(config, Some(&original), &finish(&original, &root))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(dir: &Path) -> PathBuf {
        dir.join("opencode.jsonc")
    }

    #[test]
    fn a_missing_config_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let config = file(dir.path());
        add_server(&config, "/bin/astlens").unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(
            value["mcp"]["servers"]["astlens"],
            serde_json::json!({
                "type": "local",
                "command": ["/bin/astlens", "--serve"],
                "codemode": false,
            })
        );
    }

    #[test]
    fn comments_and_other_servers_survive() {
        let dir = tempfile::tempdir().unwrap();
        let config = file(dir.path());
        fs::write(
            &config,
            "{\n  // keep me\n  \"model\": \"x\",\n  \"mcp\": {\n    \"servers\": {\n      \"other\": {\"type\": \"remote\", \"url\": \"https://example.com\"}\n    }\n  }\n}\n",
        )
        .unwrap();
        add_server(&config, "/bin/astlens").unwrap();
        add_server(&config, "/bin/astlens").unwrap();
        let text = fs::read_to_string(&config).unwrap();
        assert!(text.contains("// keep me"));
        assert!(text.contains("\"other\""));
        assert_eq!(text.matches("\"astlens\"").count(), 1);
        assert!(text.ends_with('\n'));
        assert_eq!(
            fs::read_to_string(config.with_extension("jsonc.bak"))
                .unwrap()
                .matches("\"astlens\"")
                .count(),
            0
        );
    }

    #[test]
    fn remove_only_drops_our_server() {
        let dir = tempfile::tempdir().unwrap();
        let config = file(dir.path());
        fs::write(
            &config,
            "{\n  // keep me\n  \"mcp\": {\"servers\": {\n    \"astlens\": {\"type\": \"local\", \"command\": [\"x\"]},\n    \"other\": {\"type\": \"remote\"}\n  }}\n}\n",
        )
        .unwrap();
        remove_server(&config).unwrap();
        let text = fs::read_to_string(&config).unwrap();
        assert!(!text.contains("\"astlens\""));
        assert!(text.contains("\"other\""));
        assert!(text.contains("// keep me"));
    }

    #[test]
    fn invalid_config_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let config = file(dir.path());
        fs::write(&config, "{ not json").unwrap();
        assert!(add_server(&config, "/bin/astlens").is_err());
        assert_eq!(fs::read_to_string(&config).unwrap(), "{ not json");
    }

    #[test]
    fn the_entry_goes_where_opencode_takes_its_servers_from() {
        let dir = tempfile::tempdir().unwrap();
        let json = dir.path().join("opencode.json");
        let jsonc = dir.path().join("opencode.jsonc");
        let servers = "{\"mcp\": {\"servers\": {\"other\": {\"type\": \"remote\"}}}}";

        // The jsonc servers replace those of the plain file, read before it.
        fs::write(&json, servers).unwrap();
        fs::write(&jsonc, servers).unwrap();
        assert_eq!(config_path(dir.path()), jsonc);

        // Without servers of its own, the jsonc file replaces nothing.
        fs::write(&jsonc, "{\"mcp\": {\"servers\": {}}}").unwrap();
        assert_eq!(config_path(dir.path()), json);

        fs::write(&json, "{}").unwrap();
        fs::write(&jsonc, servers).unwrap();
        assert_eq!(config_path(dir.path()), jsonc);
    }

    #[test]
    fn an_existing_config_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let json = dir.path().join("opencode.json");
        let jsonc = dir.path().join("opencode.jsonc");
        assert_eq!(config_path(dir.path()), json);
        fs::write(&jsonc, "{}").unwrap();
        assert_eq!(config_path(dir.path()), jsonc);
        fs::write(&json, "{}").unwrap();
        assert_eq!(config_path(dir.path()), json);
    }
}
