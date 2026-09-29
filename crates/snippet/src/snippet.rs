use anyhow::{Context as _, Result};
use smallvec::SmallVec;
use std::{collections::BTreeMap, ops::Range, sync::LazyLock};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snippet {
    pub text: String,
    pub tabstops: Vec<TabStop>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TabStop {
    pub ranges: SmallVec<[Range<isize>; 2]>,
    pub choices: Option<Vec<String>>,
    /// Parallel to `ranges`. `Some` marks a regex-transform mirror of this tabstop.
    pub transforms: SmallVec<[Option<SnippetTransform>; 2]>,
}

impl TabStop {
    fn add_range(&mut self, range: Range<isize>, transform: Option<SnippetTransform>) {
        self.ranges.push(range);
        self.transforms.push(transform);
    }

    /// Ranges to select when this tabstop is active. Transform mirrors are omitted
    /// so they keep their transformed text instead of being overwritten by multi-cursor.
    pub fn selection_ranges(&self) -> Vec<&Range<isize>> {
        if self.transforms.iter().all(|transform| transform.is_none()) {
            return self.ranges.iter().collect();
        }
        let selectable: Vec<_> = self
            .ranges
            .iter()
            .zip(&self.transforms)
            .filter_map(|(range, transform)| transform.is_none().then_some(range))
            .collect();
        if selectable.is_empty() {
            self.ranges.iter().collect()
        } else {
            selectable
        }
    }

    pub fn transform_mirrors(&self) -> impl Iterator<Item = (&Range<isize>, &SnippetTransform)> {
        self.ranges
            .iter()
            .zip(&self.transforms)
            .filter_map(|(range, transform)| transform.as_ref().map(|transform| (range, transform)))
    }
}

/// A VS Code-style regex transform: `${n/regex/format/options}` or `${var/regex/format/options}`.
#[derive(Clone, Debug)]
pub struct SnippetTransform {
    pattern: String,
    format: Vec<FormatReplacement>,
    flags: String,
    regex: regex::Regex,
}

impl PartialEq for SnippetTransform {
    fn eq(&self, other: &Self) -> bool {
        self.pattern == other.pattern && self.format == other.format && self.flags == other.flags
    }
}

#[derive(Clone, Debug, PartialEq)]
enum FormatReplacement {
    Text(String),
    Group {
        index: usize,
        shorthand: Option<String>,
        if_value: Option<String>,
        else_value: Option<String>,
    },
}

pub trait VariableResolver {
    fn resolve(&self, name: &str) -> Option<String>;
}

impl VariableResolver for () {
    fn resolve(&self, _: &str) -> Option<String> {
        None
    }
}

impl VariableResolver for BTreeMap<String, String> {
    fn resolve(&self, name: &str) -> Option<String> {
        self.get(name).cloned()
    }
}

enum Piece {
    Text(String),
    TabStop {
        index: usize,
        children: Vec<Piece>,
        choices: Option<Vec<String>>,
        transform: Option<SnippetTransform>,
    },
    Variable {
        name: String,
        children: Vec<Piece>,
        transform: Option<SnippetTransform>,
    },
}

impl Snippet {
    pub fn parse(source: &str) -> Result<Self> {
        Self::parse_with_resolver(source, &())
    }

    pub fn parse_with_resolver(source: &str, resolver: &dyn VariableResolver) -> Result<Self> {
        let (_, pieces) = parse_pieces(source, false).context("failed to parse snippet")?;
        let defaults = tabstop_defaults(&pieces, resolver);
        let mut text = String::with_capacity(source.len());
        let mut tabstops = BTreeMap::new();
        emit_pieces(&pieces, &mut text, &mut tabstops, &defaults, resolver);

        let len = text.len() as isize;
        let final_tabstop = tabstops.remove(&0);
        let mut tabstops = tabstops.into_values().collect::<Vec<_>>();

        if let Some(final_tabstop) = final_tabstop {
            tabstops.push(final_tabstop);
        } else {
            let end_tabstop = TabStop {
                ranges: [len..len].into_iter().collect(),
                choices: None,
                transforms: [None].into_iter().collect(),
            };

            if !tabstops.last().is_some_and(|t| *t == end_tabstop) {
                tabstops.push(end_tabstop);
            }
        }

        Ok(Snippet { text, tabstops })
    }
}

impl SnippetTransform {
    pub fn apply(&self, value: &str) -> String {
        let regex = &self.regex;
        let global = self.flags.contains('g');
        let replace_match = |captures: &regex::Captures| {
            let mut out = String::new();
            for part in &self.format {
                match part {
                    FormatReplacement::Text(text) => out.push_str(text),
                    FormatReplacement::Group {
                        index,
                        shorthand,
                        if_value,
                        else_value,
                    } => {
                        let group = captures.get(*index).map(|m| m.as_str());
                        out.push_str(&resolve_format(group, shorthand, if_value, else_value));
                    }
                }
            }
            out
        };

        if global {
            return regex.replace_all(value, replace_match).into_owned();
        }

        match regex.captures(value) {
            Some(captures) => {
                let matched = captures.get(0).map(|m| m.range()).unwrap_or(0..0);
                let mut result = String::with_capacity(value.len());
                result.push_str(&value[..matched.start]);
                result.push_str(&replace_match(&captures));
                result.push_str(&value[matched.end..]);
                result
            }
            None => {
                if self.format.iter().any(|part| {
                    matches!(
                        part,
                        FormatReplacement::Group {
                            else_value: Some(_),
                            ..
                        }
                    )
                }) {
                    let mut out = String::new();
                    for part in &self.format {
                        match part {
                            FormatReplacement::Text(text) => out.push_str(text),
                            FormatReplacement::Group {
                                shorthand,
                                if_value,
                                else_value,
                                ..
                            } => {
                                out.push_str(&resolve_format(None, shorthand, if_value, else_value))
                            }
                        }
                    }
                    out
                } else {
                    value.to_string()
                }
            }
        }
    }
}

fn compile_regex(pattern: &str, flags: &str) -> Result<regex::Regex, regex::Error> {
    let mut builder = regex::RegexBuilder::new(pattern);
    for flag in flags.chars() {
        match flag {
            'i' => {
                builder.case_insensitive(true);
            }
            'm' => {
                builder.multi_line(true);
            }
            's' => {
                builder.dot_matches_new_line(true);
            }
            _ => {}
        }
    }
    builder.build()
}

fn resolve_format(
    group: Option<&str>,
    shorthand: &Option<String>,
    if_value: &Option<String>,
    else_value: &Option<String>,
) -> String {
    let present = group.is_some_and(|value| !value.is_empty());
    if let Some(name) = shorthand {
        let value = group.unwrap_or("");
        return apply_shorthand(value, name);
    }
    if present {
        if let Some(if_value) = if_value {
            return if_value.clone();
        }
        return group.unwrap_or("").to_string();
    }
    if let Some(else_value) = else_value {
        return else_value.clone();
    }
    if if_value.is_some() {
        return String::new();
    }
    group.unwrap_or("").to_string()
}

fn apply_shorthand(value: &str, name: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    match name {
        "upcase" => value.to_uppercase(),
        "downcase" => value.to_lowercase(),
        "capitalize" => {
            let mut chars = value.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
        "pascalcase" => to_pascal_case(value),
        "camelcase" => to_camel_case(value),
        "kebabcase" => to_kebab_case(value),
        "snakecase" => to_snake_case(value),
        _ => value.to_string(),
    }
}

fn words(value: &str) -> Vec<&str> {
    static WORD_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"[\p{L}0-9]+").unwrap());
    WORD_RE.find_iter(value).map(|m| m.as_str()).collect()
}

fn to_pascal_case(value: &str) -> String {
    words(value)
        .into_iter()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

fn to_camel_case(value: &str) -> String {
    words(value)
        .into_iter()
        .enumerate()
        .map(|(index, word)| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) if index == 0 => {
                    first.to_lowercase().collect::<String>() + chars.as_str()
                }
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

fn to_kebab_case(value: &str) -> String {
    words(value)
        .into_iter()
        .map(|word| word.to_lowercase())
        .collect::<Vec<_>>()
        .join("-")
}

fn to_snake_case(value: &str) -> String {
    static CAMEL_BREAK_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(\p{Ll})(\p{Lu})").unwrap());
    static SEP_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"[\s\-]+").unwrap());
    let with_breaks = CAMEL_BREAK_RE.replace_all(value, "${1}_${2}").into_owned();
    SEP_RE
        .replace_all(&with_breaks, "_")
        .into_owned()
        .to_lowercase()
}

fn parse_pieces(mut source: &str, nested: bool) -> Result<(&str, Vec<Piece>)> {
    let mut pieces = Vec::new();
    loop {
        match source.chars().next() {
            None => return Ok(("", pieces)),
            Some('$') => {
                source = parse_dollar(&source[1..], &mut pieces)?;
            }
            Some('\\') => {
                source = &source[1..];
                if let Some(c) = source.chars().next() {
                    if c == '$' || c == '\\' || c == '}' {
                        push_text(&mut pieces, c.encode_utf8(&mut [0; 4]));
                        source = &source[1..];
                    } else {
                        push_text(&mut pieces, "\\");
                    }
                } else {
                    push_text(&mut pieces, "\\");
                }
            }
            Some('}') => {
                if nested {
                    return Ok((source, pieces));
                } else {
                    push_text(&mut pieces, "}");
                    source = &source[1..];
                }
            }
            Some(_) => {
                let chunk_end = source.find(['}', '$', '\\']).unwrap_or(source.len());
                let (chunk, rest) = source.split_at(chunk_end);
                push_text(&mut pieces, chunk);
                source = rest;
            }
        }
    }
}

fn push_text(pieces: &mut Vec<Piece>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(Piece::Text(existing)) = pieces.last_mut() {
        existing.push_str(text);
    } else {
        pieces.push(Piece::Text(text.to_string()));
    }
}

fn parse_dollar<'a>(source: &'a str, pieces: &mut Vec<Piece>) -> Result<&'a str> {
    if source.starts_with('{') {
        let inner = &source[1..];
        if let Ok((index, rest)) = parse_int(inner) {
            return parse_complex_placeholder(index, rest, pieces);
        }
        if let Some((name, rest)) = parse_ident(inner) {
            return parse_complex_variable(name, rest, pieces);
        }
        push_text(pieces, "${");
        return Ok(inner);
    }
    if let Ok((index, rest)) = parse_int(source) {
        pieces.push(Piece::TabStop {
            index,
            children: Vec::new(),
            choices: None,
            transform: None,
        });
        return Ok(rest);
    }
    if let Some((name, rest)) = parse_ident(source) {
        pieces.push(Piece::Variable {
            name: name.to_string(),
            children: Vec::new(),
            transform: None,
        });
        return Ok(rest);
    }
    push_text(pieces, "$");
    Ok(source)
}

fn parse_complex_placeholder<'a>(
    index: usize,
    mut source: &'a str,
    pieces: &mut Vec<Piece>,
) -> Result<&'a str> {
    let mut choices = None;
    let mut children = Vec::new();
    let mut transform = None;

    if source.starts_with('|') {
        let (rest, parsed) = parse_choices(&source[1..])?;
        source = rest;
        choices = parsed;
    }

    if source.starts_with(':') {
        let (rest, nested) = parse_pieces(&source[1..], true)?;
        source = rest;
        children = nested;
    }

    if source.starts_with('/') {
        let (rest, parsed) = parse_transform(&source[1..])?;
        source = rest;
        transform = Some(parsed);
    } else if source.starts_with('}') {
        source = &source[1..];
    } else {
        anyhow::bail!("expected a closing brace");
    }

    pieces.push(Piece::TabStop {
        index,
        children,
        choices,
        transform,
    });
    Ok(source)
}

fn parse_complex_variable<'a>(
    name: &str,
    mut source: &'a str,
    pieces: &mut Vec<Piece>,
) -> Result<&'a str> {
    let mut children = Vec::new();
    let mut transform = None;

    if source.starts_with(':') {
        let (rest, nested) = parse_pieces(&source[1..], true)?;
        source = rest;
        children = nested;
    }

    if source.starts_with('/') {
        let (rest, parsed) = parse_transform(&source[1..])?;
        source = rest;
        transform = Some(parsed);
    } else if source.starts_with('}') {
        source = &source[1..];
    } else {
        anyhow::bail!("expected a closing brace");
    }

    pieces.push(Piece::Variable {
        name: name.to_string(),
        children,
        transform,
    });
    Ok(source)
}

fn parse_transform(mut source: &str) -> Result<(&str, SnippetTransform)> {
    let mut pattern = String::new();
    loop {
        match source.chars().next() {
            None => anyhow::bail!("expected a closing brace"),
            Some('/') => {
                source = &source[1..];
                break;
            }
            Some('\\') => {
                source = &source[1..];
                if source.starts_with('/') {
                    pattern.push('/');
                    source = &source[1..];
                } else {
                    pattern.push('\\');
                }
            }
            Some(_) => {
                let chunk_end = source.find(['/', '\\']).unwrap_or(source.len());
                let (chunk, rest) = source.split_at(chunk_end);
                pattern.push_str(chunk);
                source = rest;
            }
        }
    }

    let mut format = Vec::new();
    loop {
        match source.chars().next() {
            None => anyhow::bail!("expected a closing brace"),
            Some('/') => {
                source = &source[1..];
                break;
            }
            Some('\\') => {
                source = &source[1..];
                if let Some(c) = source.chars().next() {
                    if c == '\\' || c == '/' {
                        push_format_text(&mut format, c.encode_utf8(&mut [0; 4]));
                        source = &source[c.len_utf8()..];
                    } else {
                        push_format_text(&mut format, "\\");
                    }
                } else {
                    push_format_text(&mut format, "\\");
                }
            }
            Some('$') => {
                if let Some((rest, replacement)) = try_parse_format_group(&source[1..]) {
                    source = rest;
                    format.push(replacement);
                } else {
                    push_format_text(&mut format, "$");
                    source = &source[1..];
                }
            }
            Some(_) => {
                let chunk_end = source.find(['/', '\\', '$']).unwrap_or(source.len());
                let (chunk, rest) = source.split_at(chunk_end);
                push_format_text(&mut format, chunk);
                source = rest;
            }
        }
    }

    let mut flags = String::new();
    loop {
        match source.chars().next() {
            None => anyhow::bail!("expected a closing brace"),
            Some('}') => {
                source = &source[1..];
                break;
            }
            Some(c) => {
                flags.push(c);
                source = &source[c.len_utf8()..];
            }
        }
    }

    let regex = compile_regex(&pattern, &flags).context("invalid snippet transform regex")?;
    Ok((
        source,
        SnippetTransform {
            pattern,
            format,
            flags,
            regex,
        },
    ))
}

fn push_format_text(format: &mut Vec<FormatReplacement>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(FormatReplacement::Text(existing)) = format.last_mut() {
        existing.push_str(text);
    } else {
        format.push(FormatReplacement::Text(text.to_string()));
    }
}

fn try_parse_format_group(source: &str) -> Option<(&str, FormatReplacement)> {
    if source.starts_with('{') {
        let (index, rest) = parse_int(&source[1..]).ok()?;
        if rest.starts_with('}') {
            return Some((
                &rest[1..],
                FormatReplacement::Group {
                    index,
                    shorthand: None,
                    if_value: None,
                    else_value: None,
                },
            ));
        }
        if !rest.starts_with(':') {
            return None;
        }
        let rest = &rest[1..];
        if rest.starts_with('/') {
            let (name, rest) = parse_ident(&rest[1..])?;
            if !rest.starts_with('}') {
                return None;
            }
            return Some((
                &rest[1..],
                FormatReplacement::Group {
                    index,
                    shorthand: Some(name.to_string()),
                    if_value: None,
                    else_value: None,
                },
            ));
        }
        if rest.starts_with('+') {
            let (value, rest) = split_until_unescaped(&rest[1..], '}')?;
            return Some((
                rest,
                FormatReplacement::Group {
                    index,
                    shorthand: None,
                    if_value: Some(value),
                    else_value: None,
                },
            ));
        }
        if rest.starts_with('-') {
            let (value, rest) = split_until_unescaped(&rest[1..], '}')?;
            return Some((
                rest,
                FormatReplacement::Group {
                    index,
                    shorthand: None,
                    if_value: None,
                    else_value: Some(value),
                },
            ));
        }
        if rest.starts_with('?') {
            let (if_value, rest) = split_until_unescaped(&rest[1..], ':')?;
            let (else_value, rest) = split_until_unescaped(rest, '}')?;
            return Some((
                rest,
                FormatReplacement::Group {
                    index,
                    shorthand: None,
                    if_value: Some(if_value),
                    else_value: Some(else_value),
                },
            ));
        }
        let (value, rest) = split_until_unescaped(rest, '}')?;
        return Some((
            rest,
            FormatReplacement::Group {
                index,
                shorthand: None,
                if_value: None,
                else_value: Some(value),
            },
        ));
    }

    let (index, rest) = parse_int(source).ok()?;
    Some((
        rest,
        FormatReplacement::Group {
            index,
            shorthand: None,
            if_value: None,
            else_value: None,
        },
    ))
}

fn split_until_unescaped(source: &str, delimiter: char) -> Option<(String, &str)> {
    let mut value = String::new();
    let mut rest = source;
    loop {
        match rest.chars().next() {
            None => return None,
            Some('\\') => {
                rest = &rest[1..];
                if let Some(c) = rest.chars().next() {
                    if c == '$' || c == '}' || c == '\\' {
                        value.push(c);
                        rest = &rest[c.len_utf8()..];
                    } else {
                        value.push('\\');
                    }
                } else {
                    value.push('\\');
                }
            }
            Some(c) if c == delimiter => {
                rest = &rest[c.len_utf8()..];
                return Some((value, rest));
            }
            Some(c) => {
                value.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
}

fn parse_int(source: &str) -> Result<(usize, &str)> {
    let len = source
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(source.len());
    anyhow::ensure!(len > 0, "expected an integer");
    let (prefix, suffix) = source.split_at(len);
    Ok((prefix.parse()?, suffix))
}

fn parse_ident(source: &str) -> Option<(&str, &str)> {
    let mut chars = source.chars();
    let first = chars.next()?;
    if first != '_' && !first.is_ascii_alphabetic() {
        return None;
    }
    let mut len = first.len_utf8();
    for c in chars {
        if c == '_' || c.is_ascii_alphanumeric() {
            len += c.len_utf8();
        } else {
            break;
        }
    }
    Some(source.split_at(len))
}

fn parse_choices(mut source: &str) -> Result<(&str, Option<Vec<String>>)> {
    let mut current_choice = String::new();
    let mut choices = Vec::new();

    loop {
        match source.chars().next() {
            None => return Ok(("", Some(choices))),
            Some('\\') => {
                source = &source[1..];
                if let Some(c) = source.chars().next() {
                    current_choice.push(c);
                    source = &source[c.len_utf8()..];
                }
            }
            Some(',') => {
                source = &source[1..];
                choices.push(std::mem::take(&mut current_choice));
            }
            Some('|') => {
                source = &source[1..];
                choices.push(current_choice);
                return Ok((source, Some(choices)));
            }
            Some(_) => {
                let chunk_end = source.find([',', '|', '\\']);
                anyhow::ensure!(
                    chunk_end.is_some(),
                    "Placeholder choice doesn't contain closing pipe-character '|'"
                );
                let (chunk, rest) = source.split_at(chunk_end.unwrap());
                current_choice.push_str(chunk);
                source = rest;
            }
        }
    }
}

fn tabstop_defaults(pieces: &[Piece], resolver: &dyn VariableResolver) -> BTreeMap<usize, String> {
    let mut defaults = BTreeMap::new();
    collect_defaults(pieces, resolver, &mut defaults);
    defaults
}

fn collect_defaults(
    pieces: &[Piece],
    resolver: &dyn VariableResolver,
    defaults: &mut BTreeMap<usize, String>,
) {
    for piece in pieces {
        match piece {
            Piece::Text(_) => {}
            Piece::TabStop {
                index,
                children,
                choices,
                transform,
            } => {
                if transform.is_none() && !defaults.contains_key(index) {
                    if let Some(choices) = choices {
                        if let Some(first) = choices.first() {
                            let mut value = first.clone();
                            value.push_str(&pieces_plain_text(children, resolver));
                            defaults.insert(*index, value);
                        }
                    } else if !children.is_empty() {
                        defaults.insert(*index, pieces_plain_text(children, resolver));
                    }
                }
                collect_defaults(children, resolver, defaults);
            }
            Piece::Variable { children, .. } => {
                collect_defaults(children, resolver, defaults);
            }
        }
    }
}

fn pieces_plain_text(pieces: &[Piece], resolver: &dyn VariableResolver) -> String {
    let mut text = String::new();
    for piece in pieces {
        match piece {
            Piece::Text(value) => text.push_str(value),
            Piece::TabStop {
                children,
                choices,
                transform,
                ..
            } => {
                if transform.is_some() {
                    continue;
                }
                if let Some(choices) = choices
                    && let Some(first) = choices.first()
                {
                    text.push_str(first);
                }
                text.push_str(&pieces_plain_text(children, resolver));
            }
            Piece::Variable {
                name,
                children,
                transform,
            } => {
                let mut value = resolver
                    .resolve(name)
                    .unwrap_or_else(|| pieces_plain_text(children, resolver));
                if let Some(transform) = transform {
                    value = transform.apply(&value);
                }
                text.push_str(&value);
            }
        }
    }
    text
}

fn emit_pieces(
    pieces: &[Piece],
    text: &mut String,
    tabstops: &mut BTreeMap<usize, TabStop>,
    defaults: &BTreeMap<usize, String>,
    resolver: &dyn VariableResolver,
) {
    for piece in pieces {
        match piece {
            Piece::Text(value) => text.push_str(value),
            Piece::Variable {
                name,
                children,
                transform,
            } => {
                let mut value = resolver
                    .resolve(name)
                    .unwrap_or_else(|| pieces_plain_text(children, resolver));
                if let Some(transform) = transform {
                    value = transform.apply(&value);
                }
                text.push_str(&value);
            }
            Piece::TabStop {
                index,
                children,
                choices,
                transform,
            } => {
                let start = text.len();
                if let Some(transform) = transform {
                    let source = defaults.get(index).cloned().unwrap_or_default();
                    text.push_str(&transform.apply(&source));
                } else {
                    if let Some(choices) = choices
                        && let Some(first) = choices.first()
                    {
                        text.push_str(first);
                    }
                    emit_pieces(children, text, tabstops, defaults, resolver);
                }
                let range = start as isize..text.len() as isize;
                tabstops
                    .entry(*index)
                    .or_insert_with(|| TabStop {
                        ranges: Default::default(),
                        choices: choices.clone(),
                        transforms: Default::default(),
                    })
                    .add_range(range, transform.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snippet_without_tabstops() {
        let snippet = Snippet::parse("one-two-three").unwrap();
        assert_eq!(snippet.text, "one-two-three");
        assert_eq!(tabstops(&snippet), &[vec![13..13]]);
    }

    #[test]
    fn test_snippet_with_tabstops() {
        let snippet = Snippet::parse("one$1two").unwrap();
        assert_eq!(snippet.text, "onetwo");
        assert_eq!(tabstops(&snippet), &[vec![3..3], vec![6..6]]);
        assert_eq!(tabstop_choices(&snippet), &[&None, &None]);

        // Multi-digit numbers
        let snippet = Snippet::parse("one$123-$99-two").unwrap();
        assert_eq!(snippet.text, "one--two");
        assert_eq!(tabstops(&snippet), &[vec![4..4], vec![3..3], vec![8..8]]);
        assert_eq!(tabstop_choices(&snippet), &[&None, &None, &None]);
    }

    #[test]
    fn test_snippet_with_last_tabstop_at_end() {
        let snippet = Snippet::parse(r#"foo.$1"#).unwrap();

        // If the final tabstop is already at the end of the text, don't insert
        // an additional tabstop at the end.
        assert_eq!(snippet.text, r#"foo."#);
        assert_eq!(tabstops(&snippet), &[vec![4..4]]);
        assert_eq!(tabstop_choices(&snippet), &[&None]);
    }

    #[test]
    fn test_snippet_with_explicit_final_tabstop() {
        let snippet = Snippet::parse(r#"<div class="$1">$0</div>"#).unwrap();

        // If the final tabstop is explicitly specified via '$0', then
        // don't insert an additional tabstop at the end.
        assert_eq!(snippet.text, r#"<div class=""></div>"#);
        assert_eq!(tabstops(&snippet), &[vec![12..12], vec![14..14]]);
        assert_eq!(tabstop_choices(&snippet), &[&None, &None]);
    }

    #[test]
    fn test_snippet_with_placeholders() {
        let snippet = Snippet::parse("one${1:two}three${2:four}").unwrap();
        assert_eq!(snippet.text, "onetwothreefour");
        assert_eq!(
            tabstops(&snippet),
            &[vec![3..6], vec![11..15], vec![15..15]]
        );
        assert_eq!(tabstop_choices(&snippet), &[&None, &None, &None]);
    }

    #[test]
    fn test_snippet_with_choice_placeholders() {
        let snippet = Snippet::parse("type ${1|i32, u32|} = $2")
            .expect("Should be able to unpack choice placeholders");

        assert_eq!(snippet.text, "type i32 = ");
        assert_eq!(tabstops(&snippet), &[vec![5..8], vec![11..11],]);
        assert_eq!(
            tabstop_choices(&snippet),
            &[&Some(vec!["i32".to_string(), " u32".to_string()]), &None]
        );

        let snippet = Snippet::parse(r"${1|\$\{1\|one\,two\,tree\|\}|}")
            .expect("Should be able to parse choice with escape characters");

        assert_eq!(snippet.text, "${1|one,two,tree|}");
        assert_eq!(tabstops(&snippet), &[vec![0..18], vec![18..18]]);
        assert_eq!(
            tabstop_choices(&snippet),
            &[&Some(vec!["${1|one,two,tree|}".to_string(),]), &None]
        );
    }

    #[test]
    fn test_snippet_with_escaped_chars_in_non_default_choices() {
        let snippet = Snippet::parse(r"${1|a,b\,c|}").unwrap();
        assert_eq!(snippet.text, "a");
        assert_eq!(tabstops(&snippet), &[vec![0..1], vec![1..1]]);
        assert_eq!(
            tabstop_choices(&snippet),
            &[&Some(vec!["a".to_string(), "b,c".to_string()]), &None]
        );

        let snippet = Snippet::parse(r"${1|one,two\|2,three\\3|}").unwrap();
        assert_eq!(snippet.text, "one");
        assert_eq!(tabstops(&snippet), &[vec![0..3], vec![3..3]]);
        assert_eq!(
            tabstop_choices(&snippet),
            &[
                &Some(vec![
                    "one".to_string(),
                    "two|2".to_string(),
                    r"three\3".to_string()
                ]),
                &None
            ]
        );
    }

    #[test]
    fn test_snippet_with_nested_placeholders() {
        let snippet = Snippet::parse(
            "for (${1:var ${2:i} = 0; ${2:i} < ${3:${4:array}.length}; ${2:i}++}) {$0}",
        )
        .unwrap();
        assert_eq!(snippet.text, "for (var i = 0; i < array.length; i++) {}");
        assert_eq!(
            tabstops(&snippet),
            &[
                vec![5..37],
                vec![9..10, 16..17, 34..35],
                vec![20..32],
                vec![20..25],
                vec![40..40],
            ]
        );
        assert_eq!(
            tabstop_choices(&snippet),
            &[&None, &None, &None, &None, &None]
        );
    }

    #[test]
    fn test_snippet_parsing_with_escaped_chars() {
        let snippet = Snippet::parse("\"\\$schema\": $1").unwrap();
        assert_eq!(snippet.text, "\"$schema\": ");
        assert_eq!(tabstops(&snippet), &[vec![11..11]]);
        assert_eq!(tabstop_choices(&snippet), &[&None]);

        let snippet = Snippet::parse("{a\\}").unwrap();
        assert_eq!(snippet.text, "{a}");
        assert_eq!(tabstops(&snippet), &[vec![3..3]]);
        assert_eq!(tabstop_choices(&snippet), &[&None]);

        // backslash not functioning as an escape
        let snippet = Snippet::parse("a\\b").unwrap();
        assert_eq!(snippet.text, "a\\b");
        assert_eq!(tabstops(&snippet), &[vec![3..3]]);

        // first backslash cancelling escaping that would
        // have happened with second backslash
        let snippet = Snippet::parse("one\\\\$1two").unwrap();
        assert_eq!(snippet.text, "one\\two");
        assert_eq!(tabstops(&snippet), &[vec![4..4], vec![7..7]]);
    }

    #[test]
    fn test_snippet_regex_transform_mirror_with_choice() {
        let snippet = Snippet::parse(
            "[${1}](/${2|tags,creators,parodies,sources,categories|}/${1/(.*)/${1:/downcase}/}/)",
        )
        .unwrap();
        assert_eq!(snippet.text, "[](/tags//)");
        assert_eq!(
            tabstops(&snippet),
            &[vec![1..1, 9..9], vec![4..8], vec![11..11]]
        );
        assert_eq!(
            tabstop_choices(&snippet),
            &[
                &None,
                &Some(vec![
                    "tags".to_string(),
                    "creators".to_string(),
                    "parodies".to_string(),
                    "sources".to_string(),
                    "categories".to_string(),
                ]),
                &None
            ]
        );
        let selectable: Vec<_> = snippet.tabstops[0]
            .selection_ranges()
            .into_iter()
            .cloned()
            .collect();
        assert_eq!(selectable, vec![1..1]);
        let mirrors: Vec<_> = snippet.tabstops[0].transform_mirrors().collect();
        assert_eq!(mirrors.len(), 1);
        assert_eq!(mirrors[0].0, &(9..9));
        assert_eq!(mirrors[0].1.apply("MixedCase"), "mixedcase");
        assert_eq!(mirrors[0].1.apply("FooBar"), "foobar");
    }

    #[test]
    fn test_snippet_transform_flags_and_conditionals() {
        let snippet = Snippet::parse("${1:aa}${1/(a)/x/g}").unwrap();
        assert_eq!(snippet.text, "aaxx");

        let snippet = Snippet::parse("${1:Hello}${1/(h)/x/i}").unwrap();
        assert_eq!(snippet.text, "Helloxello");

        let snippet = Snippet::parse("${1:foo}${1/(bar)/${1:-missed}/}").unwrap();
        assert_eq!(snippet.text, "foomissed");

        let snippet = Snippet::parse("${1:foo}${1/(foo)/${1:?yes:no}/}").unwrap();
        assert_eq!(snippet.text, "fooyes");

        let snippet = Snippet::parse("${1:foo}${1/(bar)/${1:?yes:no}/}").unwrap();
        assert_eq!(snippet.text, "foono");

        let snippet = Snippet::parse("${1:foo}${1/(foo)/${1:+hit}/}").unwrap();
        assert_eq!(snippet.text, "foohit");
    }

    fn tabstops(snippet: &Snippet) -> Vec<Vec<Range<isize>>> {
        snippet.tabstops.iter().map(|t| t.ranges.to_vec()).collect()
    }

    fn tabstop_choices(snippet: &Snippet) -> Vec<&Option<Vec<String>>> {
        snippet.tabstops.iter().map(|t| &t.choices).collect()
    }
}
