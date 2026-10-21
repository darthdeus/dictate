//! Declarative settings: defaults < config file < environment.
//!
//! Ports `lib/dictate-overlay/config.py` + the loader block at the top of
//! `bin/dictate`. The file is TOML *data*, never shell code: single-line
//! strings, finite numbers, or booleans. Unknown keys and non-single-line
//! values fail before capture starts, without echoing values.

use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Every `DICTATE_*` key the application understands.
pub fn known_keys() -> HashSet<&'static str> {
    [
        "DICTATE_CHAN",
        "DICTATE_CHANNELS",
        "DICTATE_CLIPBOARD",
        "DICTATE_CORRECT",
        "DICTATE_CORRECT_KEY",
        "DICTATE_CORRECT_MODEL",
        "DICTATE_CORRECT_TIMEOUT",
        "DICTATE_CORRECT_URL", "DICTATE_EARLY", "DICTATE_ELEVENLABS_MODEL", "DICTATE_ELEVENLABS_TIMEOUT", "DICTATE_FINAL", "DICTATE_SHADOW", "DICTATE_SHADOW_MAX_MIN", "DICTATE_PRICE_ELEVENLABS_HOUR", "DICTATE_PRICE_GROQ_IN", "DICTATE_PRICE_GROQ_OUT", "DICTATE_EARLY_LEVEL", "DICTATE_EARLY_PAUSE_MS", "DICTATE_INCREMENTAL", "DICTATE_SEGMENT_MIN_MS",
        "DICTATE_DIR",
        "DICTATE_ENGINE",
        "DICTATE_FILTER",
        "DICTATE_HISTORY",
        "DICTATE_HISTORY_DAYS",
        "DICTATE_HISTORY_DIR",
        "DICTATE_HISTORY_MAX",
        "DICTATE_INPUT",
        "DICTATE_LANG",
        "DICTATE_LLM",
        "DICTATE_LLM_KEY",
        "DICTATE_LLM_MODEL",
        "DICTATE_LLM_TIMEOUT",
        "DICTATE_LLM_URL",
        "DICTATE_MAX_SECS",
        "DICTATE_MIC",
        "DICTATE_MIN_PEAK",
        "DICTATE_MODEL",
        "DICTATE_NOISE",
        "DICTATE_NOTYPE",
        "DICTATE_OSD_BACKEND",
        "DICTATE_OSD_BG",
        "DICTATE_OSD_DONE_MS",
        "DICTATE_OSD_FG",
        "DICTATE_OSD_FONT",
        "DICTATE_OSD_HEIGHT",
        "DICTATE_OSD_MOTION",
        "DICTATE_OSD_SIZE",
        "DICTATE_OSD_SLIDE_DROP",
        "DICTATE_OSD_SLIDE_STEPS",
        "DICTATE_OSD_SLIDE_STEP_DELAY",
        "DICTATE_OSD_WIDTH",
        "DICTATE_PARAKEET_MODEL",
        "DICTATE_PASTE_KEY_DELAY",
        "DICTATE_PASTE_SETTLE",
        "DICTATE_PROMPT",
        "DICTATE_PREVIEW_BOOST",
        "DICTATE_PREVIEW_STOP",
        "DICTATE_PREVIEW_TERMS",
        "DICTATE_PW",
        "DICTATE_SERVER",
        "DICTATE_STREAM",
        "DICTATE_STREAM_GATE",
        "DICTATE_STREAM_GATE_DB",
        "DICTATE_STREAM_INTERVAL",
        "DICTATE_STREAM_MODEL",
        "DICTATE_STREAM_PAUSE",
        "DICTATE_STREAM_RIGHT",
        "DICTATE_STYLE",
        "DICTATE_TERMINALS",
        "DICTATE_TERMS",
        "DICTATE_THREADS",
        "DICTATE_TYPE_DELAY",
        "DICTATE_VAD",
        "DICTATE_VAD_MODEL",
        "DICTATE_VAD_THRESHOLD",
    ]
    .into_iter()
    .collect()
}

/// Snapshot of the process environment's `DICTATE_*` entries, so tests can
/// inject values without touching the real environment.
#[derive(Clone, Default, Debug)]
pub struct Env {
    vars: HashMap<String, String>,
    /// `None` = `DICTATE_CONFIG` unset (use default path, missing file OK).
    /// `Some("")` = explicitly disabled. `Some(path)` = explicit file.
    pub config_override: Option<String>,
    pub xdg_config_home: Option<String>,
    pub home: Option<String>,
}

impl Env {
    pub fn from_process() -> Self {
        Self {
            vars: std::env::vars()
                .filter(|(k, _)| k.starts_with("DICTATE_"))
                .collect(),
            config_override: std::env::var("DICTATE_CONFIG").ok(),
            xdg_config_home: std::env::var("XDG_CONFIG_HOME").ok(),
            home: std::env::var("HOME").ok(),
        }
    }

    pub fn with_vars(vars: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            vars: vars.into_iter().collect(),
            ..Default::default()
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }
}

/// Effective configuration after defaults < file < env.
#[derive(Clone, Debug)]
pub struct Config {
    values: HashMap<String, String>,
}

impl Config {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    pub fn get_or(&self, key: &str, default: &str) -> String {
        self.get(key).unwrap_or(default).to_string()
    }

    /// The renderer's settings (`DICTATE_OSD_*`), file values included: the
    /// renderer only sees its environment.
    pub fn osd_vars(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values.iter().filter(|(k, _)| k.starts_with("DICTATE_OSD_")).map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Re-expose merged values as an `Env` (for path resolution, which reads
    /// `DICTATE_DIR` / `DICTATE_HISTORY*` with defaults < file < env applied).
    pub fn as_env(&self, base: &Env) -> Env {
        let mut vars = self.values.clone();
        vars.remove("DICTATE_CONFIG");
        Env {
            vars,
            config_override: base.config_override.clone(),
            xdg_config_home: base.xdg_config_home.clone(),
            home: base.home.clone(),
        }
    }

    pub fn history_enabled(&self) -> bool {
        self.get_or("DICTATE_HISTORY", "1") == "1"
    }

    pub fn history_dir(&self) -> PathBuf {
        if let Some(dir) = self.get("DICTATE_HISTORY_DIR") {
            return PathBuf::from(dir);
        }
        let base = std::env::var("XDG_STATE_HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".local/state"))
            })
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        base.join("dictate/history")
    }
}

/// Parse one TOML file into `(key, value)` pairs. Mirrors `config.py`.
pub fn read_config_file(path: &Path) -> Result<Vec<(String, String)>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("dictate: cannot read configuration file {}", path.display()))?;
    let parsed: toml::Value = toml::from_str(&text)
        .with_context(|| "dictate: invalid TOML configuration".to_string())?;
    let table = parsed
        .as_table()
        .context("dictate: invalid TOML configuration")?;
    let known = known_keys();
    let mut out = Vec::new();
    for (key, value) in table {
        if !known.contains(key.as_str()) {
            bail!("dictate: Unknown setting: {key}");
        }
        let s = match value {
            toml::Value::Boolean(b) => {
                if *b {
                    "1".to_string()
                } else {
                    "0".to_string()
                }
            }
            toml::Value::Integer(i) => i.to_string(),
            toml::Value::Float(f) => {
                if !f.is_finite() {
                    bail!("dictate: Non-finite number for {key}");
                }
                // Match Python str(float) closely enough for config use.
                format!("{f}")
            }
            toml::Value::String(s) => s.clone(),
            _ => bail!("dictate: Expected a string, number, or boolean for {key}"),
        };
        if s.contains(['\n', '\r', '\t', '\0']) {
            bail!("dictate: Expected a single-line value for {key}");
        }
        out.push((key.clone(), s));
    }
    Ok(out)
}

fn default_config_path(env: &Env) -> PathBuf {
    let base = env
        .xdg_config_home
        .clone()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env.home
                .clone()
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("dictate/config.toml")
}

/// Load with bash-equivalent semantics:
/// - `DICTATE_CONFIG=""` disables file loading.
/// - unset `DICTATE_CONFIG`: default path, missing file is fine.
/// - explicit path: missing file is an error.
/// - explicit env values (including empty) override the file.
pub fn load_from_env(env: &Env) -> Result<Config> {
    let mut values: HashMap<String, String> = HashMap::new();
    let explicit = env.config_override.is_some();

    let path: Option<PathBuf> = match &env.config_override {
        None => {
            let p = default_config_path(env);
            if p.exists() || std::fs::symlink_metadata(&p).is_ok() {
                Some(p)
            } else {
                None
            }
        }
        Some(s) if s.is_empty() => None,
        Some(s) => Some(PathBuf::from(s)),
    };

    if let Some(p) = path {
        // Explicit missing file must fail; default missing file was filtered above.
        if explicit && !p.exists() && std::fs::symlink_metadata(&p).is_err() {
            bail!("dictate: cannot read configuration file {}", p.display());
        }
        for (k, v) in read_config_file(&p)? {
            values.insert(k, v);
        }
    }

    for (k, v) in &env.vars {
        if k == "DICTATE_CONFIG" {
            continue;
        }
        values.insert(k.clone(), v.clone());
    }
    Ok(Config { values })
}

/// Load from the real process environment.
pub fn load() -> Result<Config> {
    load_from_env(&Env::from_process())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn env_with_tmp(
        tmp: &Path,
        vars: impl IntoIterator<Item = (String, String)>,
        config: Option<&str>,
    ) -> Env {
        let mut e = Env::with_vars(vars);
        e.xdg_config_home = Some(tmp.to_string_lossy().into_owned());
        e.home = Some(tmp.to_string_lossy().into_owned());
        e.config_override = config.map(str::to_string);
        e
    }

    #[test]
    fn absent_default_config_is_optional() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = load_from_env(&env_with_tmp(tmp.path(), [], None)).unwrap();
        assert_eq!(cfg.get("DICTATE_STYLE"), None);
    }

    #[test]
    fn explicit_missing_file_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("config-missing.toml");
        let err = load_from_env(&env_with_tmp(
            tmp.path(),
            [],
            Some(missing.to_str().unwrap()),
        ))
        .unwrap_err();
        assert!(err.to_string().contains("cannot read"), "{err}");
    }

    #[test]
    fn empty_config_disables_loading() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = load_from_env(&env_with_tmp(tmp.path(), [], Some(""))).unwrap();
        assert_eq!(cfg.get("DICTATE_STYLE"), None);
    }

    #[test]
    fn env_precedence_empty_overrides_and_booleans() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("c.toml");
        std::fs::write(
            &path,
            "DICTATE_CHANNELS = 10\nDICTATE_STYLE = true\nDICTATE_SERVER = \"\"\nDICTATE_PROMPT = \"hello\"\n",
        )
        .unwrap();
        let cfg = load_from_env(&env_with_tmp(
            tmp.path(),
            [
                ("DICTATE_CHANNELS".into(), "2".into()),
                ("DICTATE_PROMPT".into(), "".into()),
            ],
            Some(path.to_str().unwrap()),
        ))
        .unwrap();
        assert_eq!(cfg.get("DICTATE_CHANNELS"), Some("2"));
        assert_eq!(cfg.get("DICTATE_STYLE"), Some("1"));
        assert_eq!(cfg.get("DICTATE_PROMPT"), Some(""));
        assert_eq!(cfg.get("DICTATE_SERVER"), Some(""));
    }

    #[test]
    fn literal_text_not_executed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("c.toml");
        std::fs::write(
            &path,
            "DICTATE_PROMPT = \"$(touch SHOULD_NOT_EXIST) `literal` \u{10d}esky\"\n",
        )
        .unwrap();
        let cfg = load_from_env(&env_with_tmp(
            tmp.path(),
            [],
            Some(path.to_str().unwrap()),
        ))
        .unwrap();
        assert_eq!(
            cfg.get("DICTATE_PROMPT"),
            Some("$(touch SHOULD_NOT_EXIST) `literal` \u{10d}esky")
        );
        assert!(!tmp.path().join("SHOULD_NOT_EXIST").exists());
        assert!(!Path::new("SHOULD_NOT_EXIST").exists());
    }

    #[test]
    fn rejects_unknown_type_and_multiline() {
        for bad in [
            "DICTATE_TYPO = 1",
            "DICTATE_STYLE = [1]",
            "DICTATE_PROMPT = \"line\nline\"",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("c.toml");
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(bad.as_bytes()).unwrap();
            drop(f);
            assert!(read_config_file(&path).is_err(), "{bad}");
        }
    }

    #[test]
    fn error_never_echoes_secret_value() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("c.toml");
        std::fs::write(&path, "DICTATE_PROMPT = \"secret-unclosed\n").unwrap();
        let err = read_config_file(&path).unwrap_err();
        assert!(!err.to_string().contains("secret-unclosed"), "{err}");
    }
}
