//! Append a freshly downloaded GGUF to the llm-control `models.yaml` registry.
//!
//! Opt-in via the `LLMFIT_REGISTRY_YAML` environment variable; no-op (returns
//! `Ok(false)`) when the variable is unset. This keeps the upstream repo
//! unaffected for non-Pc-control users.
//!
//! **Comment preservation note:** `serde_yml` round-trips data values but drops
//! YAML `#` comments. The current models.yaml uses `notes:` fields (real YAML
//! values) rather than `#` comments, so round-tripping is safe. If
//! load-bearing `#` comments are added later, switch to an append-only text
//! writer.

use std::collections::HashMap;

/// Append a downloaded GGUF to the llm-control models.yaml registry.
///
/// * `gguf_filename` — basename only, e.g. `"Llama-3.1-8B-Instruct-Q4_K_M.gguf"`
/// * `source_repo`  — HuggingFace repo id, e.g. `"bartowski/Llama-3.1-8B-Instruct-GGUF"`
/// * `_model`       — (reserved) catalog metadata if matched; currently unused (v1 uses
///   file-derived defaults; threading `LlmModel` is deferred to v2)
///
/// Returns:
/// * `Ok(true)`  — entry appended
/// * `Ok(false)` — skipped (env var unset, or duplicate id/path)
/// * `Err(msg)`  — I/O or YAML parse error (caller must NOT fail the download for this)
pub fn register_model_yaml(
    gguf_filename: &str,
    source_repo: &str,
    _model: Option<&crate::models::LlmModel>,
) -> Result<bool, String> {
    // Only run when the env var is explicitly set.
    let yaml_path = match std::env::var("LLMFIT_REGISTRY_YAML") {
        Ok(p) if !p.is_empty() => std::path::PathBuf::from(p),
        _ => return Ok(false),
    };

    // Derive fields from the filename.
    let stem = gguf_filename
        .strip_suffix(".gguf")
        .unwrap_or(gguf_filename);

    let id = slug(stem);
    let display_name = stem.replace('-', " ").replace('_', " ");
    let family = derive_family(stem);
    let path = format!("/models/{gguf_filename}");
    let quantization = parse_quantization(stem);

    // Defaults for ctx / vram (v1; v2 will enrich from LlmModel).
    let ctx: u64 = 8192;
    let vram_estimate_mb: u64 = {
        // Conservative: file size in MiB + 1024 MiB KV headroom.
        match std::fs::metadata(
            std::path::Path::new(&std::env::var("LLMFIT_MODELS_DIR").unwrap_or_default())
                .join(gguf_filename),
        ) {
            Ok(meta) => meta.len() / (1024 * 1024) + 1024,
            Err(_) => 6144, // 6 GiB safe default when file not found
        }
    };

    let today = today_str();
    let notes = format!(
        "Auto-added by llmfit install on {today} from {source_repo}. \
         NOT wired into llama-swap.yaml — set an alias to load."
    );

    // Load existing YAML (or start with an empty list).
    let raw = if yaml_path.exists() {
        std::fs::read_to_string(&yaml_path)
            .map_err(|e| format!("registry_yaml: read {}: {e}", yaml_path.display()))?
    } else {
        String::new()
    };

    // Parse as generic YAML mapping with a `models` list.
    let mut doc: serde_yml::Value = if raw.trim().is_empty() {
        serde_yml::Value::Mapping(serde_yml::Mapping::new())
    } else {
        serde_yml::from_str(&raw)
            .map_err(|e| format!("registry_yaml: parse {}: {e}", yaml_path.display()))?
    };

    // Ensure `doc` is a mapping.
    if !doc.is_mapping() {
        return Err(format!(
            "registry_yaml: {} is not a YAML mapping",
            yaml_path.display()
        ));
    }

    // Get or create the `models` sequence.
    let models_key = serde_yml::Value::String("models".to_string());
    if doc.get(&models_key).is_none() {
        doc[&models_key] = serde_yml::Value::Sequence(serde_yml::Sequence::new());
    }

    let models_seq = doc[&models_key]
        .as_sequence_mut()
        .ok_or_else(|| format!("registry_yaml: `models` in {} is not a sequence", yaml_path.display()))?;

    // Idempotency check: skip if id or path already present.
    for entry in models_seq.iter() {
        if let Some(existing_id) = entry.get("id").and_then(|v| v.as_str()) {
            if existing_id == id {
                return Ok(false);
            }
        }
        if let Some(existing_path) = entry.get("path").and_then(|v| v.as_str()) {
            if existing_path == path {
                return Ok(false);
            }
        }
    }

    // Build the new entry.
    let mut entry_map: HashMap<String, serde_yml::Value> = HashMap::new();
    entry_map.insert("id".to_string(), sv(id));
    entry_map.insert("display_name".to_string(), sv(display_name));
    entry_map.insert("family".to_string(), sv(family));
    entry_map.insert("path".to_string(), sv(path));
    entry_map.insert("quantization".to_string(), sv(quantization));
    entry_map.insert("ctx".to_string(), serde_yml::Value::Number(serde_yml::Number::from(ctx)));
    entry_map.insert(
        "vram_estimate_mb".to_string(),
        serde_yml::Value::Number(serde_yml::Number::from(vram_estimate_mb)),
    );
    entry_map.insert(
        "reasoning_capable".to_string(),
        serde_yml::Value::Bool(false),
    );
    entry_map.insert(
        "reasoning_enabled".to_string(),
        serde_yml::Value::Bool(false),
    );
    entry_map.insert("notes".to_string(), sv(notes));

    // Convert HashMap to serde_yml::Mapping (preserving insertion order via sort).
    let mut mapping = serde_yml::Mapping::new();
    // Insert in the canonical field order.
    for key in &[
        "id",
        "display_name",
        "family",
        "path",
        "quantization",
        "ctx",
        "vram_estimate_mb",
        "reasoning_capable",
        "reasoning_enabled",
        "notes",
    ] {
        if let Some(val) = entry_map.remove(*key) {
            mapping.insert(serde_yml::Value::String(key.to_string()), val);
        }
    }
    models_seq.push(serde_yml::Value::Mapping(mapping));

    // Atomic write: temp file in same dir → rename.
    let parent = yaml_path.parent().ok_or_else(|| {
        format!("registry_yaml: {} has no parent directory", yaml_path.display())
    })?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("registry_yaml: create dir {}: {e}", parent.display()))?;

    let tmp_path = yaml_path.with_extension("yaml.tmp");
    let serialized = serde_yml::to_string(&doc)
        .map_err(|e| format!("registry_yaml: serialize: {e}"))?;
    std::fs::write(&tmp_path, &serialized)
        .map_err(|e| format!("registry_yaml: write tmp {}: {e}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, &yaml_path)
        .map_err(|e| format!("registry_yaml: rename to {}: {e}", yaml_path.display()))?;

    Ok(true)
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn sv(s: impl Into<String>) -> serde_yml::Value {
    serde_yml::Value::String(s.into())
}

/// Convert a GGUF stem to a kebab-case id slug.
/// e.g. `"Llama-3.1-8B-Instruct-Q4_K_M"` → `"llama-3-1-8b-instruct-q4-k-m"`
fn slug(stem: &str) -> String {
    let lower = stem.to_lowercase();
    // Replace non-alphanumeric chars with '-', collapse consecutive '-'.
    let mut result = String::with_capacity(lower.len());
    let mut prev_dash = false;
    for ch in lower.chars() {
        if ch.is_ascii_alphanumeric() {
            result.push(ch);
            prev_dash = false;
        } else {
            if !prev_dash {
                result.push('-');
            }
            prev_dash = true;
        }
    }
    // Trim leading/trailing dashes.
    result.trim_matches('-').to_string()
}

/// Derive the model family from the GGUF stem.
/// Take the first '-'-separated token before a size token (e.g. "8b", "70b").
fn derive_family(stem: &str) -> String {
    let lower = stem.to_lowercase();
    let parts: Vec<&str> = lower.split('-').collect();
    // Find the first part that looks like a size token (digits followed by 'b').
    let stop = parts.iter().position(|p| {
        let p = p.trim_start_matches(|c: char| c.is_ascii_digit());
        p == "b" || p.starts_with('b')
    });
    let take = stop.unwrap_or(1).max(1);
    parts[..take].join("-")
}

/// Extract the quantization string from a GGUF stem.
/// Regex equivalent: `(?i)(Q\d[_A-Z0-9]*|IQ\d[_A-Z]*|F16|BF16)`.
fn parse_quantization(stem: &str) -> String {
    let upper = stem.to_uppercase();
    // Walk tokens split on '-' or '_'.
    for token in upper.split(|c: char| c == '-' || c == '_') {
        if token == "F16" || token == "BF16" {
            return token.to_string();
        }
        if token.starts_with("IQ") && token.len() > 2 && token.chars().nth(2).map_or(false, |c| c.is_ascii_digit()) {
            return token.to_string();
        }
        if token.starts_with('Q') && token.len() > 1 && token.chars().nth(1).map_or(false, |c| c.is_ascii_digit()) {
            return token.to_string();
        }
    }
    "unknown".to_string()
}

fn today_str() -> String {
    // Use std only (no chrono dep).  If the OS call fails, fall back to "unknown".
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Days since epoch → date arithmetic.
    let days = secs / 86400;
    let (mut year, mut month, mut day) = (1970u64, 1u64, 1u64);
    let mut remaining = days;
    loop {
        let leap = if year % 400 == 0 {
            366
        } else if year % 100 == 0 {
            365
        } else if year % 4 == 0 {
            366
        } else {
            365
        };
        if remaining < leap {
            break;
        }
        remaining -= leap;
        year += 1;
    }
    let leap_year = year % 400 == 0 || (year % 4 == 0 && year % 100 != 0);
    let days_in_month = [
        31u64,
        if leap_year { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    for &dim in &days_in_month {
        if remaining < dim {
            day = remaining + 1;
            break;
        }
        remaining -= dim;
        month += 1;
    }
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_converts_correctly() {
        assert_eq!(
            slug("Llama-3.1-8B-Instruct-Q4_K_M"),
            "llama-3-1-8b-instruct-q4-k-m"
        );
        assert_eq!(slug("Qwen2.5-7B-Instruct-Q5_K_M"), "qwen2-5-7b-instruct-q5-k-m");
    }

    #[test]
    fn parse_quantization_extracts_known_types() {
        assert_eq!(parse_quantization("Llama-3.1-8B-Instruct-Q4_K_M"), "Q4");
        assert_eq!(parse_quantization("Model-IQ3_XS"), "IQ3");
        assert_eq!(parse_quantization("Model-F16"), "F16");
        assert_eq!(parse_quantization("Model-BF16"), "BF16");
        assert_eq!(parse_quantization("Model-unknown-quant"), "unknown");
    }

    #[test]
    fn derive_family_extracts_first_segment() {
        assert_eq!(derive_family("Llama-3-1-8B-Instruct-Q4"), "llama");
        assert_eq!(derive_family("Qwen2-5-7B-Chat"), "qwen2");
    }

    #[test]
    fn register_noop_when_env_unset() {
        // Ensure env var is absent.
        // SAFETY: test-only single-threaded env mutation (edition 2024 marks
        // env::set_var/remove_var unsafe due to cross-thread data-race risk).
        unsafe { std::env::remove_var("LLMFIT_REGISTRY_YAML") };
        let result = register_model_yaml("some-model-Q4_K_M.gguf", "org/repo", None);
        assert_eq!(result, Ok(false));
    }

    #[test]
    fn register_deduplicates_by_id() {
        use std::io::Write;
        let dir = std::env::temp_dir().join("llmfit_test_dedup");
        std::fs::create_dir_all(&dir).unwrap();
        let yaml_path = dir.join("models.yaml");

        // Seed with an entry that has the same id as what we'll try to add.
        let existing = "models:\n  - id: llama-3-1-8b-instruct-q4-k-m\n    path: /models/x.gguf\n";
        let mut f = std::fs::File::create(&yaml_path).unwrap();
        f.write_all(existing.as_bytes()).unwrap();

        // SAFETY: test-only single-threaded env mutation (edition 2024 marks
        // env::set_var/remove_var unsafe due to cross-thread data-race risk).
        unsafe { std::env::set_var("LLMFIT_REGISTRY_YAML", yaml_path.to_str().unwrap()) };
        let result = register_model_yaml("Llama-3.1-8B-Instruct-Q4_K_M.gguf", "org/repo", None);
        unsafe { std::env::remove_var("LLMFIT_REGISTRY_YAML") };

        assert_eq!(result, Ok(false)); // skipped
    }
}
