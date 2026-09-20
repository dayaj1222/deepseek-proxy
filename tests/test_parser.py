import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from deepseek_proxy.tools import format as fmt
from deepseek_proxy.tools.parser import Event, ToolParser


def parse(chunks, **kwargs):
    parser = ToolParser(**kwargs)
    events = []
    for chunk in chunks:
        events.extend(parser.feed(chunk))
    events.extend(parser.finish())
    assert parser.finish() == []
    normalized = []
    for event in events:
        if event.kind == "tool":
            event = Event(
                "tool",
                (event.value["function"]["name"], json.loads(event.value["function"]["arguments"])),
            )
        if normalized and event.kind == normalized[-1].kind == "text":
            normalized[-1].value += event.value
        else:
            normalized.append(event)
    return normalized


class ToolParserTests(unittest.TestCase):
    def invariant(self, source, expected, **kwargs):
        self.assertEqual(parse([source], **kwargs), expected)
        self.assertEqual(parse(list(source), **kwargs), expected)
        for split in range(len(source) + 1):
            self.assertEqual(parse([source[:split], source[split:]], **kwargs), expected)

    def test_all_dialects(self):
        for dialect in fmt.DIALECTS.values():
            if dialect.body_kind == "json":
                body = '{"x": [1, {"a": true}]}'
            else:
                body = f'{dialect.param_open} name="x" string="false">[1, {{"a": true}}]{dialect.param_close}'
            source = (
                f'Before{dialect.wrapper_open}{dialect.invoke_open} name="f">'
                f"{body}{dialect.invoke_close}{dialect.wrapper_close}After"
            )
            self.invariant(
                source,
                [
                    Event("text", "Before"),
                    Event("tool", ("f", {"x": [1, {"a": True}]})),
                    Event("text", "After"),
                ],
            )

    def test_dsml_mixed_pipes_and_spaces(self):
        source = '<｜｜DSML｜ invoke name="f"><｜DSML｜｜ parameter name="x" string="true">ok</｜｜DSML｜ parameter></｜DSML｜｜ invoke>'
        self.invariant(source, [Event("tool", ("f", {"x": "ok"}))])

    def test_orphan_dsml(self):
        source = '<｜DSML｜parameter name="command" string="true">ls -la</｜DSML｜parameter>'
        self.invariant(
            source,
            [
                Event(
                    "malformed", {"name": None, "body": source, "reason": "orphan_parameter_block"}
                )
            ],
        )

    def test_consecutive_orphans_grouped(self):
        first = '<｜DSML｜parameter name="command" string="true">ls</｜DSML｜parameter>'
        second = '<parameter name="cwd" string="true">/tmp</parameter>'
        group = first + "\n  " + second
        malformed = Event(
            "malformed", {"name": None, "body": group, "reason": "orphan_parameter_block"}
        )
        self.invariant(group, [malformed])
        self.invariant(
            group + '<invoke name="f">{}</invoke>', [malformed, Event("tool", ("f", {}))]
        )
        self.invariant(group + "tail", [malformed, Event("text", "tail")])
        self.invariant("<｜DSML｜tool_calls>" + group + "</｜DSML｜tool_calls>", [malformed])
        parser = ToolParser()
        self.assertEqual(parser.feed(first), [])
        self.assertEqual(parser.feed("\n  "), [])
        self.assertEqual(parser.feed(second), [])
        self.assertEqual(parser.finish(), [malformed])

    def test_orphan_group_truncation_and_bound(self):
        first = '<parameter name="a">1</parameter>'
        for tail in ['<parameter name="b">2', '<parameter name="b"']:
            source = first + "\n" + tail
            self.invariant(
                source,
                [
                    Event(
                        "malformed",
                        {"name": None, "body": source, "reason": "orphan_parameter_unterminated"},
                    )
                ],
            )
        source = (first + "\n") * 20 + '<invoke name="f">{}</invoke>'
        events = parse([source], buffer_limit=80)
        self.assertEqual([e.kind for e in events], ["malformed", "tool"])
        self.assertEqual(events[0].value["reason"], "buffer_limit_exceeded")
        self.assertLessEqual(len(events[0].value["body"]), 80)
        self.invariant(source, events, buffer_limit=80)

    def test_string_aware_close(self):
        args = {"nested": [{"text": 'literal </invoke> and "quoted" \\ tail'}]}
        source = '<invoke name="f">' + json.dumps(args) + "</invoke>"
        self.invariant(source, [Event("tool", ("f", args))])

    def test_json_inside_xml_parameter(self):
        args = {"x": [{"text": '</invoke> </parameter> "quoted"'}]}
        source = (
            '<invoke name="f"><parameter name="x" string="false">'
            + json.dumps(args["x"])
            + "</parameter></invoke>"
        )
        self.invariant(source, [Event("tool", ("f", args))])
        self.invariant(
            '<invoke name="f"><parameter name="x" string="true">{unfinished "quote</parameter></invoke>',
            [Event("tool", ("f", {"x": '{unfinished "quote'}))],
        )

    def test_truncated_block_followed_by_call(self):
        source = '<invoke name="a">{"x":1}<invoke name="b">{}</invoke>'
        self.invariant(source, [Event("tool", ("a", {"x": 1})), Event("tool", ("b", {}))])

    def test_escapes(self):
        self.invariant(
            r'hello \<invoke name="f">bye\\', [Event("text", 'hello <invoke name="f">bye\\\\')]
        )
        self.invariant(
            '<invoke name="f">{"x":"\\</invoke>"}</invoke>',
            [Event("tool", ("f", {"x": "</invoke>"}))],
        )
        self.invariant(
            '<invoke name="f"><parameter name="x" string="true">\\</invoke></parameter></invoke>',
            [Event("tool", ("f", {"x": "</invoke>"}))],
        )
        for count in range(1, 5):
            args = {"x": "\\" * count + "</invoke>"}
            self.invariant(
                '<invoke name="f">' + json.dumps(args) + "</invoke>", [Event("tool", ("f", args))]
            )

    def test_disabled_literal(self):
        source = '\\<invoke name="f">{}</invoke><｜DSML｜tool_calls>\\'
        self.invariant(source, [Event("text", source)], enabled=False)

    def test_spaced_dsml_escapes(self):
        for marker in [
            "</｜｜ DSML ｜ invoke >",
            "<｜ DSML｜ parameter",
            "</｜ DSML｜ parameter >",
            "<｜ DSML ｜ calls>",
        ]:
            self.invariant("literal \\" + marker, [Event("text", "literal " + marker)])
            self.invariant(
                '<invoke name="f">{"x":"\\' + marker + '"}</invoke>',
                [Event("tool", ("f", {"x": marker}))],
            )
            self.invariant(
                '<invoke name="f"><parameter name="x" string="true">\\'
                + marker
                + "</parameter></invoke>",
                [Event("tool", ("f", {"x": marker}))],
            )

    def test_tolerant_stack_and_eof(self):
        for body in ['{"x":[{"a":1', '{"x":[{"a":1}]}]]', '{"x":[{"a":1}}']:
            self.invariant('<invoke name="f">' + body, [Event("tool", ("f", {"x": [{"a": 1}]}))])
        self.invariant('<invoke name="f">{"x":2}</inv', [Event("tool", ("f", {"x": 2}))])
        self.invariant(
            '<invoke name="f">{"x":"line\nnext"}</invoke>',
            [Event("tool", ("f", {"x": "line\nnext"}))],
        )
        expected = {"a": [{"b": [{"c": 1}]}]}
        for body in ['{"a":[{"b":[{"c":1', '{"a":[{"b":[{"c":1}]}', '{"a":[{"b":[{"c":1]}]}']:
            self.invariant('<invoke name="f">' + body, [Event("tool", ("f", expected))])

    def test_no_duplicate_outcomes(self):
        source = '<invoke name="a">{}</invoke><invoke name="b">bad</invoke>'
        result = parse(list(source))
        self.assertEqual([e.kind for e in result], ["tool", "malformed"])
        self.assertEqual(result[1].value["name"], "b")

    def test_mislabeled_salvage(self):
        source = '<parameter name="invoke name="run"><parameter name="parameter name="command" string="true">ls</parameter></parameter>'
        self.invariant(source, [Event("tool", ("run", {"command": "ls"}))])

    def test_truncation(self):
        for source in [
            '<invoke name="f"',
            '<invoke name="f">',
            '<parameter name="x"',
            '<parameter name="x">',
        ]:
            result = parse([source])
            self.assertEqual(len(result), 1)
            self.assertEqual(result[0].kind, "malformed")
            self.invariant(source, result)

    def test_buffer_limit(self):
        source = (
            '<invoke name="f">{"x":"' + "a" * 300 + '"}</invoke>tail<invoke name="g">{}</invoke>'
        )
        result = parse([source], buffer_limit=64)
        self.assertEqual([e.kind for e in result], ["malformed", "text", "tool"])
        self.assertEqual(result[0].value["reason"], "buffer_limit_exceeded")
        self.assertLessEqual(len(result[0].value["body"]), 64)
        self.invariant(source, result, buffer_limit=64)
        parser = ToolParser(buffer_limit=64)
        parser.feed('<invoke name="f">' + "x" * 10000)
        self.assertLessEqual(len(parser._body) + len(parser._header) + len(parser._tag), 64)
        self.assertEqual(parser.finish(), [])
        with self.assertRaises(ValueError):
            ToolParser(buffer_limit=0)

    def test_config_path(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "custom.toml"
            path.write_text('TOOL_FORMAT = "dsml"\n')
            with patch.dict(os.environ, {"DEEPSEEK_CONFIG": str(path)}):
                self.assertEqual(fmt._toml_tool_format(), "dsml")

    def test_dialect_repair_prompts(self):
        for dialect in fmt.DIALECTS.values():
            with patch.object(fmt, "ACTIVE", dialect):
                prompt = fmt.build_repair_prompt([])
                self.assertIn(dialect.template(), prompt)
                self.assertEqual("body is ONE JSON object" in prompt, dialect.body_kind == "json")
                self.assertNotIn(
                    "silently discarded", dialect.instruction + dialect.format_reminder
                )


if __name__ == "__main__":
    unittest.main()
