use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use tong_core::error::{IoContext, Result, TongError};
use tong_core::paths;

mod dependency;
mod types;

use dependency::{dependency_table, parse_dependency, parse_source_spec};

pub use types::{
    BinTarget, Dependency, DependencySource, LibTarget, Manifest, ManifestKind, Package, SourceSpec,
};

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Section {
    Root,
    Package,
    Lib,
    Bin(usize),
    Dependencies,
    Dependency(String),
    Features,
    Tong,
    TongSource(String),
    Other(String),
}

#[derive(Debug, Clone)]
enum TomlValue {
    String(String),
    Bool(bool),
    Array(Vec<TomlValue>),
    InlineTable(BTreeMap<String, TomlValue>),
    Other(String),
}

impl Manifest {
    pub fn discover(start: &Path) -> Result<PathBuf> {
        let start = if start.as_os_str().is_empty() {
            Path::new(".")
        } else {
            start
        };

        if start.is_file() {
            return paths::canonicalize(start);
        }

        let start = paths::canonicalize(start)?;
        let mut current = Some(start.as_path());
        while let Some(dir) = current {
            let tong = dir.join("Tong.toml");
            if tong.exists() {
                return paths::canonicalize(&tong);
            }

            let cargo = dir.join("Cargo.toml");
            if cargo.exists() {
                return paths::canonicalize(&cargo);
            }

            current = dir.parent();
        }

        Err(TongError::unsupported(format!(
            "no Tong.toml or Cargo.toml found from {}",
            start.display()
        )))
    }

    pub fn load(path: &Path) -> Result<Self> {
        let path = paths::canonicalize(path)?;
        let kind = manifest_kind(&path)?;
        let raw = fs::read_to_string(&path)
            .with_context(format!("failed to read manifest {}", path.display()))?;

        if matches!(kind, ManifestKind::Tong)
            && let Some(extends) = parse_tong_extends(&path, &raw)?
        {
            let base = path
                .parent()
                .ok_or_else(|| {
                    TongError::invalid_manifest(path.clone(), "manifest has no parent directory")
                })?
                .join(extends);
            return Self::load(&base);
        }

        parse_manifest(path, kind, &raw)
    }
}

impl ManifestKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cargo => "Cargo.toml",
            Self::Tong => "Tong.toml",
        }
    }
}

fn manifest_kind(path: &Path) -> Result<ManifestKind> {
    match path.file_name().and_then(|name| name.to_str()) {
        Some("Cargo.toml") => Ok(ManifestKind::Cargo),
        Some("Tong.toml") => Ok(ManifestKind::Tong),
        _ => Err(TongError::invalid_manifest(
            path.to_path_buf(),
            "manifest must be named Cargo.toml or Tong.toml",
        )),
    }
}

fn parse_tong_extends(path: &Path, raw: &str) -> Result<Option<String>> {
    let mut section = Section::Root;
    for (line_no, line) in logical_lines(raw) {
        if line.starts_with('[') {
            section = parse_section(path, line_no, &line)?;
            continue;
        }

        if section != Section::Tong {
            continue;
        }

        let Some((key, value)) = split_key_value(&line) else {
            return Err(TongError::parse(
                path.to_path_buf(),
                line_no,
                "expected key = value",
            ));
        };

        if key == "extends" {
            return Ok(Some(parse_string(path, line_no, value)?.to_owned()));
        }
    }
    Ok(None)
}

fn parse_manifest(path: PathBuf, kind: ManifestKind, raw: &str) -> Result<Manifest> {
    let root = path
        .parent()
        .ok_or_else(|| TongError::invalid_manifest(path.clone(), "manifest has no parent"))?
        .to_path_buf();

    let mut section = Section::Root;
    let mut package_values = BTreeMap::new();
    let mut lib_values = BTreeMap::new();
    let mut bin_values: Vec<BTreeMap<String, TomlValue>> = Vec::new();
    let mut dependency_values: BTreeMap<String, BTreeMap<String, TomlValue>> = BTreeMap::new();
    let mut features = BTreeMap::new();
    let mut source_values: BTreeMap<String, BTreeMap<String, TomlValue>> = BTreeMap::new();

    for (line_no, line) in logical_lines(raw) {
        if line.starts_with('[') {
            section = parse_section(&path, line_no, &line)?;
            if matches!(section, Section::Bin(_)) {
                bin_values.push(BTreeMap::new());
                section = Section::Bin(bin_values.len() - 1);
            }
            continue;
        }

        if matches!(section, Section::Other(_)) {
            continue;
        }

        let Some((key, value)) = split_key_value(&line) else {
            return Err(TongError::parse(
                path.clone(),
                line_no,
                "expected key = value",
            ));
        };

        match &section {
            Section::Package => {
                if matches!(key, "name" | "version" | "edition" | "build") {
                    package_values.insert(key.to_owned(), parse_value(&path, line_no, value)?);
                }
            }
            Section::Lib => {
                if matches!(key, "name" | "path" | "proc-macro") {
                    lib_values.insert(key.to_owned(), parse_value(&path, line_no, value)?);
                }
            }
            Section::Bin(index) => {
                let values = bin_values.get_mut(*index).ok_or_else(|| {
                    TongError::parse(path.clone(), line_no, "internal parser error for [[bin]]")
                })?;
                if matches!(key, "name" | "path") {
                    values.insert(key.to_owned(), parse_value(&path, line_no, value)?);
                }
            }
            Section::Dependencies => {
                let value = parse_value(&path, line_no, value)?;
                dependency_values.insert(key.to_owned(), dependency_table(value));
            }
            Section::Dependency(alias) => {
                dependency_values
                    .entry(alias.clone())
                    .or_default()
                    .insert(key.to_owned(), parse_value(&path, line_no, value)?);
            }
            Section::Features => {
                features.insert(key.to_owned(), parse_string_array(&path, line_no, value)?);
            }
            Section::TongSource(name) => {
                source_values
                    .entry(name.clone())
                    .or_default()
                    .insert(key.to_owned(), parse_value(&path, line_no, value)?);
            }
            _ => {}
        }
    }

    let name = required_string(&path, &package_values, "package.name")?;
    let version = optional_string(&package_values, "version").unwrap_or_else(|| "0.0.0".to_owned());
    let edition = optional_string(&package_values, "edition").unwrap_or_else(|| "2021".to_owned());
    let package = Package {
        name,
        version,
        edition,
    };

    let dependencies = dependency_values
        .into_iter()
        .map(|(alias, values)| parse_dependency(&path, &root, alias, values))
        .collect::<Result<Vec<_>>>()?;
    let sources = source_values
        .into_iter()
        .map(|(name, values)| {
            parse_source_spec(&path, &root, name, &values).map(|spec| (spec.0, spec.1))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let build_script = discover_build_script(&root, &package_values);
    let lib = discover_lib(&root, &package, &lib_values);
    let bins = discover_bins(&root, &package, &bin_values);

    if lib.is_none() && bins.is_empty() {
        return Err(TongError::invalid_manifest(
            path,
            "no Rust targets found; expected src/lib.rs, src/main.rs, [lib], or [[bin]]",
        ));
    }

    Ok(Manifest {
        path,
        root,
        kind,
        package,
        features,
        sources,
        build_script,
        lib,
        bins,
        dependencies,
    })
}

fn discover_build_script(root: &Path, values: &BTreeMap<String, TomlValue>) -> Option<PathBuf> {
    match values.get("build") {
        Some(TomlValue::String(path)) => Some(root.join(path)),
        Some(TomlValue::Bool(false)) => None,
        Some(TomlValue::Other(value)) if value == "false" => None,
        _ => {
            let default = root.join("build.rs");
            default.exists().then_some(default)
        }
    }
}

fn discover_lib(
    root: &Path,
    package: &Package,
    values: &BTreeMap<String, TomlValue>,
) -> Option<LibTarget> {
    let default = root.join("src/lib.rs");
    if values.is_empty() && !default.exists() {
        return None;
    }

    let name = optional_string(values, "name")
        .unwrap_or_else(|| paths::normalize_crate_name(&package.name));
    let path = optional_string(values, "path")
        .map(|path| root.join(path))
        .unwrap_or(default);
    let proc_macro = optional_bool(values, "proc-macro").unwrap_or(false);

    Some(LibTarget {
        name,
        path,
        proc_macro,
    })
}

fn discover_bins(
    root: &Path,
    package: &Package,
    bins: &[BTreeMap<String, TomlValue>],
) -> Vec<BinTarget> {
    if bins.is_empty() {
        let default = root.join("src/main.rs");
        if default.exists() {
            return vec![BinTarget {
                name: package.name.clone(),
                path: default,
            }];
        }
        return Vec::new();
    }

    bins.iter()
        .map(|values| {
            let name = optional_string(values, "name").unwrap_or_else(|| package.name.clone());
            let path = optional_string(values, "path")
                .map(|path| root.join(path))
                .unwrap_or_else(|| root.join(format!("src/bin/{name}.rs")));
            BinTarget { name, path }
        })
        .collect()
}

fn parse_section(path: &Path, line: usize, value: &str) -> Result<Section> {
    if value.starts_with("[[") {
        if !value.ends_with("]]") {
            return Err(TongError::parse(
                path.to_path_buf(),
                line,
                "unterminated array section",
            ));
        }
        let name = value.trim_start_matches("[[").trim_end_matches("]]").trim();
        return match name {
            "bin" => Ok(Section::Bin(0)),
            other if other.starts_with("tong.sources.") => Ok(Section::TongSource(
                other.trim_start_matches("tong.sources.").to_owned(),
            )),
            other => Ok(Section::Other(other.to_owned())),
        };
    }

    if !value.ends_with(']') {
        return Err(TongError::parse(
            path.to_path_buf(),
            line,
            "unterminated section",
        ));
    }
    let name = value.trim_start_matches('[').trim_end_matches(']').trim();
    Ok(match name {
        "package" => Section::Package,
        "lib" => Section::Lib,
        "dependencies" => Section::Dependencies,
        name if name.starts_with("dependencies.") => {
            Section::Dependency(name.trim_start_matches("dependencies.").to_owned())
        }
        "features" => Section::Features,
        "tong" => Section::Tong,
        name if name.starts_with("tong.sources.") => {
            Section::TongSource(name.trim_start_matches("tong.sources.").to_owned())
        }
        other => Section::Other(other.to_owned()),
    })
}

fn split_key_value(line: &str) -> Option<(&str, &str)> {
    let mut in_string = false;
    let mut escaped = false;
    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '=' if !in_string => return Some((line[..idx].trim(), line[idx + 1..].trim())),
            _ => {}
        }
    }
    None
}

fn parse_value(path: &Path, line: usize, value: &str) -> Result<TomlValue> {
    let value = value.trim();
    if value.starts_with('"') {
        return Ok(TomlValue::String(
            parse_string(path, line, value)?.to_owned(),
        ));
    }
    if value == "true" {
        return Ok(TomlValue::Bool(true));
    }
    if value == "false" {
        return Ok(TomlValue::Bool(false));
    }
    if value.starts_with('[') {
        return parse_array(path, line, value).map(TomlValue::Array);
    }
    if value.starts_with('{') {
        return parse_inline_table(path, line, value).map(TomlValue::InlineTable);
    }
    Ok(TomlValue::Other(value.to_owned()))
}

fn parse_array(path: &Path, line: usize, value: &str) -> Result<Vec<TomlValue>> {
    if !value.ends_with(']') {
        return Err(TongError::parse(
            path.to_path_buf(),
            line,
            "unterminated array",
        ));
    }
    let inner = value.trim_start_matches('[').trim_end_matches(']').trim();
    split_commas(inner)
        .into_iter()
        .filter(|item| !item.trim().is_empty())
        .map(|item| parse_value(path, line, item))
        .collect()
}

fn parse_inline_table(
    path: &Path,
    line: usize,
    value: &str,
) -> Result<BTreeMap<String, TomlValue>> {
    if !value.ends_with('}') {
        return Err(TongError::parse(
            path.to_path_buf(),
            line,
            "unterminated inline table",
        ));
    }
    let inner = value.trim_start_matches('{').trim_end_matches('}').trim();
    let mut table = BTreeMap::new();
    for item in split_commas(inner) {
        if item.trim().is_empty() {
            continue;
        }
        let Some((key, value)) = split_key_value(item) else {
            return Err(TongError::parse(
                path.to_path_buf(),
                line,
                "expected key = value in inline table",
            ));
        };
        table.insert(key.trim().to_owned(), parse_value(path, line, value)?);
    }
    Ok(table)
}

fn split_commas(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut depth = 0usize;
    let mut start = 0;
    for (idx, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '[' | '{' if !in_string => depth += 1,
            ']' | '}' if !in_string => depth = depth.saturating_sub(1),
            ',' if !in_string && depth == 0 => {
                parts.push(&value[start..idx]);
                start = idx + 1;
            }
            _ => {}
        }
    }
    parts.push(&value[start..]);
    parts
}

fn parse_string<'a>(path: &Path, line: usize, value: &'a str) -> Result<&'a str> {
    let value = value.trim();
    if !value.starts_with('"') || !value.ends_with('"') || value.len() < 2 {
        return Err(TongError::parse(
            path.to_path_buf(),
            line,
            "expected string",
        ));
    }
    Ok(&value[1..value.len() - 1])
}

fn strip_comment(line: &str) -> String {
    let mut in_string = false;
    let mut escaped = false;
    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '#' if !in_string => return line[..idx].to_owned(),
            _ => {}
        }
    }
    line.to_owned()
}

fn logical_lines(raw: &str) -> Vec<(usize, String)> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut start_line = 0usize;
    let mut depth = 0isize;
    let mut in_triple_string = false;

    for (idx, raw_line) in raw.lines().enumerate() {
        let line_no = idx + 1;
        let line = strip_comment(raw_line).trim().to_owned();
        if in_triple_string {
            if line.contains("\"\"\"") {
                in_triple_string = false;
            }
            continue;
        }
        if line.matches("\"\"\"").count() % 2 == 1 {
            in_triple_string = true;
            continue;
        }
        if line.is_empty() && depth == 0 {
            continue;
        }
        if current.is_empty() {
            start_line = line_no;
            current.push_str(&line);
        } else {
            current.push(' ');
            current.push_str(&line);
        }

        depth += bracket_delta(&line);
        if depth <= 0 {
            let line = current.trim().to_owned();
            if !line.is_empty() {
                lines.push((start_line, line));
            }
            current.clear();
            depth = 0;
        }
    }

    if !current.trim().is_empty() {
        lines.push((start_line, current.trim().to_owned()));
    }

    lines
}

fn bracket_delta(line: &str) -> isize {
    let mut delta = 0;
    let mut in_string = false;
    let mut escaped = false;
    for ch in line.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '[' | '{' if !in_string => delta += 1,
            ']' | '}' if !in_string => delta -= 1,
            _ => {}
        }
    }
    delta
}

fn required_string(
    path: &Path,
    values: &BTreeMap<String, TomlValue>,
    dotted_key: &str,
) -> Result<String> {
    let key = dotted_key.rsplit('.').next().unwrap_or(dotted_key);
    optional_string(values, key).ok_or_else(|| {
        TongError::invalid_manifest(path.to_path_buf(), format!("missing `{dotted_key}`"))
    })
}

fn optional_string(values: &BTreeMap<String, TomlValue>, key: &str) -> Option<String> {
    match values.get(key) {
        Some(TomlValue::String(value)) => Some(value.clone()),
        _ => None,
    }
}

fn optional_bool(values: &BTreeMap<String, TomlValue>, key: &str) -> Option<bool> {
    match values.get(key) {
        Some(TomlValue::Bool(value)) => Some(*value),
        _ => None,
    }
}

fn parse_string_array(path: &Path, line: usize, value: &str) -> Result<Vec<String>> {
    match parse_value(path, line, value)? {
        TomlValue::Array(values) => values
            .into_iter()
            .map(|value| match value {
                TomlValue::String(value) => Ok(value),
                _ => Err(TongError::parse(
                    path.to_path_buf(),
                    line,
                    "expected string array",
                )),
            })
            .collect(),
        _ => Err(TongError::parse(
            path.to_path_buf(),
            line,
            "expected string array",
        )),
    }
}

fn optional_string_array(values: &BTreeMap<String, TomlValue>, key: &str) -> Option<Vec<String>> {
    match values.get(key) {
        Some(TomlValue::Array(values)) => values
            .iter()
            .map(|value| match value {
                TomlValue::String(value) => Some(value.clone()),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}
