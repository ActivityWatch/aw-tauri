//! In-place edits to the user's `config.toml` made from the tray.
//!
//! The file is re-read from disk (not the config cached at startup) and edited
//! with `toml_edit`, so comments, layout and hand edits made while the app runs
//! are kept. A file that doesn't parse is left untouched, and every write goes
//! through [`write_atomic`] so a crash mid-write can't truncate the config.

use log::{info, warn};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value};

use crate::{autostart, get_config_path, UserConfig};

/// Replaces `path` with `contents` atomically: writes a temporary file next to
/// it, flushes it to disk and renames it over the original (which replaces the
/// target on Windows too). The original's permissions are kept, and a symlinked
/// config is written through to its target rather than replaced by a file.
pub(crate) fn write_atomic(path: &Path, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    let target = resolve_symlinks(path);
    let dir = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".into());
    let tmp = dir.join(format!(".{file_name}.tmp-{}", std::process::id()));

    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(contents.as_ref())?;
        file.sync_all()?;
        drop(file);
        if let Ok(meta) = fs::metadata(&target) {
            fs::set_permissions(&tmp, meta.permissions())?;
        }
        fs::rename(&tmp, &target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Follows `path` through any chain of symlinks to the file it names, whether or
/// not that file exists yet (unlike `canonicalize`), so writing a config whose
/// link target hasn't been created yet creates the target and keeps the link.
fn resolve_symlinks(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    // Bounded, like the OS's own limit, so a symlink loop can't spin forever.
    for _ in 0..40 {
        let is_link = fs::symlink_metadata(&current)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        if !is_link {
            break;
        }
        match fs::read_link(&current) {
            Ok(link) if link.is_absolute() => current = link,
            Ok(link) => current = current.parent().map(|dir| dir.join(&link)).unwrap_or(link),
            Err(_) => break,
        }
    }
    current
}

/// Adds `name` to, or removes it from, `[autostart] modules` in the config file,
/// so a module started or stopped from the tray stays that way after a restart.
pub(crate) fn persist_module_autostart(name: &str, enabled: bool) -> Result<(), String> {
    let _guard = autostart::PERSIST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let path = get_config_path();
    let source = fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read config file {}: {e}", path.display()))?;
    match set_module_autostart(&source, name, enabled)? {
        Some(updated) => write_atomic(&path, updated)
            .map_err(|e| format!("Failed to write config file {}: {e}", path.display())),
        None => Ok(()),
    }
}

/// Persists a tray module click (see [`persist_module_autostart`]), logging
/// instead of failing: the module has already been started/stopped either way.
pub(crate) fn persist_module_click(name: &str, enabled: bool) {
    match persist_module_autostart(name, enabled) {
        Ok(()) => info!(
            "{} {name} {} autostart modules",
            if enabled { "Added" } else { "Removed" },
            if enabled { "to" } else { "from" }
        ),
        Err(e) => warn!("Could not save autostart change for {name}: {e}"),
    }
}

/// Splits whitespace/comment decor at its first newline: the part before it is
/// on the same line as the preceding token (e.g. a `# comment` after an entry's
/// comma), the rest (newline included) belongs to the following lines.
fn split_first_line(s: &str) -> (&str, &str) {
    match s.find('\n') {
        Some(i) => s.split_at(i),
        None => (s, ""),
    }
}

fn decor_str(raw: Option<&toml_edit::RawString>) -> String {
    raw.and_then(|r| r.as_str()).unwrap_or("").to_owned()
}

fn value_name(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.value().clone()),
        Value::InlineTable(t) => t.get("name").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

fn value_args(v: &Value) -> Option<String> {
    match v {
        Value::InlineTable(t) => args_of(t.get("args").and_then(Value::as_str)),
        _ => None,
    }
}

fn args_of(args: Option<&str>) -> Option<String> {
    args.filter(|a| !a.is_empty()).map(str::to_owned)
}

/// Appends `name` to an inline `modules = [...]` array, matching its layout.
///
/// In a multi-line array the text after the last entry (its suffix, or the
/// array's trailing decor if it ends with a comma) holds that entry's
/// same-line comment and then the lines before `]`. The comment stays on the
/// old last entry's line (after the comma that now follows it), the new entry
/// goes on the next line with the same indentation, and anything else that
/// was before `]` stays there.
fn array_push(modules: &mut Array, name: &str) {
    let last = modules.len().checked_sub(1);
    let last_prefix = last
        .and_then(|i| modules.get(i))
        .map(|v| decor_str(v.decor().prefix()))
        .unwrap_or_default();
    let last_suffix = last
        .and_then(|i| modules.get(i))
        .map(|v| decor_str(v.decor().suffix()))
        .unwrap_or_default();
    let trailing = decor_str(Some(modules.trailing()));
    // Text between the last entry (or `[`) and `]`, as one string.
    let tail = if last.is_some() && !modules.trailing_comma() {
        format!("{last_suffix}{trailing}")
    } else {
        trailing
    };
    let multiline = last.is_none() || last_prefix.contains('\n') || tail.contains('\n');

    if !multiline {
        // Single-line array: `["a", "b"]` -> `["a", "b", "c"]`.
        modules.push(name);
        let new = modules.len() - 1;
        if let Some(v) = modules.get_mut(new) {
            v.decor_mut()
                .set_prefix(if last.is_some() { " " } else { "" });
            v.decor_mut().set_suffix("");
        }
        return;
    }

    let indent = match last_prefix.rfind('\n') {
        Some(i) => last_prefix[i + 1..].to_owned(),
        None => "  ".to_owned(),
    };
    let (same_line, rest) = split_first_line(&tail);
    let (same_line, rest) = (same_line.to_owned(), rest.to_owned());

    if let Some(i) = last {
        if let Some(v) = modules.get_mut(i) {
            v.decor_mut().set_suffix("");
        }
    }
    modules.push(name);
    let new = modules.len() - 1;
    if let Some(v) = modules.get_mut(new) {
        v.decor_mut().set_prefix(format!("{same_line}\n{indent}"));
        v.decor_mut().set_suffix("");
    }
    modules.set_trailing(if rest.is_empty() {
        "\n".to_owned()
    } else {
        rest
    });
}

/// Removes entry `i` from an inline `modules = [...]` array, keeping the
/// comments that belong to its neighbours.
///
/// The removed entry's own comments (lines above it, and a comment on its
/// line) go with it. A comment on the previous entry's line, which the parser
/// stores in front of entry `i`, is kept.
fn array_remove(modules: &mut Array, i: usize) {
    let n = modules.len();
    let removed = modules.get(i).expect("index in bounds");
    let prefix = decor_str(removed.decor().prefix());
    let suffix = decor_str(removed.decor().suffix());
    let multiline = prefix.contains('\n');
    let keep_prev = split_first_line(&prefix).0.to_owned();
    modules.remove(i);
    if !multiline {
        // Single-line array: drop the separator space the entry carried.
        if i == 0 {
            if let Some(v) = modules.get_mut(0) {
                v.decor_mut().set_prefix("");
            }
        }
        return;
    }

    if i + 1 < n {
        // The next entry's prefix starts with the removed entry's same-line comment.
        if let Some(next) = modules.get_mut(i) {
            let next_prefix = decor_str(next.decor().prefix());
            if next_prefix.contains('\n') {
                let rest = split_first_line(&next_prefix).1;
                next.decor_mut().set_prefix(format!("{keep_prev}{rest}"));
            }
        }
    } else {
        // Removed the last entry: the text up to `]` holds its same-line comment
        // (dropped) and then the lines before `]` (kept).
        let trailing_comma = modules.trailing_comma();
        let trailing = decor_str(Some(modules.trailing()));
        let tail = if trailing_comma {
            trailing
        } else {
            format!("{suffix}{trailing}")
        };
        let rest = split_first_line(&tail).1.to_owned();
        if i == 0 {
            modules.set_trailing_comma(false);
            let rest = if rest.is_empty() {
                "\n".to_owned()
            } else {
                rest
            };
            modules.set_trailing(format!("{keep_prev}{rest}"));
        } else if trailing_comma {
            modules.set_trailing(format!("{keep_prev}{rest}"));
        } else {
            if let Some(prev) = modules.get_mut(i - 1) {
                prev.decor_mut().set_suffix(format!("{keep_prev}{rest}"));
            }
            modules.set_trailing("");
        }
    }
}

/// Appends a `[[autostart.modules]]` table for `name` right after the last one.
fn tables_push(modules: &mut ArrayOfTables, name: &str) {
    let mut table = Table::new();
    table.insert("name", toml_edit::value(name));
    let position = modules.iter().filter_map(Table::position).max();
    table.set_position(position);
    modules.push(table);
}

/// Returns `source` with module `name` added to (`enabled`) or removed from
/// `[autostart] modules`, or `None` if it is already in that state.
///
/// Both forms the config accepts are edited: an inline `modules = [...]` array
/// (of names and/or `{ name, args }` tables) and `[[autostart.modules]]`
/// tables. Removing an entry that carries args (the last such entry, if the
/// module is listed more than once) moves them to `[module_args]`,
/// replacing any value there: the entry's args are what was in effect (they win
/// over `[module_args]` when both are set), so a later start from the tray keeps
/// using them.
pub(crate) fn set_module_autostart(
    source: &str,
    name: &str,
    enabled: bool,
) -> Result<Option<String>, String> {
    toml::from_str::<UserConfig>(source)
        .map_err(|e| format!("Config file is malformed, not updating it: {e}"))?;
    let mut doc: DocumentMut = source
        .parse()
        .map_err(|e| format!("Config file is malformed, not updating it: {e}"))?;

    let mut moved_args = None;
    {
        let item = doc
            .get_mut("autostart")
            .and_then(|item| item.get_mut("modules"))
            .ok_or("Config file has no [autostart] modules list")?;
        match item {
            Item::Value(Value::Array(modules)) => {
                let matches: Vec<usize> = (0..modules.len())
                    .filter(|&i| modules.get(i).and_then(value_name).as_deref() == Some(name))
                    .collect();
                if enabled {
                    if !matches.is_empty() {
                        return Ok(None);
                    }
                    array_push(modules, name);
                } else {
                    if matches.is_empty() {
                        return Ok(None);
                    }
                    for &i in matches.iter().rev() {
                        // Reverse order: the first args seen are the last entry's,
                        // which are the effective ones (see `configured_modules_args`).
                        if moved_args.is_none() {
                            moved_args = modules.get(i).and_then(value_args);
                        }
                        array_remove(modules, i);
                    }
                }
            }
            Item::ArrayOfTables(modules) => {
                let matches: Vec<usize> = (0..modules.len())
                    .filter(|&i| {
                        modules
                            .get(i)
                            .and_then(|t| t.get("name"))
                            .and_then(Item::as_str)
                            == Some(name)
                    })
                    .collect();
                if enabled {
                    if !matches.is_empty() {
                        return Ok(None);
                    }
                    tables_push(modules, name);
                } else {
                    if matches.is_empty() {
                        return Ok(None);
                    }
                    for &i in matches.iter().rev() {
                        let args = modules
                            .get(i)
                            .and_then(|t| t.get("args"))
                            .and_then(Item::as_str);
                        if moved_args.is_none() {
                            moved_args = args_of(args);
                        }
                        modules.remove(i);
                    }
                }
            }
            _ => return Err("Config file's [autostart] modules is not a list".into()),
        }
        // An empty `[[autostart.modules]]` array renders as nothing, which would
        // drop the required key; write it as `modules = []` instead.
        if item
            .as_array_of_tables()
            .is_some_and(ArrayOfTables::is_empty)
        {
            if let Some(autostart) = doc.get_mut("autostart").and_then(Item::as_table_like_mut) {
                autostart.remove("modules");
                autostart.insert("modules", toml_edit::value(Array::new()));
            }
        }
    }

    if let Some(args) = moved_args {
        let module_args = doc
            .entry("module_args")
            .or_insert(toml_edit::table())
            .as_table_like_mut()
            .ok_or("Config file has a non-table module_args")?;
        match module_args.get_mut(name) {
            // Keep the key's decor (e.g. a comment above it) and replace the value.
            Some(existing) if existing.is_value() => {
                let decor = existing.as_value().map(|v| v.decor().clone());
                let mut new = Value::from(args);
                if let Some(decor) = decor {
                    *new.decor_mut() = decor;
                }
                *existing = Item::Value(new);
            }
            _ => {
                module_args.insert(name, toml_edit::value(args));
            }
        }
    }

    let updated = doc.to_string();
    toml::from_str::<UserConfig>(&updated)
        .map_err(|e| format!("Updated config did not parse, not writing it: {e}"))?;
    Ok(Some(updated))
}

#[cfg(test)]
mod tests {
    use super::{set_module_autostart, write_atomic};
    use crate::{write_formatted_config, AutostartConfig, ModuleEntry, UpdatesConfig, UserConfig};

    /// A unique scratch dir under the system temp dir; never the real config dir.
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aw-tauri-cfg-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A config as written on first run by `write_formatted_config` (Python
    /// watchers, e.g. an X11 session without aw-awatcher installed).
    fn first_run_config() -> String {
        let config = UserConfig {
            port: 5600,
            discovery_paths: vec!["/home/u/aw-modules".into()],
            autostart: AutostartConfig {
                enabled: true,
                minimized: true,
                modules: vec![
                    ModuleEntry::Simple("aw-watcher-afk".into()),
                    ModuleEntry::Simple("aw-watcher-window".into()),
                ],
            },
            module_args: Default::default(),
            updates: UpdatesConfig::default(),
        };
        let dir = scratch_dir("first-run");
        let path = dir.join("config.toml");
        write_formatted_config(&config, &path).unwrap();
        let s = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        s
    }

    fn parse(source: &str) -> UserConfig {
        toml::from_str::<UserConfig>(source).unwrap()
    }

    fn modules_of(source: &str) -> Vec<String> {
        parse(source)
            .autostart
            .modules
            .iter()
            .map(|m| m.name().to_string())
            .collect()
    }

    fn set(source: &str, name: &str, enabled: bool) -> String {
        set_module_autostart(source, name, enabled)
            .unwrap()
            .unwrap_or_else(|| panic!("{name} -> {enabled} should change the config"))
    }

    #[test]
    fn tray_toggles_persist_to_autostart_modules() {
        let source = first_run_config();

        // Enabling aw-awatcher from the tray adds it, in the file's one-per-line layout.
        let added = set(&source, "aw-awatcher", true);
        assert_eq!(
            modules_of(&added),
            ["aw-watcher-afk", "aw-watcher-window", "aw-awatcher"]
        );
        assert!(
            added.contains("  \"aw-watcher-window\",\n  \"aw-awatcher\"\n]"),
            "{added}"
        );

        // Disabling the Python watchers removes them; nothing else changes.
        let removed = set(&added, "aw-watcher-afk", false);
        let removed = set(&removed, "aw-watcher-window", false);
        assert_eq!(modules_of(&removed), ["aw-awatcher"]);
        assert_eq!(
            removed,
            source.replace(
                "  \"aw-watcher-afk\",\n  \"aw-watcher-window\"\n",
                "  \"aw-awatcher\"\n"
            ),
            "only the modules list should change"
        );

        // Removing the last entry, then adding it back, round-trips exactly.
        let without_last = set(&added, "aw-awatcher", false);
        assert_eq!(without_last, source);

        // Already in the requested state: no write.
        assert!(set_module_autostart(&removed, "aw-awatcher", true)
            .unwrap()
            .is_none());
        assert!(set_module_autostart(&removed, "aw-watcher-afk", false)
            .unwrap()
            .is_none());

        // Emptying the list and re-enabling into it.
        let empty = set(&removed, "aw-awatcher", false);
        assert!(modules_of(&empty).is_empty());
        assert!(empty.contains("modules = [\n]"), "{empty}");
        let readded = set(&empty, "aw-watcher-afk", true);
        assert_eq!(modules_of(&readded), ["aw-watcher-afk"]);
        assert!(
            readded.contains("modules = [\n  \"aw-watcher-afk\"\n]"),
            "{readded}"
        );
    }

    #[test]
    fn tray_toggle_keeps_comments_and_moves_inline_args() {
        let source = r#"# my config
port = 5600
discovery_paths = []

[autostart]
enabled = true
minimized = true
# watchers to start
modules = [
  "aw-awatcher",
  { name = "aw-watcher-input", args = "--poll-time 5" }
]
"#;
        let out = set(source, "aw-watcher-input", false);
        assert!(out.contains("# my config") && out.contains("# watchers to start"));
        assert_eq!(modules_of(&out), ["aw-awatcher"]);
        // The args survive so a later manual start from the tray still uses them.
        assert_eq!(parse(&out).module_args["aw-watcher-input"], "--poll-time 5");
    }

    /// Removing an entry whose inline args differ from its `[module_args]`
    /// value keeps the inline args, since those were the ones in effect.
    #[test]
    fn removing_entry_prefers_inline_args_over_module_args() {
        let source = r#"port = 5600
discovery_paths = []

[autostart]
enabled = true
minimized = true
modules = [
  { name = "aw-watcher-input", args = "--poll-time 5" },
]

[module_args]
# input watcher tuning
aw-watcher-input = "--poll-time 60"
aw-awatcher = "--verbose"
"#;
        let out = set(source, "aw-watcher-input", false);
        let config = parse(&out);
        assert!(config.autostart.modules.is_empty());
        assert_eq!(config.module_args["aw-watcher-input"], "--poll-time 5");
        assert_eq!(config.module_args["aw-awatcher"], "--verbose");
        assert!(
            out.contains("# input watcher tuning\naw-watcher-input = \"--poll-time 5\"\n"),
            "{out}"
        );

        // An entry without args leaves `[module_args]` alone.
        let source = source.replace(
            "{ name = \"aw-watcher-input\", args = \"--poll-time 5\" }",
            "\"aw-watcher-input\"",
        );
        let out = set(&source, "aw-watcher-input", false);
        assert_eq!(
            parse(&out).module_args["aw-watcher-input"],
            "--poll-time 60"
        );
    }

    /// Comments before, inside and after entries survive adds and removes,
    /// each exactly once and on the line it was on, with or without a
    /// trailing comma.
    #[test]
    fn tray_toggle_keeps_comments_on_entries() {
        let head =
            "port = 5600\ndiscovery_paths = []\n\n[autostart]\nenabled = true\nminimized = true\n";
        for trailing_comma in [false, true] {
            let comma = if trailing_comma { "," } else { "" };
            let source = format!(
                "{head}modules = [ # started at login\n  # window + afk\n  \"aw-awatcher\", # rust\n  # last one\n  \"aw-watcher-input\"{comma} # input\n  # end of list\n]\n"
            );

            let added = set(&source, "aw-sync", true);
            assert_eq!(
                modules_of(&added),
                ["aw-awatcher", "aw-watcher-input", "aw-sync"],
                "{added}"
            );
            assert_eq!(
                added,
                format!(
                    "{head}modules = [ # started at login\n  # window + afk\n  \"aw-awatcher\", # rust\n  # last one\n  \"aw-watcher-input\", # input\n  \"aw-sync\"{comma}\n  # end of list\n]\n"
                ),
                "trailing_comma={trailing_comma}"
            );
            for c in [
                "# started at login",
                "# window + afk",
                "# rust",
                "# last one",
                "# input",
                "# end of list",
            ] {
                assert_eq!(added.matches(c).count(), 1, "{c} in {added}");
            }

            // Removing the new entry restores the original exactly.
            assert_eq!(
                set(&added, "aw-sync", false),
                source,
                "trailing_comma={trailing_comma}"
            );

            // Removing the last original entry drops its own comments only.
            let removed = set(&source, "aw-watcher-input", false);
            assert_eq!(
                removed,
                format!(
                    "{head}modules = [ # started at login\n  # window + afk\n  \"aw-awatcher\"{comma} # rust\n  # end of list\n]\n"
                ),
                "trailing_comma={trailing_comma}"
            );

            // Removing the first entry keeps the comment after `[`.
            let removed = set(&source, "aw-awatcher", false);
            assert_eq!(modules_of(&removed), ["aw-watcher-input"]);
            assert!(
                removed.contains(
                    "modules = [ # started at login\n  # last one\n  \"aw-watcher-input\""
                ),
                "{removed}"
            );
            assert!(
                !removed.contains("# rust") && !removed.contains("# window + afk"),
                "{removed}"
            );
        }
    }

    #[test]
    fn tray_toggle_edits_single_line_arrays() {
        let head =
            "port = 5600\ndiscovery_paths = []\n\n[autostart]\nenabled = true\nminimized = true\n";
        let source = format!("{head}modules = [\"aw-watcher-afk\", \"aw-watcher-window\"]\n");
        let added = set(&source, "aw-awatcher", true);
        assert!(
            added.contains(
                "modules = [\"aw-watcher-afk\", \"aw-watcher-window\", \"aw-awatcher\"]\n"
            ),
            "{added}"
        );
        let removed = set(&source, "aw-watcher-afk", false);
        assert!(
            removed.contains("modules = [\"aw-watcher-window\"]\n"),
            "{removed}"
        );
        let empty = set(&removed, "aw-watcher-window", false);
        assert!(modules_of(&empty).is_empty());
        let readded = set(&empty, "aw-awatcher", true);
        assert_eq!(modules_of(&readded), ["aw-awatcher"]);
    }

    /// `[[autostart.modules]]` tables (valid config, parsed as `ModuleEntry::Full`).
    #[test]
    fn tray_toggle_edits_array_of_tables() {
        let source = r#"port = 5600
discovery_paths = []

[autostart]
enabled = true
minimized = true

# the window watcher
[[autostart.modules]]
name = "aw-awatcher"

[[autostart.modules]]
name = "aw-watcher-input"
args = "--poll-time 5"

[updates]
auto_download = true
"#;
        let added = set(source, "aw-sync", true);
        assert_eq!(
            modules_of(&added),
            ["aw-awatcher", "aw-watcher-input", "aw-sync"]
        );
        // The new table goes right after the last one, before `[updates]`.
        let sync = added.find("name = \"aw-sync\"").unwrap();
        assert!(added.find("name = \"aw-watcher-input\"").unwrap() < sync);
        assert!(sync < added.find("[updates]").unwrap(), "{added}");
        assert!(added.contains("# the window watcher"));
        assert!(set_module_autostart(&added, "aw-sync", true)
            .unwrap()
            .is_none());

        let removed = set(&added, "aw-watcher-input", false);
        let config = parse(&removed);
        assert_eq!(modules_of(&removed), ["aw-awatcher", "aw-sync"]);
        assert_eq!(config.module_args["aw-watcher-input"], "--poll-time 5");
        assert!(config.updates.auto_download);

        // Removing every table still leaves a config that parses.
        let removed = set(&removed, "aw-awatcher", false);
        let removed = set(&removed, "aw-sync", false);
        assert!(modules_of(&removed).is_empty(), "{removed}");
        assert!(removed.contains("\nmodules = []\n"), "{removed}");
        let readded = set(&removed, "aw-awatcher", true);
        assert_eq!(modules_of(&readded), ["aw-awatcher"]);
    }

    /// A module listed twice: the last entry's args are the effective ones, so
    /// those are what removal saves, for both list forms.
    #[test]
    fn removing_duplicate_entries_keeps_effective_args() {
        let head =
            "port = 5600\ndiscovery_paths = []\n\n[autostart]\nenabled = true\nminimized = true\n";
        let inline = format!(
            "{head}modules = [\n  {{ name = \"aw-watcher-input\", args = \"--first\" }},\n  \"aw-awatcher\",\n  {{ name = \"aw-watcher-input\", args = \"--last\" }},\n  \"aw-watcher-input\"\n]\n"
        );
        let tables = format!(
            "{head}\n[[autostart.modules]]\nname = \"aw-watcher-input\"\nargs = \"--first\"\n\n[[autostart.modules]]\nname = \"aw-awatcher\"\n\n[[autostart.modules]]\nname = \"aw-watcher-input\"\nargs = \"--last\"\n"
        );
        for source in [inline, tables] {
            let out = set(&source, "aw-watcher-input", false);
            assert_eq!(modules_of(&out), ["aw-awatcher"], "{out}");
            assert_eq!(
                parse(&out).module_args["aw-watcher-input"],
                "--last",
                "{out}"
            );
        }
    }

    /// The last entry has no args, so the effective inline args are an earlier
    /// entry's (they still beat `[module_args]`); those are what removal saves.
    #[test]
    fn removing_duplicates_saves_last_entry_with_args() {
        let source = r#"port = 5600
discovery_paths = []

[autostart]
enabled = true
minimized = true
modules = [ { name = "aw-watcher-input", args = "--first" }, "aw-watcher-input" ]

[module_args]
aw-watcher-input = "--poll-time 60"
"#;
        let out = set(source, "aw-watcher-input", false);
        assert!(modules_of(&out).is_empty(), "{out}");
        assert_eq!(
            parse(&out).module_args["aw-watcher-input"],
            "--first",
            "{out}"
        );
    }

    #[test]
    fn tray_toggle_leaves_malformed_config_alone() {
        assert!(set_module_autostart("port = \n[autostart", "aw-awatcher", true).is_err());
    }

    #[test]
    fn write_atomic_replaces_file_and_cleans_up() {
        let dir = scratch_dir("atomic");
        let path = dir.join("config.toml");
        std::fs::write(&path, "old").unwrap();
        write_atomic(&path, "new contents").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new contents");
        // Writing a missing file creates it.
        let fresh = dir.join("fresh.toml");
        write_atomic(&fresh, "x").unwrap();
        assert_eq!(std::fs::read_to_string(&fresh).unwrap(), "x");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");

        #[cfg(unix)]
        {
            // A symlinked config is written through to its target.
            let real = dir.join("real.toml");
            std::fs::write(&real, "old").unwrap();
            let link = dir.join("link.toml");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            write_atomic(&link, "via link").unwrap();
            assert!(std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(std::fs::read_to_string(&real).unwrap(), "via link");

            // A dangling link (first run, target not created yet) keeps the link
            // and creates its target, also through a relative link.
            let link = dir.join("dangling.toml");
            std::os::unix::fs::symlink("target.toml", &link).unwrap();
            write_atomic(&link, "first run").unwrap();
            assert!(std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(
                std::fs::read_to_string(dir.join("target.toml")).unwrap(),
                "first run"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
