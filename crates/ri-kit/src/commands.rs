// Slash-command parsing and template expansion.
//
// Loads `.md` command templates from a directory and expands a
// `/name arg1 arg2 ...` user input into the matching template's body
// with positional arguments substituted.
//
// Templates support variable substitution in their body:
//   $1, $2, ...         positional args
//   $@ and $ARGUMENTS   all args joined
//   ${@:N}              args from Nth onwards (1-indexed)
//   ${@:N:L}            L args starting from Nth
//
// Substitution is single-pass: argument values containing $ patterns
// are never re-expanded.

use std::fs;
use std::path::{Path, PathBuf};

/// A prompt template loaded from a .md file.
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    pub content: String,
    pub path: PathBuf,
}

/// A parsed `/command arg1 arg2 ...` invocation.
pub struct SlashCommand<'a> {
    pub name: &'a str,
    pub args_str: &'a str,
}

/// Load all .md prompt templates from a single directory.
///
/// Returns an empty vec if the directory doesn't exist or can't be read.
/// Non-recursive: only direct children are scanned.
pub fn load_templates(dir: &Path) -> Vec<PromptTemplate> {
    let mut templates = Vec::new();
    load_dir(dir, &mut templates);
    templates
}

/// Try to parse `text` as a `/name ...` command.
/// Returns `None` if the text doesn't start with `/`.
pub fn parse_command(text: &str) -> Option<SlashCommand<'_>> {
    if !text.starts_with('/') {
        return None;
    }
    let (name, args_str) = match text.find(' ') {
        Some(i) => (&text[1..i], &text[i + 1..]),
        None => (&text[1..], ""),
    };
    Some(SlashCommand { name, args_str })
}

/// If `text` is a `/command ...` that matches a loaded template, expand it.
/// Otherwise return the text unchanged.
pub fn expand_prompt(text: &str, templates: &[PromptTemplate]) -> String {
    let cmd = match parse_command(text) {
        Some(c) => c,
        None => return text.to_string(),
    };
    let tmpl = match templates.iter().find(|t| t.name == cmd.name) {
        Some(t) => t,
        None => return text.to_string(),
    };
    let args: Vec<&str> = if cmd.args_str.is_empty() {
        Vec::new()
    } else {
        cmd.args_str.split_whitespace().collect()
    };
    substitute_args(&tmpl.content, &args)
}

/// Substitute argument placeholders in a template body. See module docs
/// for the supported `$` patterns.
pub fn substitute_args(content: &str, args: &[&str]) -> String {
    let all = args.join(" ");
    let bytes = content.as_bytes();
    let mut out = String::with_capacity(content.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != b'$' {
            let ch = content[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }

        let rest = &content[i..];

        // ${@:N} or ${@:N:L}
        if rest.starts_with("${@:") {
            if let Some(close) = rest.find('}') {
                out.push_str(&expand_slice(&rest[4..close], args));
                i += close + 1;
                continue;
            }
        }

        // $ARGUMENTS
        if rest.starts_with("$ARGUMENTS") {
            out.push_str(&all);
            i += "$ARGUMENTS".len();
            continue;
        }

        // $@
        if rest.len() >= 2 && bytes[i + 1] == b'@' {
            out.push_str(&all);
            i += 2;
            continue;
        }

        // $N (positional, greedy digits)
        if rest.len() >= 2 && bytes[i + 1].is_ascii_digit() {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if let Ok(n) = content[i + 1..j].parse::<usize>() {
                let idx = n.saturating_sub(1);
                out.push_str(args.get(idx).copied().unwrap_or(""));
                i = j;
                continue;
            }
        }

        // Not a recognized pattern -- emit $ literally.
        out.push('$');
        i += 1;
    }

    out
}

// -- Internals ----------------------------------------------------------------

fn load_dir(dir: &Path, out: &mut Vec<PromptTemplate>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }

        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        let raw = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let (fm, body) = parse_frontmatter(&raw);
        let description = extract_field(&fm, "description")
            .unwrap_or_default()
            .to_string();

        out.push(PromptTemplate {
            name,
            description,
            content: body.to_string(),
            path,
        });
    }
}

/// Parse YAML frontmatter delimited by `---`.
fn parse_frontmatter(content: &str) -> (&str, &str) {
    let after_open = if content.starts_with("---\r\n") {
        5
    } else if content.starts_with("---\n") {
        4
    } else {
        return ("", content);
    };

    match content[after_open..].find("\n---") {
        Some(pos) => {
            let fm = &content[after_open..after_open + pos];
            let close_end = after_open + pos + 4; // skip "\n---"
            let body_start = if content[close_end..].starts_with("\r\n") {
                close_end + 2
            } else if content[close_end..].starts_with('\n') {
                close_end + 1
            } else {
                close_end
            };
            let body = content.get(body_start..).unwrap_or("");
            (fm, body)
        }
        None => ("", content),
    }
}

/// Extract a simple `key: value` field from frontmatter text.
fn extract_field<'a>(frontmatter: &'a str, key: &str) -> Option<&'a str> {
    for line in frontmatter.lines() {
        if let Some(rest) = line.trim().strip_prefix(key) {
            if let Some(value) = rest.strip_prefix(':') {
                let v = value.trim();
                let v = v.strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                    .unwrap_or(v);
                return Some(v);
            }
        }
    }
    None
}

fn expand_slice(spec: &str, args: &[&str]) -> String {
    let parts: Vec<&str> = spec.split(':').collect();
    let start = parts
        .first()
        .and_then(|s| s.parse::<usize>().ok())
        .map(|n| n.saturating_sub(1))
        .unwrap_or(0)
        .min(args.len());

    match parts.get(1).and_then(|s| s.parse::<usize>().ok()) {
        Some(len) => args[start..(start + len).min(args.len())].join(" "),
        None => args[start..].join(" "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_basic() {
        let input = "---\ndescription: hello world\n---\nbody here";
        let (fm, body) = parse_frontmatter(input);
        assert_eq!(fm, "description: hello world");
        assert_eq!(body, "body here");
    }

    #[test]
    fn frontmatter_empty() {
        let (fm, body) = parse_frontmatter("no frontmatter");
        assert_eq!(fm, "");
        assert_eq!(body, "no frontmatter");
    }

    #[test]
    fn frontmatter_empty_body() {
        let input = "---\nfoo: bar\n---\n";
        let (fm, body) = parse_frontmatter(input);
        assert_eq!(fm, "foo: bar");
        assert_eq!(body, "");
    }

    #[test]
    fn extract_description() {
        let fm = "description: Full implementation workflow\nother: value";
        assert_eq!(extract_field(fm, "description"), Some("Full implementation workflow"));
        assert_eq!(extract_field(fm, "other"), Some("value"));
        assert_eq!(extract_field(fm, "missing"), None);
    }

    #[test]
    fn extract_quoted() {
        assert_eq!(extract_field("key: \"quoted value\"", "key"), Some("quoted value"));
        assert_eq!(extract_field("key: 'single'", "key"), Some("single"));
    }

    #[test]
    fn parse_command_basic() {
        let cmd = parse_command("/task implement foo").unwrap();
        assert_eq!(cmd.name, "task");
        assert_eq!(cmd.args_str, "implement foo");
    }

    #[test]
    fn parse_command_no_args() {
        let cmd = parse_command("/help").unwrap();
        assert_eq!(cmd.name, "help");
        assert_eq!(cmd.args_str, "");
    }

    #[test]
    fn parse_command_not_slash() {
        assert!(parse_command("plain text").is_none());
    }

    #[test]
    fn substitute_positional() {
        let args = vec!["foo", "bar"];
        assert_eq!(substitute_args("$1 and $2", &args), "foo and bar");
    }

    #[test]
    fn substitute_all() {
        let args = vec!["a", "b", "c"];
        assert_eq!(substitute_args("args: $@", &args), "args: a b c");
        assert_eq!(substitute_args("args: $ARGUMENTS", &args), "args: a b c");
    }

    #[test]
    fn substitute_slice() {
        let args = vec!["a", "b", "c", "d"];
        assert_eq!(substitute_args("${@:2}", &args), "b c d");
        assert_eq!(substitute_args("${@:2:2}", &args), "b c");
    }

    #[test]
    fn substitute_missing_arg() {
        let args = vec!["only"];
        assert_eq!(substitute_args("$1 $2 $3", &args), "only  ");
    }

    #[test]
    fn no_reexpansion() {
        let args = vec!["literal $@ text"];
        assert_eq!(substitute_args("result: $1", &args), "result: literal $@ text");
    }

    #[test]
    fn dollar_passthrough() {
        let args = vec!["x"];
        assert_eq!(substitute_args("cost is $50", &args), "cost is ");
        assert_eq!(substitute_args("$$ and $", &args), "$$ and $");
    }

    #[test]
    fn utf8_body() {
        let args = vec!["world"];
        assert_eq!(substitute_args("hello $1!", &args), "hello world!");
    }
}
