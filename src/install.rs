//! `--install` and `--uninstall`: set this binary up in Claude Code and
//! OpenCode at user scope.
//!
//! Install registers the MCP server, writes the guard hook or plugin and adds a
//! short instruction block to `CLAUDE.md` or `AGENTS.md`. Uninstall undoes
//! that, removes the block `scripts/install.sh` added to the shell rc files, and
//! deletes the binary. Output goes to stderr, like everything else this binary
//! logs.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::atomic::write_atomic;
use crate::cli::Client;
use crate::opencode;

const SERVER: &str = "astlens";
const GUARD: &str = include_str!("../scripts/astlens-guard.sh");
const GUARD_NAME: &str = "astlens-guard.sh";
const OPENCODE_PLUGIN: &str = include_str!("../scripts/opencode-guard.ts");
const OPENCODE_PLUGIN_NAME: &str = "index.ts";

const MEMORY_BEGIN: &str = "<!-- astlens:begin -->";
const MEMORY_END: &str = "<!-- astlens:end -->";
const MEMORY_TEXT: &str = "\
# Code navigation
When exploring or reading source code, prefer the astlens MCP tools over Read/Grep:
- `codebase_map` for a first look at an unfamiliar codebase or directory: the symbols the rest of the code uses most; `focus` ranks around the task's files.
- `get_file_outline` before opening a file; `search_symbols` to find a definition.
- `read_symbol` to read one function/class instead of the whole file; `read_lines` for a range of lines (a compiler error, a search hit).
- `replace_symbol` to rewrite or delete a whole function/class, `insert_symbol` to add one; no Read needed.
- `replace_text` for any other change (an import, a few lines, several spots in one file): exact old text, which must occur once, and its replacement; no Read needed. `replace_lines` to replace a line range, passing the lines as they are now. Do not edit code with a python/perl/sed script.
- `read_file_pruned` when a whole file is needed but may be large.
- `find_references` to see where a symbol is used; `search_text` instead of Grep.
- `changed_symbols` to see which symbols a diff touches before reading it.
- The read tools also open dependency sources by absolute path (e.g. under ~/.cargo/registry/src, ~/.rustup/toolchains, /usr/include).
Use Read/Grep for non-code files or when astlens fails.
Load the tools with ToolSearch (\"select:mcp__astlens__...\") if they are deferred.
";

const OPENCODE_MEMORY_TEXT: &str = "\
# Code navigation
When exploring or reading source code, prefer the astlens MCP tools over read/grep:
- `astlens_codebase_map` for a first look at an unfamiliar codebase or directory: the symbols the rest of the code uses most; `focus` ranks around the task's files.
- `astlens_get_file_outline` before opening a file; `astlens_search_symbols` to find a definition.
- `astlens_read_symbol` to read one function/class instead of the whole file; `astlens_read_lines` for a range of lines (a compiler error, a search hit).
- `astlens_replace_symbol` to rewrite or delete a whole function/class, `astlens_insert_symbol` to add one; no read needed.
- `astlens_replace_text` for any other change (an import, a few lines, several spots in one file): exact old text, which must occur once, and its replacement; no read needed. `astlens_replace_lines` to replace a line range, passing the lines as they are now. Do not edit code with a python/perl/sed script.
- `astlens_read_file_pruned` when a whole file is needed but may be large.
- `astlens_find_references` to see where a symbol is used; `astlens_search_text` instead of grep.
- `astlens_changed_symbols` to see which symbols a diff touches before reading it.
- The read tools also open dependency sources by absolute path (e.g. under ~/.cargo/registry/src, ~/.rustup/toolchains, /usr/include).
Use read/grep for non-code files or when astlens fails.
";

// Must match the markers in scripts/install.sh.
const RC_BEGIN: &str = "# astlens:begin";
const RC_END: &str = "# astlens:end";

fn log(message: impl std::fmt::Display) {
    eprintln!("==> {message}");
}

/// Files this module touches.
struct Paths {
    /// OpenCode's global config directory, `~/.config/opencode`.
    config_dir: PathBuf,
    claude_hook: PathBuf,
    claude_settings: PathBuf,
    claude_memory: PathBuf,
    opencode_memory: PathBuf,
    opencode_plugin: PathBuf,
    opencode_guard: PathBuf,
    rc_files: Vec<PathBuf>,
    cargo_bin: PathBuf,
}

impl Paths {
    fn from_env() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("neither HOME nor USERPROFILE is set"))?;
        let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
        let claude = var("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home.join(".claude"));
        let zsh = var("ZDOTDIR").unwrap_or_else(|| home.clone());
        let config = var("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        let opencode = config.join("opencode");
        let plugin = opencode.join("plugins").join("astlens-guard");
        Ok(Self {
            config_dir: opencode.clone(),
            claude_hook: claude.join("hooks").join(GUARD_NAME),
            claude_settings: claude.join("settings.json"),
            claude_memory: claude.join("CLAUDE.md"),
            opencode_memory: opencode.join("AGENTS.md"),
            opencode_plugin: plugin.join(OPENCODE_PLUGIN_NAME),
            opencode_guard: plugin.join(GUARD_NAME),
            rc_files: vec![
                home.join(".bashrc"),
                home.join(".bash_profile"),
                zsh.join(".zshrc"),
                config.join("fish").join("config.fish"),
            ],
            cargo_bin: var("CARGO_HOME")
                .unwrap_or_else(|| home.join(".cargo"))
                .join("bin"),
        })
    }

    /// The OpenCode file that gets the server entry, chosen from what is on
    /// disk when it is needed.
    fn opencode_config(&self) -> PathBuf {
        opencode::config_path(&self.config_dir)
    }
}

/// Run a command-line program. On Windows an npm install provides
/// `claude.cmd` or `opencode.cmd`, which `Command` does not find under the
/// bare name.
fn run_cli(program: &str, args: &[&str]) -> io::Result<Output> {
    let run = |program: &str| {
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .output()
    };
    match run(program) {
        Err(err) if cfg!(windows) && err.kind() == io::ErrorKind::NotFound => {
            run(&format!("{program}.cmd"))
        }
        other => other,
    }
}

/// Whether `program --version` runs and succeeds.
fn runs(program: &str) -> bool {
    run_cli(program, &["--version"]).is_ok_and(|output| output.status.success())
}

fn current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot locate this binary")?;
    Ok(exe.canonicalize().unwrap_or(exe))
}

/// A path as Claude Code should see it: Windows paths lose the verbatim prefix
/// from `canonicalize` and use forward slashes, which Git Bash also accepts.
fn native_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        text.trim_start_matches(r"\\?\").replace('\\', "/")
    } else {
        text.into_owned()
    }
}

fn claude_available() -> bool {
    runs("claude")
}

fn opencode_available(paths: &Paths) -> bool {
    runs("opencode") || paths.config_dir.exists()
}

pub fn install(enforce: bool, client: Client) -> Result<()> {
    let paths = Paths::from_env()?;
    let (claude_target, opencode_target) = match client {
        Client::Claude => {
            if !claude_available() {
                bail!("cannot run 'claude'; install Claude Code first");
            }
            (true, false)
        }
        Client::Opencode => (false, true),
        Client::All => {
            let claude_target = claude_available();
            let opencode_target = opencode_available(&paths);
            if !claude_target && !opencode_target {
                bail!("neither Claude Code nor OpenCode was found; install one first");
            }
            (claude_target, opencode_target)
        }
    };

    // Checked once, before anything is written, so a missing tool never
    // leaves one client set up and the other not.
    if enforce {
        let guard = match (claude_target, opencode_target) {
            (true, true) => "the hook and the guard plugin need",
            (true, false) => "the hook needs",
            _ => "the guard plugin needs",
        };
        if !runs("jq") {
            bail!(
                "'jq' not found, and {guard} it; install jq, or use --no-enforce to register the MCP server only"
            );
        }
        if opencode_target && !runs("bash") {
            bail!(
                "'bash' not found, and the guard plugin needs it; install bash, or use --no-enforce to register the MCP server only"
            );
        }
    }

    if claude_target {
        install_claude(&paths, enforce)?;
    }
    if opencode_target {
        install_opencode(&paths, enforce)?;
    }
    log(
        "Done. Restart Claude Code, or start a new OpenCode session, then check the MCP server list",
    );
    Ok(())
}

fn install_claude(paths: &Paths, enforce: bool) -> Result<()> {
    let exe = current_exe()?;
    log("Registering MCP server 'astlens' with Claude Code (user scope)");
    // alwaysLoad keeps the tools in the model's manifest; without it Claude Code
    // defers them behind ToolSearch and the model falls back to Read/Grep.
    let config = json!({
        "type": "stdio",
        "command": native_path(&exe),
        "args": ["--serve"],
        "alwaysLoad": true,
    });
    register(&config.to_string(), |args| run_cli("claude", args))?;

    if enforce {
        log(format_args!("Writing hook {}", paths.claude_hook.display()));
        write_hook(&paths.claude_hook)?;
        log(format_args!(
            "Adding hook to {}",
            paths.claude_settings.display()
        ));
        add_hook_to_settings(paths)?;
        log(format_args!(
            "Adding instructions to {}",
            paths.claude_memory.display()
        ));
        add_memory(&paths.claude_memory, MEMORY_TEXT)?;
    } else {
        log("Removing any Claude Code hook and CLAUDE.md block from an earlier install");
        remove_enforcement(paths)?;
    }
    Ok(())
}

fn install_opencode(paths: &Paths, enforce: bool) -> Result<()> {
    let exe = current_exe()?;
    setup_opencode(paths, &native_path(&exe), enforce)
}

fn setup_opencode(paths: &Paths, exe: &str, enforce: bool) -> Result<()> {
    let config = paths.opencode_config();
    log(format_args!(
        "Registering MCP server 'astlens' with OpenCode ({})",
        config.display()
    ));
    opencode::add_server(&config, exe)?;
    // An entry an earlier install left in the other file would only confuse.
    for other in opencode::global_configs(&paths.config_dir) {
        if other != config
            && let Err(err) = opencode::remove_server(&other)
        {
            log(format_args!("{err}; remove astlens from it by hand"));
        }
    }

    if enforce {
        log(format_args!(
            "Writing guard plugin {}",
            paths.opencode_plugin.display()
        ));
        write_opencode_guard(paths)?;
        log(format_args!(
            "Adding instructions to {}",
            paths.opencode_memory.display()
        ));
        add_memory(&paths.opencode_memory, OPENCODE_MEMORY_TEXT)?;
    } else {
        log("Removing any OpenCode guard and AGENTS.md block from an earlier install");
        remove_opencode_enforcement(paths)?;
    }
    Ok(())
}

/// Register the server through `run`, which stands for the `claude` CLI. An
/// earlier registration is the usual reason `add-json` fails, so it is removed
/// only then: any other failure leaves a working registration in place.
fn register(config: &str, mut run: impl FnMut(&[&str]) -> io::Result<Output>) -> Result<()> {
    let add = ["mcp", "add-json", "--scope", "user", SERVER, config];
    let remove = ["mcp", "remove", "--scope", "user", SERVER];
    let stderr = |output: &Output| String::from_utf8_lossy(&output.stderr).trim().to_string();

    let first = run(&add).context("cannot run 'claude mcp add-json'")?;
    if first.status.success() {
        return Ok(());
    }
    let removed = run(&remove).context("cannot run 'claude mcp remove'")?;
    if !removed.status.success() {
        bail!("'claude mcp add-json' failed: {}", stderr(&first));
    }
    let lost = "the earlier registration is already removed; fix this and run --install again";
    let second = run(&add).with_context(|| format!("cannot run 'claude mcp add-json'; {lost}"))?;
    if !second.status.success() {
        bail!("'claude mcp add-json' failed: {}; {lost}", stderr(&second));
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let paths = Paths::from_env()?;
    log("Removing MCP server registration from Claude Code");
    let failure = match run_cli("claude", &["mcp", "remove", "--scope", "user", SERVER]) {
        Ok(output) if output.status.success() => None,
        Ok(output) => Some(format!(
            "'claude mcp remove' failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Err(err) => Some(format!("Could not run 'claude' ({err})")),
    };
    if let Some(failure) = failure {
        log(format_args!(
            "{failure}; if it is still listed, remove it later with: claude mcp remove --scope user {SERVER}"
        ));
    }
    log(format_args!(
        "Removing MCP server registration from OpenCode ({})",
        paths.config_dir.display()
    ));
    for config in opencode::global_configs(&paths.config_dir) {
        if let Err(err) = opencode::remove_server(&config) {
            log(format_args!(
                "{err}; if astlens is still listed, remove it from the OpenCode config by hand"
            ));
        }
    }
    log("Removing hooks, instruction blocks and PATH entry");
    remove_files(&paths)?;
    remove_binary(&current_exe()?, &paths.cargo_bin);
    log("Done. Restart Claude Code and open a new shell.");
    Ok(())
}

fn write_hook(hook: &Path) -> Result<()> {
    if let Some(dir) = hook.parent() {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    fs::write(hook, GUARD).with_context(|| format!("cannot write {}", hook.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(hook, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("cannot make {} executable", hook.display()))?;
    }
    Ok(())
}

/// The OpenCode plugin is a directory with the TypeScript entry point and the
/// shared shell guard beside it.
fn write_opencode_guard(paths: &Paths) -> Result<()> {
    write_hook(&paths.opencode_guard)?;
    write_atomic(&paths.opencode_plugin, OPENCODE_PLUGIN)
        .with_context(|| format!("cannot write {}", paths.opencode_plugin.display()))
}

/// Read settings, refusing to touch a file that is not a JSON object.
fn read_settings(path: &Path) -> Result<Option<Value>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(Some(json!({})));
    }
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid JSON; fix it first", path.display()))?;
    if !value.is_object() {
        bail!("{} is not a JSON object; fix it first", path.display());
    }
    Ok(Some(value))
}

fn write_settings(path: &Path, settings: &Value) -> Result<()> {
    let mut text = serde_json::to_string_pretty(settings)?;
    text.push('\n');
    write_atomic(path, &text).with_context(|| format!("cannot write {}", path.display()))
}

fn add_hook_to_settings(paths: &Paths) -> Result<()> {
    let mut settings = match read_settings(&paths.claude_settings)? {
        Some(settings) => {
            let backup = paths.claude_settings.with_extension("json.bak");
            fs::copy(&paths.claude_settings, &backup)
                .with_context(|| format!("cannot write {}", backup.display()))?;
            settings
        }
        None => json!({}),
    };
    // Quoted, so that a home directory with a space keeps working.
    add_hook(
        &mut settings,
        &format!("\"{}\"", native_path(&paths.claude_hook)),
    )?;
    write_settings(&paths.claude_settings, &settings)
}

/// Append our entry after the existing hooks, replacing an earlier one.
fn add_hook(settings: &mut Value, command: &str) -> Result<()> {
    strip_hook(settings);
    let entry = json!({
        "matcher": "Read|Grep|Bash",
        "hooks": [{"type": "command", "command": command}],
    });
    let root = settings
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings are not a JSON object"))?;
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let pre = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings: 'hooks' is not an object"))?
        .entry("PreToolUse")
        .or_insert_with(|| json!([]));
    pre.as_array_mut()
        .ok_or_else(|| anyhow!("settings: 'hooks.PreToolUse' is not an array"))?
        .push(entry);
    Ok(())
}

/// Drop our hook, matched by file name so that any older form of the command
/// goes too, and leave no empty containers behind. Other hooks stay as they are.
fn strip_hook(settings: &mut Value) {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    let Some(pre) = hooks.get_mut("PreToolUse").and_then(Value::as_array_mut) else {
        return;
    };
    pre.retain_mut(|entry| {
        let Some(commands) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        let before = commands.len();
        commands.retain(|hook| {
            !hook
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|command| command.contains(GUARD_NAME))
        });
        // An entry that had no commands to begin with is not ours to remove.
        !commands.is_empty() || before == 0
    });
    if pre.is_empty() {
        hooks.remove("PreToolUse");
    }
    if hooks.is_empty()
        && let Some(root) = settings.as_object_mut()
    {
        root.remove("hooks");
    }
}

fn add_memory(path: &Path, block: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
    };
    let mut text = strip_block(&text, MEMORY_BEGIN, MEMORY_END);
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("{MEMORY_BEGIN}\n{block}{MEMORY_END}\n"));
    write_atomic(path, &text).with_context(|| format!("cannot write {}", path.display()))
}

/// Remove `begin`..`end` lines. A `begin` with no `end` after it stays, so a
/// damaged block never takes the user's text below it.
fn strip_block(text: &str, begin: &str, end: &str) -> String {
    let is = |line: &str, marker: &str| line.trim_end_matches(['\r', '\n']) == marker;
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut kept = String::with_capacity(text.len());
    let mut i = 0;
    while i < lines.len() {
        if is(lines[i], begin)
            && let Some(len) = lines[i..].iter().position(|line| is(line, end))
        {
            i += len + 1;
            continue;
        }
        kept.push_str(lines[i]);
        i += 1;
    }
    kept
}

/// Remove a block from a file, leaving a file without the block untouched.
fn strip_block_in_file(path: &Path, begin: &str, end: &str) -> Result<()> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
    };
    let stripped = strip_block(&text, begin, end);
    if stripped != text {
        write_atomic(path, &stripped)
            .with_context(|| format!("cannot write {}", path.display()))?;
    }
    Ok(())
}

/// Everything uninstall removes except the MCP registration and the binary.
fn remove_files(paths: &Paths) -> Result<()> {
    remove_enforcement(paths)?;
    remove_opencode_enforcement(paths)?;
    for rc in &paths.rc_files {
        strip_block_in_file(rc, RC_BEGIN, RC_END)?;
    }
    Ok(())
}

/// What `--install` adds and `--install --no-enforce` does not: the hook, its
/// entry in the settings and the CLAUDE.md block.
fn remove_enforcement(paths: &Paths) -> Result<()> {
    if let Some(mut settings) = read_settings(&paths.claude_settings)? {
        let before = settings.clone();
        strip_hook(&mut settings);
        if settings != before {
            write_settings(&paths.claude_settings, &settings)?;
        }
    }
    match fs::remove_file(&paths.claude_hook) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => {
            return Err(err)
                .with_context(|| format!("cannot remove {}", paths.claude_hook.display()));
        }
        _ => {}
    }
    strip_block_in_file(&paths.claude_memory, MEMORY_BEGIN, MEMORY_END)
}

/// What OpenCode enforcement adds: the plugin directory and the AGENTS.md
/// block.
fn remove_opencode_enforcement(paths: &Paths) -> Result<()> {
    for path in [&paths.opencode_plugin, &paths.opencode_guard] {
        match fs::remove_file(path) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => {
                return Err(err).with_context(|| format!("cannot remove {}", path.display()));
            }
            _ => {}
        }
    }
    if let Some(dir) = paths.opencode_plugin.parent() {
        match fs::remove_dir(dir) {
            Err(err)
                if !matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
                ) =>
            {
                return Err(err).with_context(|| format!("cannot remove {}", dir.display()));
            }
            _ => {}
        }
    }
    strip_block_in_file(&paths.opencode_memory, MEMORY_BEGIN, MEMORY_END)
}

#[derive(Debug, PartialEq, Eq)]
enum Owner {
    Cargo,
    PackageManager,
    User,
}

/// Who should remove this binary. Deleting a cargo or package manager file by
/// hand would leave that tool's records wrong.
fn owner(exe: &Path, cargo_bin: &Path) -> Owner {
    let cargo_bin = cargo_bin
        .canonicalize()
        .unwrap_or_else(|_| cargo_bin.to_path_buf());
    if exe.starts_with(&cargo_bin) {
        Owner::Cargo
    } else if exe.starts_with("/usr") || exe.starts_with("/opt") {
        Owner::PackageManager
    } else {
        Owner::User
    }
}

fn remove_binary(exe: &Path, cargo_bin: &Path) {
    let shown = exe.display();
    match owner(exe, cargo_bin) {
        Owner::Cargo => log("Installed by cargo; remove it with: cargo uninstall astlens"),
        Owner::PackageManager => log(format_args!(
            "Not removing {shown}; use the package manager that installed it"
        )),
        Owner::User => match fs::remove_file(exe) {
            Ok(()) => log(format_args!("Removed {shown}")),
            // Windows refuses to delete a running executable.
            Err(err) => log(format_args!(
                "Could not remove {shown} ({err}); close Claude Code, then delete it by hand"
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The global layout: OpenCode's config directory is `~/.config/opencode`.
    fn paths(dir: &Path) -> Paths {
        let opencode = dir.join(".config/opencode");
        let plugin = opencode.join("plugins/astlens-guard");
        Paths {
            config_dir: opencode.clone(),
            claude_hook: dir.join(".claude/hooks").join(GUARD_NAME),
            claude_settings: dir.join(".claude/settings.json"),
            claude_memory: dir.join(".claude/CLAUDE.md"),
            opencode_memory: opencode.join("AGENTS.md"),
            opencode_plugin: plugin.join(OPENCODE_PLUGIN_NAME),
            opencode_guard: plugin.join(GUARD_NAME),
            rc_files: vec![dir.join(".bashrc"), dir.join("fish/config.fish")],
            cargo_bin: dir.join(".cargo/bin"),
        }
    }

    fn commands(settings: &Value) -> Vec<String> {
        settings["hooks"]["PreToolUse"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|entry| entry["hooks"].as_array().unwrap())
            .map(|hook| hook["command"].as_str().unwrap().to_string())
            .collect()
    }

    /// Answer `register`'s commands with the given exit codes, recording them.
    #[cfg(unix)]
    fn run_register(codes: &[i32]) -> (Result<()>, Vec<String>) {
        use std::os::unix::process::ExitStatusExt as _;

        let mut codes = codes.iter();
        let mut seen = Vec::new();
        let result = register("{}", |args| {
            seen.push(args[1].to_string());
            let code = *codes.next().expect("no more commands expected");
            Ok(Output {
                status: std::process::ExitStatus::from_raw(code << 8),
                stdout: Vec::new(),
                stderr: format!("error {}", seen.len()).into_bytes(),
            })
        });
        (result, seen)
    }

    #[cfg(unix)]
    #[test]
    fn register_touches_nothing_else_when_the_first_add_works() {
        let (result, seen) = run_register(&[0]);
        result.unwrap();
        assert_eq!(seen, ["add-json"]);
    }

    #[cfg(unix)]
    #[test]
    fn register_replaces_an_earlier_registration() {
        let (result, seen) = run_register(&[1, 0, 0]);
        result.unwrap();
        assert_eq!(seen, ["add-json", "remove", "add-json"]);
    }

    #[cfg(unix)]
    #[test]
    fn register_reports_the_first_error_when_nothing_was_removed() {
        let (result, seen) = run_register(&[1, 1]);
        assert_eq!(seen, ["add-json", "remove"]);
        assert_eq!(
            result.unwrap_err().to_string(),
            "'claude mcp add-json' failed: error 1"
        );
    }

    #[cfg(unix)]
    #[test]
    fn register_says_so_when_the_removed_registration_is_lost() {
        let (result, seen) = run_register(&[1, 0, 1]);
        assert_eq!(seen, ["add-json", "remove", "add-json"]);
        let message = result.unwrap_err().to_string();
        assert!(message.contains("error 3"), "{message}");
        assert!(message.contains("run --install again"), "{message}");
    }

    #[test]
    fn hook_goes_after_other_hooks_and_replaces_older_forms() {
        let mut settings = json!({
            "model": "x",
            "hooks": {"PreToolUse": [
                {"matcher": "Bash", "hooks": [{"type": "command", "command": "rtk hook claude"}]},
                {"matcher": "Read|Grep", "hooks": [{"type": "command", "command": "/old/astlens-guard.sh"}]},
            ]},
        });
        add_hook(&mut settings, "\"/new/astlens-guard.sh\"").unwrap();
        assert_eq!(
            commands(&settings),
            ["rtk hook claude", "\"/new/astlens-guard.sh\""]
        );
        assert_eq!(settings["model"], "x");
    }

    #[test]
    fn stripping_the_only_hook_leaves_no_empty_containers() {
        let mut settings = json!({"model": "x"});
        add_hook(&mut settings, "/h/astlens-guard.sh").unwrap();
        strip_hook(&mut settings);
        assert_eq!(settings, json!({"model": "x"}));
    }

    #[test]
    fn stripping_keeps_a_shared_entry_and_other_events() {
        let mut settings = json!({"hooks": {
            "PreToolUse": [{"matcher": "Bash", "hooks": [
                {"type": "command", "command": "other"},
                {"type": "command", "command": "/h/astlens-guard.sh"},
            ]}],
            "Stop": [],
        }});
        strip_hook(&mut settings);
        assert_eq!(commands(&settings), ["other"]);
        assert!(settings["hooks"]["Stop"].is_array());
    }

    #[test]
    fn settings_keys_keep_their_order() {
        let mut settings: Value = serde_json::from_str(r#"{"z": 1, "a": 2}"#).unwrap();
        add_hook(&mut settings, "c").unwrap();
        let text = settings.to_string();
        assert!(text.find("\"z\"").unwrap() < text.find("\"a\"").unwrap());
    }

    #[test]
    fn strip_block_removes_only_complete_blocks() {
        assert_eq!(strip_block("a\nB\nx\nE\nb\n", "B", "E"), "a\nb\n");
        assert_eq!(strip_block("a\r\nB\r\nE\r\nb", "B", "E"), "a\r\nb");
        assert_eq!(strip_block("a\nB\nkeep\n", "B", "E"), "a\nB\nkeep\n");
        assert_eq!(strip_block("no block", "B", "E"), "no block");
    }

    #[test]
    fn install_files_can_be_repeated_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        fs::create_dir_all(paths.claude_settings.parent().unwrap()).unwrap();
        fs::write(&paths.claude_settings, r#"{"model": "x"}"#).unwrap();
        fs::write(&paths.claude_memory, "mine").unwrap();
        fs::write(
            &paths.rc_files[0],
            format!("before\n{RC_BEGIN}\nexport PATH=x\n{RC_END}\nafter\n"),
        )
        .unwrap();

        for _ in 0..2 {
            write_hook(&paths.claude_hook).unwrap();
            add_hook_to_settings(&paths).unwrap();
            add_memory(&paths.claude_memory, MEMORY_TEXT).unwrap();
        }
        let settings = read_settings(&paths.claude_settings).unwrap().unwrap();
        assert_eq!(commands(&settings).len(), 1);
        let memory = fs::read_to_string(&paths.claude_memory).unwrap();
        assert_eq!(memory.matches(MEMORY_BEGIN).count(), 1);
        assert!(memory.starts_with("mine\n"));
        assert!(paths.claude_hook.exists());

        remove_files(&paths).unwrap();
        assert!(!paths.claude_hook.exists());
        assert_eq!(
            read_settings(&paths.claude_settings).unwrap().unwrap(),
            json!({"model": "x"})
        );
        assert_eq!(fs::read_to_string(&paths.claude_memory).unwrap(), "mine\n");
        assert_eq!(
            fs::read_to_string(&paths.rc_files[0]).unwrap(),
            "before\nafter\n"
        );
    }

    #[test]
    fn no_enforce_removes_the_hook_but_keeps_the_path_entry() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        fs::create_dir_all(paths.claude_settings.parent().unwrap()).unwrap();
        fs::write(&paths.claude_settings, "{}").unwrap();
        let rc = format!("{RC_BEGIN}\nexport PATH=x\n{RC_END}\n");
        fs::write(&paths.rc_files[0], &rc).unwrap();
        write_hook(&paths.claude_hook).unwrap();
        add_hook_to_settings(&paths).unwrap();
        add_memory(&paths.claude_memory, MEMORY_TEXT).unwrap();

        remove_enforcement(&paths).unwrap();
        assert!(!paths.claude_hook.exists());
        let settings = read_settings(&paths.claude_settings).unwrap().unwrap();
        assert!(commands(&settings).is_empty());
        let memory = fs::read_to_string(&paths.claude_memory).unwrap();
        assert!(!memory.contains(MEMORY_BEGIN));
        assert_eq!(fs::read_to_string(&paths.rc_files[0]).unwrap(), rc);
    }

    #[test]
    fn invalid_settings_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        fs::create_dir_all(paths.claude_settings.parent().unwrap()).unwrap();
        fs::write(&paths.claude_settings, "{ not json").unwrap();
        assert!(add_hook_to_settings(&paths).is_err());
        assert!(remove_files(&paths).is_err());
        assert_eq!(
            fs::read_to_string(&paths.claude_settings).unwrap(),
            "{ not json"
        );
    }

    #[test]
    fn opencode_instructions_can_be_repeated_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        fs::create_dir_all(paths.opencode_memory.parent().unwrap()).unwrap();
        fs::write(&paths.opencode_memory, "mine").unwrap();

        for _ in 0..2 {
            add_memory(&paths.opencode_memory, OPENCODE_MEMORY_TEXT).unwrap();
        }
        let memory = fs::read_to_string(&paths.opencode_memory).unwrap();
        assert_eq!(memory.matches(MEMORY_BEGIN).count(), 1);
        assert!(memory.starts_with("mine\n"));
        assert!(memory.contains("astlens_read_symbol"));

        remove_opencode_enforcement(&paths).unwrap();
        assert_eq!(
            fs::read_to_string(&paths.opencode_memory).unwrap(),
            "mine\n"
        );
    }

    #[test]
    fn opencode_setup_writes_the_registration_guard_and_instructions() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        setup_opencode(&paths, "/bin/astlens", true).unwrap();

        let config = fs::read_to_string(paths.opencode_config()).unwrap();
        assert_eq!(
            config,
            "{\n  \"mcp\": {\n    \"servers\": {\n      \"astlens\": {\n        \"type\": \"local\",\n        \"command\": [\"/bin/astlens\", \"--serve\"],\n        \"codemode\": false\n      }\n    }\n  }\n}\n"
        );
        assert!(paths.opencode_plugin.exists());
        assert!(paths.opencode_guard.exists());
        assert!(
            fs::read_to_string(&paths.opencode_memory)
                .unwrap()
                .contains(OPENCODE_MEMORY_TEXT.trim_end())
        );
    }

    #[test]
    fn opencode_setup_without_enforce_removes_the_guard_but_not_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        setup_opencode(&paths, "/bin/astlens", true).unwrap();
        setup_opencode(&paths, "/bin/astlens", false).unwrap();

        assert!(!paths.opencode_plugin.exists());
        assert!(!paths.opencode_guard.exists());
        let memory = fs::read_to_string(&paths.opencode_memory).unwrap();
        assert!(!memory.contains(MEMORY_BEGIN));
        assert!(
            fs::read_to_string(paths.opencode_config())
                .unwrap()
                .contains("\"astlens\"")
        );
    }

    #[test]
    fn opencode_setup_writes_where_the_servers_take_effect() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let json = paths.config_dir.join("opencode.json");
        let jsonc = paths.config_dir.join("opencode.jsonc");
        fs::create_dir_all(&paths.config_dir).unwrap();
        // An entry from an earlier install, shadowed by the jsonc servers.
        setup_opencode(&paths, "/bin/astlens", false).unwrap();
        fs::write(
            &jsonc,
            "{\"mcp\": {\"servers\": {\"other\": {\"type\": \"remote\"}}}}\n",
        )
        .unwrap();

        setup_opencode(&paths, "/bin/astlens", false).unwrap();
        let text = fs::read_to_string(&jsonc).unwrap();
        assert!(text.contains("\"astlens\"") && text.contains("\"other\""));
        assert!(!fs::read_to_string(&json).unwrap().contains("\"astlens\""));
    }

    #[test]
    fn opencode_guard_files_can_be_repeated_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());

        for _ in 0..2 {
            write_opencode_guard(&paths).unwrap();
        }
        assert!(paths.opencode_plugin.exists());
        assert!(paths.opencode_guard.exists());
        assert!(
            fs::read_to_string(&paths.opencode_plugin)
                .unwrap()
                .contains("astlens-guard")
        );

        remove_opencode_enforcement(&paths).unwrap();
        assert!(!paths.opencode_plugin.exists());
        assert!(!paths.opencode_guard.exists());
        assert!(!paths.opencode_plugin.parent().unwrap().exists());
    }

    #[test]
    fn opencode_guard_leaves_a_shared_plugin_directory() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        write_opencode_guard(&paths).unwrap();
        let other = paths.opencode_plugin.parent().unwrap().join("other.ts");
        fs::write(&other, "mine").unwrap();

        remove_opencode_enforcement(&paths).unwrap();
        assert!(!paths.opencode_plugin.exists());
        assert_eq!(fs::read_to_string(other).unwrap(), "mine");
    }

    #[test]
    fn remove_files_tolerates_a_clean_home() {
        let dir = tempfile::tempdir().unwrap();
        remove_files(&paths(dir.path())).unwrap();
    }

    #[test]
    fn binary_owner_follows_its_location() {
        let cargo = Path::new("/nonexistent-home/.cargo/bin");
        let owner_of = |exe: &str| owner(Path::new(exe), cargo);
        assert_eq!(owner_of("/nonexistent-home/.cargo/bin/x"), Owner::Cargo);
        assert_eq!(owner_of("/usr/bin/x"), Owner::PackageManager);
        assert_eq!(owner_of("/nonexistent-home/.local/bin/x"), Owner::User);
    }
}
