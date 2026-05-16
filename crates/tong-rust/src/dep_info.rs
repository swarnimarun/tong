use std::path::PathBuf;

pub(crate) fn parse_makefile_dep_info(raw: &str) -> Vec<PathBuf> {
    let mut logical = String::new();
    for line in raw.lines() {
        let line = line.trim_end();
        if let Some(stripped) = line.strip_suffix('\\') {
            logical.push_str(stripped);
            logical.push(' ');
        } else {
            logical.push_str(line);
            logical.push(' ');
        }
    }

    let Some((_, deps)) = split_unescaped_colon(&logical) else {
        return Vec::new();
    };
    split_escaped_whitespace(deps)
        .into_iter()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn split_unescaped_colon(value: &str) -> Option<(&str, &str)> {
    let mut escaped = false;
    for (idx, ch) in value.char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == ':' {
            return Some((&value[..idx], &value[idx + 1..]));
        }
    }
    None
}

fn split_escaped_whitespace(value: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                values.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        values.push(current);
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_makefile_dep_info_with_escaped_spaces() {
        let deps = parse_makefile_dep_info(
            "/tmp/out.rlib: src/lib.rs src/some\\ file.rs \\\n             /tmp/generated.rs\n",
        );
        assert_eq!(
            deps,
            [
                PathBuf::from("src/lib.rs"),
                PathBuf::from("src/some file.rs"),
                PathBuf::from("/tmp/generated.rs")
            ]
        );
    }
}
