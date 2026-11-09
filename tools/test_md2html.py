#!/usr/bin/env python3
"""Regression tests for the documentation's code blocks.

python3 -m unittest discover -s tools -p 'test_*.py'
"""

import unittest
from html.parser import HTMLParser

import md2html


class CodeText(HTMLParser):
    # Read highlighted HTML back as text to check copyable source is unchanged.
    def __init__(self, markup):
        # Collect text nodes after HTML entities have been decoded.
        super().__init__()
        self.text = ""
        self.feed(markup)

    def handle_data(self, data):
        # Keep all whitespace from the preformatted code block.
        self.text += data


class CodeBlocks(unittest.TestCase):
    # Exercise syntax tokens against source with quotes, comments and HTML.
    def test_c_directives_after_the_first_line(self):
        # Whole-block rendering must still recognize each preprocessor line.
        source = "#include <stdio.h>\n#include <stdint.h>\nint count = 2;"
        markup = md2html.highlighted(
            source.splitlines(), set(), md2html.SOURCE_TOKENS["c"]
        )
        self.assertEqual(markup.count('<span class="attr">#include</span>'), 2)
        self.assertEqual(CodeText(markup).text, source + "\n")

    def test_nix_strings_and_comments(self):
        # Keywords in a string or comment must retain the surrounding token.
        source = 'let\n  clock = "if < 3 & true"; # inherit\nin { epoch = 1790985600; }'
        markup = md2html.highlighted(
            source.splitlines(), set(), md2html.SOURCE_TOKENS["nix"]
        )
        self.assertIn('<span class="keyword">let</span>', markup)
        self.assertIn('<span class="string">"if &lt; 3 &amp; true"</span>', markup)
        self.assertIn('<span class="comment"># inherit</span>', markup)
        self.assertIn('<span class="number">1790985600</span>', markup)
        self.assertEqual(CodeText(markup).text, source + "\n")

    def test_nix_multiline_string_with_marked_line(self):
        # A marked line inside an indented string must still be string text.
        source = "script = ''\n  echo if < 3 # shell text\n'';"
        markup = md2html.highlighted(
            source.splitlines(), {2}, md2html.SOURCE_TOKENS["nix"]
        )
        self.assertIn(
            '<span class="mark"><span class="string">  echo if &lt; 3 # shell text</span></span>',
            markup,
        )
        self.assertEqual(CodeText(markup).text, source + "\n")

    def test_shell_fences(self):
        # Shell aliases must highlight options and quotes without eating flake fragments.
        source = 'set -o pipefail\nrewind cat "$run" --archive tar | tar -xf - # export\nnix build .#stamp'
        for language in ("sh", "bash", "shell"):
            with self.subTest(language=language):
                markup = md2html.highlighted(
                    source.splitlines(), set(), md2html.SOURCE_TOKENS[language]
                )
                self.assertIn('<span class="option">--archive</span>', markup)
                self.assertIn('<span class="string">"$run"</span>', markup)
                self.assertIn('<span class="operator">|</span>', markup)
                self.assertIn('<span class="comment"># export</span>', markup)
                self.assertIn(".#stamp", markup)
                self.assertEqual(CodeText(markup).text, source + "\n")

    def test_console_commands_leave_output_alone(self):
        # Highlight prompted commands and continuations, preserving recorded output colors.
        lines = [
            "$ nix build .#stamp \\",
            '> --rebuild "$out"',
            "error: --rebuild <failed>",
            "> output",
        ]
        spans = [(2, 0, 6, "red")]
        markup = md2html.console(lines, spans)
        self.assertIn('<span class="option">--rebuild</span>', markup)
        self.assertIn('<span class="string">"$out"</span>', markup)
        self.assertIn(
            '<span class="ansi-red">error:</span> --rebuild &lt;failed&gt;', markup
        )
        self.assertIn("\n&gt; output", markup)
        self.assertEqual(CodeText(markup).text, "\n".join(lines) + "\n")


if __name__ == "__main__":
    unittest.main()
