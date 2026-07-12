// Slash-command parsing and template expansion.
//
// Loads `.md` command templates from a directory and expands a
// `/name <arguments>` user input into the matching template's body.
//
// A template body interpolates the argument text with either of two
// interchangeable placeholders:
//   $@ and $ARGUMENTS   the argument text, substituted verbatim
//
// The argument is emitted exactly as the user typed it -- newlines,
// indentation, and internal whitespace preserved -- so a multi-line message
// survives expansion and can be copied back out to tweak.

use std::fs;
use std::path::{Path, PathBuf};

/// A prompt template loaded from a .md file.
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    pub content: String,
    pub path: PathBuf,
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

/// If `text` is a `/command <arguments>` that matches a loaded template, expand
/// it; otherwise return the text unchanged. When several templates share a name
/// the last one loaded wins, so a project-local template shadows a global one.
pub fn expand_prompt(text: &str, templates: &[PromptTemplate]) -> String {
    let cmd = match parse_command(text) {
        Some(c) => c,
        None => return text.to_string(),
    };
    match templates.iter().rfind(|t| t.name == cmd.name) {
        Some(t) => substitute_args(&t.content, cmd.args_str),
        None => text.to_string(),
    }
}

/// A parsed `/command <arguments>` invocation.
struct SlashCommand<'a> {
    name: &'a str,
    args_str: &'a str,
}

/// Parse `text` as a `/name <arguments>` command, or `None` if it doesn't start
/// with `/`. The name runs up to the first whitespace character, which is
/// consumed as the separator; everything after it is the argument, kept
/// verbatim -- its own leading indentation and internal formatting preserved.
fn parse_command(text: &str) -> Option<SlashCommand<'_>> {
    let rest = text.strip_prefix('/')?;
    let (name, args_str) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    Some(SlashCommand { name, args_str })
}

/// Replace every `$@` / `$ARGUMENTS` in a template body with `args`, verbatim.
/// One left-to-right pass that never rescans substituted text, so an argument
/// that itself contains `$@` is safe; a `$` that starts neither token is literal.
fn substitute_args(content: &str, args: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let token = &rest[at..];
        match token.strip_prefix("$ARGUMENTS").or_else(|| token.strip_prefix("$@")) {
            Some(tail) => {
                out.push_str(args);
                rest = tail;
            }
            None => {
                out.push('$');
                rest = &token[1..];
            }
        }
    }
    out.push_str(rest);
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
    fn parse_command_separator() {
        // Exactly one whitespace char separates the name from the argument: a
        // newline separator is consumed, but the argument's own leading
        // indentation survives verbatim.
        let nl = parse_command("/task\n    indented").unwrap();
        assert_eq!(nl.name, "task");
        assert_eq!(nl.args_str, "    indented");
        assert_eq!(parse_command("/task hello").unwrap().args_str, "hello");
    }

    #[test]
    fn preserves_formatting() {
        let arg = "# Heading\n\n- one\n- two\n    indented";
        assert_eq!(substitute_args("$@", arg), arg);
        assert_eq!(
            substitute_args("before\n$ARGUMENTS\nafter", arg),
            format!("before\n{arg}\nafter"),
        );
    }

    #[test]
    fn dollar_is_literal() {
        assert_eq!(substitute_args("cost is $50", "x"), "cost is $50");
        assert_eq!(substitute_args("$$ and $", "x"), "$$ and $");
    }

    #[test]
    fn no_reexpansion() {
        assert_eq!(substitute_args("$@", "literal $@ text"), "literal $@ text");
    }
}
