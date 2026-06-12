use indexmap::IndexMap;
use ruff_python_stdlib::identifiers::is_identifier;
use ruff_source_file::UniversalNewlines;
use ruff_text_size::TextSize;

use super::indentation;
use super::preformatted::PreformattedBlockScanner;
use super::rst::is_field_list_marker;

/// Returns parameter documentation from recognized Google-style parameter sections.
pub(super) fn parameter_documentation(raw: &str) -> IndexMap<String, String> {
    let mut parameters = IndexMap::new();
    visit_parameter_sections(raw, |body| {
        extend_parameter_documentation(&mut parameters, body);
    });
    parameters
}

/// Visits Google-style parameter sections in source order.
fn visit_parameter_sections<'a>(raw: &'a str, mut visit: impl FnMut(&[ParsedLine<'a>])) {
    let lines = parsed_lines(raw);
    let mut preformatted_blocks = PreformattedBlockScanner::default();
    let mut index = 0;

    while index < lines.len() {
        // Content in another block can resemble a top-level Google section.
        if preformatted_blocks.consume_preformatted_line(lines[index].text) {
            index += 1;
            continue;
        }
        if let Some(end) = non_google_underlined_section_end(&lines, index) {
            index = end;
            continue;
        }
        if let Some(end) = indented_non_google_block_end(&lines, index) {
            index = end;
            continue;
        }

        let Some(header) = parse_section_header(&lines, index) else {
            preformatted_blocks.observe_line_outside_preformatted_block(lines[index].text);
            index += 1;
            continue;
        };
        let body_end = section_body_end(&lines, header);
        if header.kind == HeaderKind::Parameters {
            visit(&lines[header.body_start..body_end]);
        }
        index = body_end;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParsedLine<'a> {
    text: &'a str,
}

fn parsed_lines(raw: &str) -> Vec<ParsedLine<'_>> {
    raw.universal_newlines()
        .map(|line| ParsedLine {
            text: line.as_str(),
        })
        .collect()
}

/// Returns the index after a non-Google underlined section that starts at `index`.
fn non_google_underlined_section_end(lines: &[ParsedLine<'_>], index: usize) -> Option<usize> {
    let header_indent = non_google_underlined_section_indent(lines, index)?;
    let mut section_end = index + 2;
    let mut preformatted_blocks = PreformattedBlockScanner::default();

    while section_end < lines.len() {
        if preformatted_blocks.consume_preformatted_line(lines[section_end].text) {
            section_end += 1;
            continue;
        }
        // A blank-separated header at this indentation is a sibling, not nested content.
        if section_end > index + 2
            && (non_google_underlined_section_indent(lines, section_end)
                .is_some_and(|indent| indent <= header_indent)
                || (lines[section_end - 1].text.trim().is_empty()
                    && parse_section_header(lines, section_end)
                        .is_some_and(|header| header.indent <= header_indent)))
        {
            break;
        }
        preformatted_blocks.observe_line_outside_preformatted_block(lines[section_end].text);
        section_end += 1;
    }
    Some(section_end)
}

/// Returns the indentation of a non-Google underlined section at `index`.
fn non_google_underlined_section_indent(
    lines: &[ParsedLine<'_>],
    index: usize,
) -> Option<TextSize> {
    let header = lines.get(index)?.text;
    let underline = lines.get(index + 1)?.text;
    let header_indent = indentation(header);
    let underline_text = underline.trim();

    (!header.trim().is_empty()
        && !header.trim_end().ends_with(':')
        && indentation(underline) == header_indent
        && underline_text.len() >= 3
        && underline_text
            .chars()
            .all(|character| matches!(character, '-' | '=')))
    .then_some(header_indent)
}

/// Returns the index after an indented reST or Markdown container at `index`.
fn indented_non_google_block_end(lines: &[ParsedLine<'_>], index: usize) -> Option<usize> {
    let marker = lines.get(index)?.text;
    if !is_rest_directive_marker(marker)
        && !is_field_list_marker(marker)
        && !starts_with_markdown_list_item(marker.trim_start())
    {
        return None;
    }

    let marker_indent = indentation(marker);
    Some(
        lines[index + 1..]
            .iter()
            .position(|line| {
                !line.text.trim().is_empty() && indentation(line.text) <= marker_indent
            })
            .map_or(lines.len(), |offset| index + 1 + offset),
    )
}

fn is_rest_directive_marker(line: &str) -> bool {
    let Some(directive) = line.trim_start().strip_prefix(".. ") else {
        return false;
    };
    let Some((name, _)) = directive.split_once("::") else {
        return false;
    };
    !name.is_empty() && !name.chars().any(char::is_whitespace)
}

fn starts_with_markdown_list_item(line: &str) -> bool {
    let bytes = line.as_bytes();
    if matches!(bytes, [b'-' | b'+' | b'*', b' ' | b'\t', ..]) {
        return true;
    }

    let digits = bytes
        .iter()
        .take(9)
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    digits > 0
        && matches!(bytes.get(digits), Some(b'.' | b')'))
        && matches!(bytes.get(digits + 1), Some(b' ' | b'\t'))
}

/// Returns the index of the first line outside `header`'s body.
fn section_body_end(lines: &[ParsedLine<'_>], header: SectionHeader) -> usize {
    let mut body_end = header.body_start;
    let mut preformatted_blocks = PreformattedBlockScanner::default();
    let mut parameter_item_indent = None;
    let mut aligned_container_body = false;

    while let Some(line) = lines.get(body_end) {
        // Once a preformatted block begins, its contents cannot end the section.
        if preformatted_blocks.is_active()
            && preformatted_blocks.consume_preformatted_line(line.text)
        {
            body_end += 1;
            continue;
        }

        if line.text.trim().is_empty() {
            if !blank_line_continues_section(
                &lines[body_end..],
                header,
                parameter_item_indent,
                aligned_container_body,
            ) {
                break;
            }
            while let Some(line) = lines.get(body_end)
                && line.text.trim().is_empty()
            {
                body_end += 1;
            }
            continue;
        }

        // PEP 257 can align a first-line heading with its body.
        let can_start_aligned_container = body_end == header.body_start;
        if section_header_ends_body(
            lines,
            body_end,
            header,
            parameter_item_indent,
            aligned_container_body || can_start_aligned_container,
        ) || !line_belongs_to_body(
            header,
            line.text,
            parameter_item_indent,
            aligned_container_body || can_start_aligned_container,
        ) {
            break;
        }

        aligned_container_body |= is_aligned_container_body(header, line.text);
        parameter_item_indent =
            parameter_item_indent.or_else(|| parameter_item_indent_for_line(header, line.text));

        if !preformatted_blocks.consume_preformatted_line(line.text) {
            preformatted_blocks.observe_line_outside_preformatted_block(line.text);
        }
        body_end += 1;
    }

    body_end
}

/// Returns whether content after leading blank lines still belongs to `header`.
fn blank_line_continues_section(
    lines: &[ParsedLine<'_>],
    header: SectionHeader,
    parameter_item_indent: Option<TextSize>,
    aligned_container_body: bool,
) -> bool {
    let Some((offset, next)) = lines
        .iter()
        .enumerate()
        .find(|(_, line)| !line.text.trim().is_empty())
    else {
        return false;
    };

    let next_indent = indentation(next.text);
    if next_indent <= header.indent
        && (parse_section_header(lines, offset).is_some() || is_inline_section_header(next.text))
    {
        return false;
    }
    // A blank line separates prose aligned with the parameter items from the section body.
    if parameter_item_indent == Some(next_indent)
        && parameter_item_indent_for_line(header, next.text).is_none()
    {
        return false;
    }

    line_belongs_to_body(
        header,
        next.text,
        parameter_item_indent,
        aligned_container_body,
    )
}

/// Returns whether a recognized header at `index` ends the current section body.
fn section_header_ends_body(
    lines: &[ParsedLine<'_>],
    index: usize,
    header: SectionHeader,
    parameter_item_indent: Option<TextSize>,
    aligned_container_body: bool,
) -> bool {
    let Some(line) = lines.get(index) else {
        return false;
    };
    if aligned_container_body && is_aligned_container_body(header, line.text) {
        return false;
    }
    if indentation(line.text) <= header.indent && is_inline_section_header(line.text) {
        return true;
    }

    parse_section_header(lines, index).is_some_and(|next| {
        next.indent <= header.indent
            && (next.underlined
                || !lowercase_same_indent_parameter_takes_precedence(
                    header,
                    line.text,
                    parameter_item_indent,
                ))
    })
}

/// Returns whether `line` belongs to `header` under Google-style indentation rules.
fn line_belongs_to_body(
    header: SectionHeader,
    line: &str,
    parameter_item_indent: Option<TextSize>,
    aligned_container_body: bool,
) -> bool {
    let line_indent = indentation(line);
    // Once a same-indent item establishes a first-line PEP 257 layout, continuations at that
    // indentation remain part of the section.
    (aligned_container_body && is_aligned_container_body(header, line))
        || line_indent > header.indent
        || (line_indent == header.indent
            && parameter_item_indent.is_none_or(|indent| indent == line_indent)
            && (parameter_item_indent.is_some()
                || parameter_item_indent_for_line(header, line).is_some()))
}

fn is_aligned_container_body(header: SectionHeader, line: &str) -> bool {
    header.kind == HeaderKind::Container && indentation(line) == header.indent
}

/// Returns whether a same-indent lowercase item is a parameter rather than a section header.
fn lowercase_same_indent_parameter_takes_precedence(
    header: SectionHeader,
    line: &str,
    parameter_item_indent: Option<TextSize>,
) -> bool {
    let line_indent = indentation(line);
    line_indent == header.indent
        && parameter_item_indent.is_none_or(|indent| indent == line_indent)
        && line.trim().chars().next().is_some_and(char::is_lowercase)
        && parameter_item_indent_for_line(header, line).is_some()
}

fn parameter_item_indent_for_line(header: SectionHeader, line: &str) -> Option<TextSize> {
    (header.kind == HeaderKind::Parameters && parse_parameter(line.trim()).is_some())
        .then(|| indentation(line))
}

/// Parses a recognized Google-style section header at `index`.
fn parse_section_header(lines: &[ParsedLine<'_>], index: usize) -> Option<SectionHeader> {
    let line = lines.get(index)?.text;
    let kind = section_kind(line)?;
    let underlined = lines
        .get(index + 1)
        .is_some_and(|line| is_section_underline(line.text));

    Some(SectionHeader {
        kind,
        indent: indentation(line),
        body_start: index + 1 + usize::from(underlined),
        underlined,
    })
}

fn is_section_underline(line: &str) -> bool {
    let line = line.trim();
    !line.is_empty() && line.chars().all(|character| matches!(character, '-' | '='))
}

fn section_kind(line: &str) -> Option<HeaderKind> {
    let name = line.trim().strip_suffix(':')?.trim();
    section_kind_from_name(name)
}

fn section_kind_from_name(name: &str) -> Option<HeaderKind> {
    let normalized = name
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    Some(match normalized.as_str() {
        "args" | "arguments" | "parameters" | "keyword args" | "keyword arguments"
        | "other args" | "other arguments" | "other parameters" => HeaderKind::Parameters,
        "attributes" | "return" | "returns" | "yield" | "yields" | "raise" | "raises" => {
            HeaderKind::StructuredBoundary
        }
        "attention" | "caution" | "danger" | "error" | "example" | "examples" | "hint"
        | "important" | "methods" | "note" | "notes" | "references" | "see also" | "tip"
        | "todo" | "todos" | "warning" | "warnings" | "warns" => HeaderKind::Container,
        _ => return None,
    })
}

/// Returns whether `line` is a recognized section header followed by inline content.
fn is_inline_section_header(line: &str) -> bool {
    let Some((name, description)) = split_once_unbracketed_colon(line.trim()) else {
        return false;
    };
    let name = name.trim();
    !description.trim().is_empty()
        && name.chars().next().is_some_and(char::is_uppercase)
        && section_kind_from_name(name).is_some()
}

/// Parses a parameter item into its display name and description.
fn parse_parameter(line: &str) -> Option<(&str, &str)> {
    let (name, description) = split_once_unbracketed_colon(line)?;
    let (display_name, _) = parse_parenthesized_type(name.trim());
    google_parameter_names(display_name)
        .all(is_parameter_name)
        .then_some((display_name, description.trim()))
}

fn is_parameter_name(name: &str) -> bool {
    let identifier = name
        .strip_prefix("**")
        .or_else(|| name.strip_prefix('*'))
        .unwrap_or(name);
    is_identifier(identifier)
}

/// Extends `parameters` with the documented items in one parameter section body.
fn extend_parameter_documentation(
    parameters: &mut IndexMap<String, String>,
    lines: &[ParsedLine<'_>],
) {
    let mut current: Option<(String, String)> = None;
    let mut item_indent = None;

    // The first item fixes the indentation. Colons at other levels remain continuation prose.
    for line in lines {
        let trimmed = line.text.trim();
        let line_indent = indentation(line.text);
        if trimmed.is_empty() {
            if let Some((_, description)) = &mut current {
                if !description.is_empty() && !description.ends_with('\n') {
                    description.push('\n');
                }
                description.push('\n');
            }
        } else if item_indent.is_none_or(|indent| line_indent == indent)
            && let Some((names, description)) = parse_parameter(trimmed)
        {
            insert_parameter_documentation(
                parameters,
                current.replace((names.to_string(), description.to_string())),
            );
            item_indent.get_or_insert(line_indent);
        } else if let Some((_, description)) = &mut current {
            if !description.is_empty() && !description.ends_with('\n') {
                description.push('\n');
            }
            description.push_str(trimmed);
        }
    }

    insert_parameter_documentation(parameters, current);
}

/// Inserts a completed parameter item under each of its comma-separated names.
fn insert_parameter_documentation(
    parameters: &mut IndexMap<String, String>,
    parameter: Option<(String, String)>,
) {
    let Some((names, description)) = parameter else {
        return;
    };
    let description = description.trim();
    if !description.is_empty() {
        for name in google_parameter_names(&names) {
            parameters.insert(name.to_string(), description.to_string());
        }
    }
}

fn google_parameter_names(display_name: &str) -> impl Iterator<Item = &str> {
    display_name.split(',').map(str::trim)
}

/// Splits at the first colon outside bracket pairs and quoted strings.
fn split_once_unbracketed_colon(line: &str) -> Option<(&str, &str)> {
    let mut depths = [0usize; 3];
    let mut quote = None;
    let mut escaped = false;

    for (index, character) in line.char_indices() {
        if let Some(quote_character) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == quote_character {
                quote = None;
            }
            continue;
        }

        match character {
            '\'' | '"' => quote = Some(character),
            '(' => depths[0] += 1,
            ')' => depths[0] = depths[0].saturating_sub(1),
            '[' => depths[1] += 1,
            ']' => depths[1] = depths[1].saturating_sub(1),
            '{' => depths[2] += 1,
            '}' => depths[2] = depths[2].saturating_sub(1),
            ':' if depths == [0; 3] => {
                return Some((&line[..index], &line[index + character.len_utf8()..]));
            }
            _ => {}
        }
    }
    None
}

/// Splits a trailing parenthesized type from a parameter display name.
fn parse_parenthesized_type(name: &str) -> (&str, Option<&str>) {
    if !name.ends_with(')') {
        return (name, None);
    }

    let mut depth = 0usize;
    let mut opening = None;
    let mut quote = None;
    let mut escaped = false;

    // Only a balanced group that closes at the end can be a type suffix.
    for (index, character) in name.char_indices() {
        if let Some(quote_character) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == quote_character {
                quote = None;
            }
            continue;
        }

        match character {
            '\'' | '"' => quote = Some(character),
            '(' => {
                if depth == 0 {
                    opening = Some(index);
                }
                depth += 1;
            }
            ')' => {
                depth = match depth.checked_sub(1) {
                    Some(depth) => depth,
                    None => return (name, None),
                };
                if depth == 0 && index + character.len_utf8() == name.len() {
                    let Some(opening) = opening else {
                        return (name, None);
                    };
                    let display_name = name[..opening].trim();
                    let ty = name[opening + '('.len_utf8()..index].trim();
                    return if display_name.is_empty() || ty.is_empty() {
                        (name, None)
                    } else {
                        (display_name, Some(ty))
                    };
                }
            }
            _ => {}
        }
    }
    (name, None)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SectionHeader {
    kind: HeaderKind,
    indent: TextSize,
    body_start: usize,
    underlined: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderKind {
    Parameters,
    StructuredBoundary,
    Container,
}

#[cfg(test)]
mod tests {
    use super::parameter_documentation;

    #[test]
    fn extracts_parameter_items() {
        for (raw, expected) in [
            (
                "Arguments:\nfirst: First parameter.\nAligned continuation.\nsecond: Second parameter.\nReturns:\nbool: Result.",
                &[
                    ("first", "First parameter.\nAligned continuation."),
                    ("second", "Second parameter."),
                ][..],
            ),
            (
                "Args:\n  \tfirst: First parameter.\n        second: Second parameter.",
                &[
                    ("first", "First parameter."),
                    ("second", "Second parameter."),
                ],
            ),
            (
                "Args:\n    x, y: Coordinates.",
                &[("x", "Coordinates."), ("y", "Coordinates.")],
            ),
            (
                "Args:\n    value: Initial documentation.\n    value, for example: can be omitted.",
                &[(
                    "value",
                    "Initial documentation.\nvalue, for example: can be omitted.",
                )],
            ),
            (
                "Args:\n    value: First documentation.\n    value: Replacement documentation.",
                &[("value", "Replacement documentation.")],
            ),
            (
                "Args:\n    value (Literal[\"(\"]): Quoted parenthesis.",
                &[("value", "Quoted parenthesis.")],
            ),
            (
                "Args:\n    callback() (Callable): Not a parameter.\n    value: Documentation.",
                &[("value", "Documentation.")],
            ),
            (
                "Args:\n    value: First paragraph.\n\n\n        Second paragraph.",
                &[("value", "First paragraph.\n\n\nSecond paragraph.")],
            ),
        ] {
            assert_parameter_documentation(raw, expected);
        }
    }

    #[test]
    fn recognizes_parameter_section_headings() {
        for heading in [
            "Args",
            "Arguments",
            "Parameters",
            "Keyword Args",
            "Keyword Arguments",
            "Other Args",
            "Other Arguments",
            "Other Parameters",
        ] {
            let raw = format!("{heading}:\n    value: Parameter documentation.");
            assert_parameter_documentation(&raw, &[("value", "Parameter documentation.")]);
        }
        assert_parameter_documentation(
            "Args:\n----\n    value: Parameter documentation.\n\nReturns:\n    bool: Result.",
            &[("value", "Parameter documentation.")],
        );
    }

    #[test]
    fn respects_section_boundaries() {
        for (raw, expected) in [
            (
                "Args:\n    value: Parameter documentation.\nMethods:\n    helper: Method documentation.",
                &[("value", "Parameter documentation.")][..],
            ),
            (
                "Example:\n    Args:\n        nested: Not parameter documentation.\nArgs:\n    value: Parameter documentation.",
                &[("value", "Parameter documentation.")],
            ),
            (
                "Args:\n    first: First parameter.\n    last: Last parameter.\n\nReturns: Result.",
                &[("first", "First parameter."), ("last", "Last parameter.")],
            ),
            (
                "Args:\nerror:\n    Error documentation.\nargs: Args documentation.\nreturns: Return documentation.\nReturns:\nbool: Result.",
                &[
                    ("error", "Error documentation."),
                    ("args", "Args documentation."),
                    ("returns", "Return documentation."),
                ],
            ),
            (
                "Args:\nvalue: Parameter documentation.\n\nAdditional details.",
                &[("value", "Parameter documentation.")],
            ),
            (
                "Args:\n    Warning: Capitalized parameter.\n    following: Following parameter.",
                &[
                    ("Warning", "Capitalized parameter."),
                    ("following", "Following parameter."),
                ],
            ),
            (
                "Args:\nfirst: First parameter.\nlast: Last parameter.\nReturns: Result.",
                &[("first", "First parameter."), ("last", "Last parameter.")],
            ),
            (
                "Args:\nvalue: Parameter documentation.\n\nreturns:\n--------\n    bool: Result.",
                &[("value", "Parameter documentation.")],
            ),
            (
                "Args:\n    value: Parameter documentation.\n\n    Additional details.",
                &[("value", "Parameter documentation.")],
            ),
            (
                "Parameters\n----------\nnumpy : int\n    NumPy docs.\n\nArgs:\n    google: Google docs.",
                &[("google", "Google docs.")],
            ),
        ] {
            assert_parameter_documentation(raw, expected);
        }
    }

    #[test]
    fn ignores_sections_in_other_containers() {
        for raw in [
            "Examples\n--------\nArgs:\n    nested: Not parameter documentation.",
            ".. note::\n    Args:\n        nested: Not parameter documentation.",
            "- Example:\n    Args:\n        nested: Not parameter documentation.",
            "1. Example:\n    Args:\n        nested: Not parameter documentation.",
            ":param value: Example input.\n    Args:\n        nested: Not parameter documentation.",
            "Example:\nArgs:\n    nested: Not parameter documentation.\nReturns:\n    str: Result.",
        ] {
            assert_parameter_documentation(raw, &[]);
        }

        for raw in [
            "Summary.\n\n    ```text\n    Args:\n        nested: Not parameter documentation.\n    ```\n\n    Args:\n        value: Parameter documentation.",
            "Summary.\n\n    Example::\n\n        Args:\n            nested: Not parameter documentation.\n\n    Args:\n        value: Parameter documentation.",
        ] {
            assert_parameter_documentation(raw, &[("value", "Parameter documentation.")]);
        }

        assert_parameter_documentation(
            ".. note::\n    Args:\n        nested: Not parameter documentation.\nArgs:\n    value: Parameter documentation.",
            &[("value", "Parameter documentation.")],
        );
    }

    #[test]
    fn ignores_doctest_content_and_resumes_after_it() {
        for raw in [
            "        >>> example()\n\tArgs:\n\t    nested: Not parameter documentation.",
            "        >>> example()\n        \u{a0}\n        Args:\n            nested: Not parameter documentation.",
        ] {
            assert_parameter_documentation(raw, &[]);
        }
        assert_parameter_documentation(
            "        >>> example()\n        result\n\t\n        Args:\n            value: Parameter documentation.",
            &[("value", "Parameter documentation.")],
        );
    }

    fn assert_parameter_documentation(raw: &str, expected: &[(&str, &str)]) {
        let parameters = parameter_documentation(raw);
        assert_eq!(parameters.len(), expected.len(), "{raw}");
        for &(name, documentation) in expected {
            assert_eq!(
                parameters.get(name).map(String::as_str),
                Some(documentation),
                "{raw}"
            );
        }
    }
}
