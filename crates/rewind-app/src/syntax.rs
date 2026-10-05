//! Syntax highlighting for the source panel and the file viewer.
//!
//! A file's language comes from its name, then its extension, then the
//! interpreter its first line names. Its lines are parsed with syntect's
//! Sublime Text grammars, which `build.rs` compiles from vendor/syntaxes,
//! and each span of text is colored by what its scopes say it is, in the
//! app's own colors (`theme::syntax`) rather than a syntect theme's.
//!
//! A grammar parses a file from its start, so a line's colors depend on
//! every line before it. `Highlighter` parses only as far as the lines on
//! screen and keeps what it parsed, so opening a long file at its top costs
//! the lines shown, once. A line far down a long file, as the source panel
//! opens at, would hold up a frame for every line above it, about a quarter
//! of a second for 5,000 lines of C; such a line shows plain while a copy
//! of the highlighter finishes the file on a background thread.

use std::ops::Range;
use std::sync::OnceLock;

use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxSet};

use crate::theme;

/// The syntax set `build.rs` dumps.
static DUMP: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/syntaxes.packdump"));

/// The syntax set, loaded on first use.
fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(|| syntect::dumps::from_binary(DUMP))
}

/// A language the app highlights.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    C,
    Cpp,
    Rust,
    Go,
    Python,
    Shell,
    Makefile,
    Markdown,
    Nix,
    Assembly,
    Dockerfile,
}

/// Files whose name alone says their language. env-vars is the shell
/// script of `declare -x` lines Nix writes into a build's directory.
const BY_NAME: &[(&str, Language)] = &[
    ("env-vars", Language::Shell),
    ("Makefile", Language::Makefile),
    ("makefile", Language::Makefile),
    ("GNUmakefile", Language::Makefile),
    ("Dockerfile", Language::Dockerfile),
    ("Containerfile", Language::Dockerfile),
];

/// Extensions and their languages. Case counts: .S is assembly run
/// through the C preprocessor first.
const BY_EXTENSION: &[(&str, Language)] = &[
    ("c", Language::C),
    ("h", Language::C),
    ("cc", Language::Cpp),
    ("cpp", Language::Cpp),
    ("cxx", Language::Cpp),
    ("hh", Language::Cpp),
    ("hpp", Language::Cpp),
    ("hxx", Language::Cpp),
    ("rs", Language::Rust),
    ("go", Language::Go),
    ("py", Language::Python),
    ("sh", Language::Shell),
    ("bash", Language::Shell),
    ("mk", Language::Makefile),
    ("md", Language::Markdown),
    ("nix", Language::Nix),
    ("s", Language::Assembly),
    ("S", Language::Assembly),
    ("asm", Language::Assembly),
    ("dockerfile", Language::Dockerfile),
];

/// Interpreters a first line's `#!` can name, by their names without a
/// version.
const BY_INTERPRETER: &[(&str, Language)] = &[
    ("sh", Language::Shell),
    ("bash", Language::Shell),
    ("dash", Language::Shell),
    ("python", Language::Python),
    ("make", Language::Makefile),
];

/// What starts a script's first line, and the program that runs the
/// interpreter named after it.
const SHEBANG: &str = "#!";
const ENV: &str = "env";

impl Language {
    /// The language of the file at `path`, whose first line is
    /// `first_line`: by its name, then its extension, then the interpreter
    /// a `#!` line names. None for a file of no language the app knows.
    pub fn of(path: &str, first_line: &str) -> Option<Language> {
        let name = path.rsplit('/').next().unwrap_or(path);
        let by_name = BY_NAME.iter().find(|(n, _)| *n == name);
        let by_extension = || {
            let (_, extension) = name.rsplit_once('.')?;
            BY_EXTENSION.iter().find(|(e, _)| *e == extension)
        };
        let known = by_name.or_else(by_extension).map(|&(_, language)| language);
        known.or_else(|| interpreted_by(first_line))
    }

    /// The grammar of the language.
    fn syntax(self) -> Option<&'static syntect::parsing::SyntaxReference> {
        syntaxes().find_syntax_by_name(self.grammar())
    }

    /// The name of the language's grammar in the syntax set.
    fn grammar(self) -> &'static str {
        match self {
            Language::C => "C",
            Language::Cpp => "C++",
            Language::Rust => "Rust",
            Language::Go => "Go",
            Language::Python => "Python",
            Language::Shell => "Bourne Again Shell (bash)",
            Language::Makefile => "Makefile",
            Language::Markdown => "Markdown",
            Language::Nix => "Nix",
            Language::Assembly => "asm",
            Language::Dockerfile => "Dockerfile (with bash)",
        }
    }
}

/// The language of the interpreter a script's first line names after
/// `#!`, itself or through env, by its name without a version.
fn interpreted_by(first_line: &str) -> Option<Language> {
    let command = first_line.strip_prefix(SHEBANG)?;
    let mut words = command.split_whitespace();
    let mut program = words.next()?.rsplit('/').next()?;
    if program == ENV {
        program = words.next()?;
    }
    let name = program.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    BY_INTERPRETER
        .iter()
        .find(|(n, _)| *n == name)
        .map(|&(_, language)| language)
}

/// What a span of source text is, as the app colors it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    /// Anything else, punctuation and operators among it: the row's own
    /// color.
    Plain,
    Comment,
    Keyword,
    String,
    Number,
    Type,
    Function,
}

impl Token {
    /// The token's color.
    pub fn color(self) -> u32 {
        match self {
            Token::Plain => theme::SOFT,
            Token::Comment => theme::syntax::COMMENT,
            Token::Keyword => theme::syntax::KEYWORD,
            Token::String => theme::syntax::STRING,
            Token::Number => theme::syntax::NUMBER,
            Token::Type => theme::syntax::TYPE,
            Token::Function => theme::syntax::FUNCTION,
        }
    }
}

/// Scope prefixes and the token each makes. A span's scopes are tried
/// innermost first, and the first scope with a prefix here decides: the
/// quote around a string is punctuation inside the string's scope, so it
/// takes the string's color. Within one scope the longer prefix comes
/// first, so operators stay plain though they are keywords to Sublime.
const SCOPE_TOKENS: &[(&str, Token)] = &[
    ("keyword.operator", Token::Plain),
    ("comment", Token::Comment),
    ("string", Token::String),
    ("constant.character", Token::String),
    ("markup.raw", Token::String),
    ("constant.numeric", Token::Number),
    ("constant.language", Token::Keyword),
    ("variable.language", Token::Keyword),
    ("keyword", Token::Keyword),
    ("storage.type", Token::Type),
    ("storage", Token::Keyword),
    ("markup.heading", Token::Keyword),
    ("entity.name.type", Token::Type),
    ("entity.name.struct", Token::Type),
    ("entity.name.enum", Token::Type),
    ("entity.name.union", Token::Type),
    ("entity.name.class", Token::Type),
    ("support.type", Token::Type),
    ("entity.name.function", Token::Function),
    ("support.function", Token::Function),
    ("variable.function", Token::Function),
    ("entity.name.label", Token::Function),
];

/// The token of text under `scopes`, outermost first, as a scope stack
/// lists them.
pub fn token_of(scopes: &[Scope]) -> Token {
    let prefixes = scope_tokens();
    for &scope in scopes.iter().rev() {
        let decided = prefixes
            .iter()
            .find(|(prefix, _)| prefix.is_prefix_of(scope));
        if let Some(&(_, token)) = decided {
            return token;
        }
    }
    Token::Plain
}

/// SCOPE_TOKENS with each prefix made a scope once.
fn scope_tokens() -> &'static [(Scope, Token)] {
    static PREFIXES: OnceLock<Vec<(Scope, Token)>> = OnceLock::new();
    PREFIXES.get_or_init(|| {
        SCOPE_TOKENS
            .iter()
            .map(|&(prefix, token)| (Scope::new(prefix).expect("a valid scope"), token))
            .collect()
    })
}

/// A part of a line in one token, by its byte range in the line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub range: Range<usize>,
    pub token: Token,
}

/// How many lines past those already parsed a frame parses on the spot,
/// about 20 milliseconds of C; a line further on waits for the rest of the
/// file to be parsed off the UI thread.
pub const PARSED_IN_A_FRAME: usize = 400;

/// Whether a line too far to parse in a frame has been asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rest {
    NotAsked,
    /// Asked for, and waiting for `to_finish` to hand the rest over.
    Asked,
    /// Handed over to be finished.
    Finishing,
}

/// A file's lines highlighted as far as they have been asked for.
#[derive(Clone)]
pub struct Highlighter {
    /// The grammar's state after the last line parsed: None for a file of
    /// no known language, or once a line failed to parse.
    state: Option<(ParseState, ScopeStack)>,
    /// The spans of each line parsed, in order from the first, Plain
    /// spans left out.
    parsed: Vec<Vec<Span>>,
    rest: Rest,
}

impl Highlighter {
    /// A highlighter for a file in `language`, or none.
    pub fn new(language: Option<Language>) -> Highlighter {
        let state = language
            .and_then(Language::syntax)
            .map(|syntax| (ParseState::new(syntax), ScopeStack::new()));
        Highlighter {
            state,
            parsed: Vec::new(),
            rest: Rest::NotAsked,
        }
    }

    /// The colored spans of line `row` of `lines`, parsing the lines
    /// before it first when they have not been. Empty for a line of a
    /// file of no known language, and for a line more than
    /// PARSED_IN_A_FRAME past the lines parsed, which asks for the rest
    /// of the file to be finished instead.
    pub fn spans(&mut self, lines: &[String], row: usize) -> &[Span] {
        let far = row >= self.parsed.len() + PARSED_IN_A_FRAME;
        if far && self.state.is_some() {
            if self.rest == Rest::NotAsked {
                self.rest = Rest::Asked;
            }
            return &[];
        }
        self.parse_to(lines, row);
        self.parsed.get(row).map_or(&[], Vec::as_slice)
    }

    /// A copy to finish the file with, off the UI thread, once a line too
    /// far to parse in a frame has been asked for; None otherwise, and
    /// after the first copy.
    pub fn to_finish(&mut self) -> Option<Highlighter> {
        if self.rest != Rest::Asked {
            return None;
        }
        self.rest = Rest::Finishing;
        Some(Highlighter {
            rest: Rest::NotAsked,
            ..self.clone()
        })
    }

    /// This highlighter with every line of `lines` parsed.
    pub fn finish(mut self, lines: &[String]) -> Highlighter {
        self.parse_to(lines, lines.len());
        self
    }

    /// Parses the lines up to and including `row`, or to the end.
    fn parse_to(&mut self, lines: &[String], row: usize) {
        while self.parsed.len() <= row && self.parsed.len() < lines.len() {
            let spans = self.parse(&lines[self.parsed.len()]);
            self.parsed.push(spans);
        }
    }

    /// The spans of the next line, `line`, carrying the grammar's state
    /// on to the line after it. A line the grammar fails on ends the
    /// highlighting: it and the lines after it are plain.
    fn parse(&mut self, line: &str) -> Vec<Span> {
        let Some((state, stack)) = &mut self.state else {
            return Vec::new();
        };

        // The grammars match lines with their newline.
        let mut text = String::with_capacity(line.len() + 1);
        text.push_str(line);
        text.push('\n');
        let Ok(ops) = state.parse_line(&text, syntaxes()) else {
            self.state = None;
            return Vec::new();
        };

        // Each change of scope ends the span before it.
        let mut spans: Vec<Span> = Vec::new();
        let mut start = 0;
        for (at, op) in ops {
            push_span(
                &mut spans,
                start..at.min(line.len()),
                token_of(stack.as_slice()),
            );
            start = start.max(at.min(line.len()));
            if stack.apply(&op).is_err() {
                self.state = None;
                return Vec::new();
            }
        }
        push_span(&mut spans, start..line.len(), token_of(stack.as_slice()));
        spans
    }
}

/// Adds a span of `token` over `range`, joined to the span before when
/// that one has the same token and ends where `range` starts. Empty and
/// Plain spans are left out.
fn push_span(spans: &mut Vec<Span>, range: Range<usize>, token: Token) {
    if range.is_empty() || token == Token::Plain {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.token == token
        && last.range.end == range.start
    {
        last.range.end = range.end;
        return;
    }
    spans.push(Span { range, token });
}

#[cfg(test)]
mod tests {
    // The language chosen for a file by name, extension and first line;
    // scopes as syntect's grammars name them turned into tokens and the
    // theme's colors; and a few lines of C parsed into spans.
    use super::*;

    /// Files by name: Nix's env-vars is Bash, Makefiles and Dockerfiles
    /// by their usual names; a name wins over an extension.
    #[test]
    fn a_file_s_name_picks_its_language() {
        assert_eq!(Language::of("/build/env-vars", ""), Some(Language::Shell));
        assert_eq!(Language::of("Makefile", ""), Some(Language::Makefile));
        assert_eq!(
            Language::of("/src/GNUmakefile", ""),
            Some(Language::Makefile)
        );
        assert_eq!(
            Language::of("/src/Dockerfile", ""),
            Some(Language::Dockerfile)
        );
        assert_eq!(
            Language::of("Containerfile", "FROM alpine"),
            Some(Language::Dockerfile)
        );
    }

    /// Files by extension, with the case of .S and .s both assembly, and
    /// a file of no known extension or name in no language.
    #[test]
    fn a_file_s_extension_picks_its_language() {
        let cases = [
            ("src/pool.c", Some(Language::C)),
            ("include/pool.h", Some(Language::C)),
            ("lib/thing.cpp", Some(Language::Cpp)),
            ("src/main.rs", Some(Language::Rust)),
            ("cmd/main.go", Some(Language::Go)),
            ("tools/md2html.py", Some(Language::Python)),
            ("build.sh", Some(Language::Shell)),
            ("rules.mk", Some(Language::Makefile)),
            ("README.md", Some(Language::Markdown)),
            ("flake.nix", Some(Language::Nix)),
            ("/build/ccCasQ2y.s", Some(Language::Assembly)),
            (
                "../sysdeps/unix/sysv/linux/x86_64/clone3.S",
                Some(Language::Assembly),
            ),
            ("boot.asm", Some(Language::Assembly)),
            ("app.dockerfile", Some(Language::Dockerfile)),
            ("/build/ccX0Zq9G.res", None),
            ("/build/output", None),
        ];
        for (path, language) in cases {
            assert_eq!(Language::of(path, ""), language, "{path}");
        }
    }

    /// A file without a known name or extension takes the language of
    /// the interpreter its #! line names, directly or through env, with
    /// a version after the name; a file with a known extension keeps it.
    #[test]
    fn a_shebang_picks_the_language_of_a_script() {
        assert_eq!(
            Language::of("/build/configure", "#!/bin/sh"),
            Some(Language::Shell)
        );
        assert_eq!(
            Language::of("run", "#!/usr/bin/env bash"),
            Some(Language::Shell)
        );
        assert_eq!(
            Language::of("bin/tool", "#! /usr/bin/env python3.12 -u"),
            Some(Language::Python)
        );
        assert_eq!(
            Language::of("debian/rules", "#!/usr/bin/make -f"),
            Some(Language::Makefile)
        );
        assert_eq!(Language::of("bin/tool", "#!/usr/bin/perl"), None);
        assert_eq!(Language::of("notes", "# a heading"), None);
        assert_eq!(Language::of("x.c", "#!/bin/sh"), Some(Language::C));
    }

    /// Every language names a grammar the syntax set has.
    #[test]
    fn every_language_has_a_grammar() {
        let languages = [
            Language::C,
            Language::Cpp,
            Language::Rust,
            Language::Go,
            Language::Python,
            Language::Shell,
            Language::Makefile,
            Language::Markdown,
            Language::Nix,
            Language::Assembly,
            Language::Dockerfile,
        ];
        for language in languages {
            assert!(
                syntaxes().find_syntax_by_name(language.grammar()).is_some(),
                "{language:?}"
            );
        }
    }

    fn scopes(text: &str) -> Vec<Scope> {
        let stack: ScopeStack = text.parse().unwrap();
        stack.as_slice().to_vec()
    }

    /// Scopes as the C, Python and Markdown grammars name them become
    /// tokens by the innermost scope with a known prefix: a string's
    /// quote is the string's, an operator is plain, unknown scopes fall
    /// through to the outer ones. Each token has the theme's color, and
    /// plain text the row's soft text color.
    #[test]
    fn scopes_become_tokens_and_the_theme_s_colors() {
        let cases = [
            ("source.c comment.line.double-slash.c", Token::Comment),
            (
                "source.c string.quoted.double.c punctuation.definition.string.begin.c",
                Token::String,
            ),
            ("source.c meta.block.c keyword.control.c", Token::Keyword),
            ("source.c keyword.operator.assignment.c", Token::Plain),
            ("source.c storage.type.c", Token::Type),
            ("source.c storage.modifier.c", Token::Keyword),
            ("source.c constant.numeric.integer.decimal.c", Token::Number),
            (
                "source.c meta.function-call.c variable.function.c",
                Token::Function,
            ),
            (
                "source.c meta.function.c entity.name.function.c",
                Token::Function,
            ),
            ("source.python constant.language.python", Token::Keyword),
            (
                "text.html.markdown markup.heading.1.markdown",
                Token::Keyword,
            ),
            (
                "source.c meta.block.c punctuation.terminator.c",
                Token::Plain,
            ),
        ];
        for (stack, token) in cases {
            assert_eq!(token_of(&scopes(stack)), token, "{stack}");
        }

        assert_eq!(Token::Comment.color(), theme::syntax::COMMENT);
        assert_eq!(Token::Keyword.color(), theme::syntax::KEYWORD);
        assert_eq!(Token::String.color(), theme::syntax::STRING);
        assert_eq!(Token::Number.color(), theme::syntax::NUMBER);
        assert_eq!(Token::Type.color(), theme::syntax::TYPE);
        assert_eq!(Token::Function.color(), theme::syntax::FUNCTION);
        assert_eq!(Token::Plain.color(), theme::SOFT);
    }

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|l| l.to_string()).collect()
    }

    /// A few lines of C highlight into spans by byte range in each line:
    /// the include and its header, the type and function name, the
    /// keyword, the number and the comment. A block comment opened on one
    /// line colors the next, which needs the line before parsed first,
    /// asked for or not.
    #[test]
    fn a_c_snippet_highlights_into_spans() {
        let source = lines(&[
            "#include <stdio.h>",
            "int main(void) {",
            "\treturn 0; // done",
            "}",
            "/* open",
            "still comment */ int x;",
        ]);
        let mut highlighter = Highlighter::new(Some(Language::C));
        let span = |range: Range<usize>, token| Span { range, token };

        assert_eq!(
            highlighter.spans(&source, 5),
            &[span(0..16, Token::Comment), span(17..20, Token::Type)]
        );
        assert_eq!(
            highlighter.spans(&source, 0),
            &[span(0..8, Token::Keyword), span(9..18, Token::String)]
        );
        assert_eq!(
            highlighter.spans(&source, 1),
            &[
                span(0..3, Token::Type),
                span(4..8, Token::Function),
                span(9..13, Token::Type),
            ]
        );
        assert_eq!(
            highlighter.spans(&source, 2),
            &[
                span(1..7, Token::Keyword),
                span(8..9, Token::Number),
                span(11..18, Token::Comment),
            ]
        );
        assert_eq!(highlighter.spans(&source, 3), &[]);
    }

    /// A line further past the parsed lines than a frame parses is plain
    /// at first and asks once for the rest of the file, which a copy of
    /// the highlighter finishes as a background thread would; the
    /// finished copy has the spans parsing line by line gives. A C file
    /// of a comment and many declarations.
    #[test]
    fn a_far_line_waits_for_the_file_to_be_finished() {
        let mut source = vec!["/* far */".to_string()];
        source.extend((0..PARSED_IN_A_FRAME * 2).map(|i| format!("int x{i};")));
        let last = source.len() - 1;

        let mut lazy = Highlighter::new(Some(Language::C));
        assert_eq!(lazy.spans(&source, last), &[]);
        let rest = lazy.to_finish().expect("the far line asked for the rest");
        assert!(lazy.to_finish().is_none());

        let mut finished = rest.finish(&source);
        let mut stepped = Highlighter::new(Some(Language::C));
        for row in 0..=last {
            let expected = stepped.spans(&source, row).to_vec();
            assert_eq!(finished.spans(&source, row), expected.as_slice());
        }
        assert_eq!(
            finished.spans(&source, last),
            &[Span {
                range: 0..3,
                token: Token::Type
            }]
        );
        assert!(finished.to_finish().is_none());
    }

    /// A file of no known language has no spans, and a row past the
    /// file's end has none either.
    #[test]
    fn plain_text_has_no_spans() {
        let source = lines(&["int main(void) {", "}"]);
        let mut plain = Highlighter::new(None);
        assert_eq!(plain.spans(&source, 0), &[]);
        let mut c = Highlighter::new(Some(Language::C));
        assert_eq!(c.spans(&source, 7), &[]);
    }
}
