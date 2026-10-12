"""Tests for scripts/ci/check-names.py (python3 -m unittest scripts/ci/test_check_names.py).
The checks themselves, on hand-made tables; CI also runs the script on the
real ones."""

import importlib.util
import os
import tempfile
import unittest
from pathlib import Path

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("check_names", os.path.join(HERE, "check-names.py"))
cn = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cn)

WORDS = ["api", "core", "data", "ingest", "ocr", "scratch", "state", "qw"]
ABBRS = {
    "ca": {"form": "hyphenated", "maxLength": 31},
    "caj": {"form": "hyphenated", "maxLength": 31},
    "cosno": {"form": "global", "maxLength": 44},
    "st": {"form": "compact", "maxLength": 24},
    "title": {"form": "title", "maxLength": 260},
    "shortName": {"form": "shortName", "maxLength": 12},
    "scopeEnv": {"form": "empty", "maxLength": 0},
    "rg": {"form": "hyphenated", "maxLength": 90},
}


def ok(value, abbr, env="prod", sfx="abc123"):
    return cn.conforms(value, abbr, env, sfx, WORDS, ABBRS)


class Conforms(unittest.TestCase):
    def test_hyphenated(self):
        self.assertIsNone(ok("usnm-ingest-caj-prod", "caj"))
        self.assertIsNone(ok("usnm-qw-cmp-0-ca-xabcd", "ca", env="xabcd"))

    def test_global_and_compact(self):
        self.assertIsNone(ok("usnm-state-cosno-dev-abc123", "cosno", env="dev"))
        self.assertIsNone(ok("usnmscratchstxabcdabc123", "st", env="xabcd"))

    def test_legacy_forms_fail(self):
        self.assertIn("usnm-", ok("caj-usnm-ingest-prod", "caj"))
        self.assertIsNotNone(ok("stusnmdabc123", "st"))
        self.assertIsNotNone(ok("cosmos-usnm-prod-abc123", "cosno"))

    def test_env_last_and_abbr_before_it(self):
        self.assertIn("end with", ok("usnm-ingest-caj-prod-1", "caj"))
        self.assertIn("token before", ok("usnm-ingest-ca-prod", "caj"))

    def test_registry_and_qualifiers(self):
        self.assertIn("registry", ok("usnm-ingestion-caj-prod", "caj"))
        self.assertIn("qualifiers", ok("usnm-api-a-b-c-d-ca-prod", "ca"))
        self.assertIn("registry", ok("usnmstuffstprodabc123", "st"))

    def test_length(self):
        self.assertIn("over the 31", ok("usnm-ingest-very-long-name-caj-prod", "caj"))

    def test_display_forms(self):
        self.assertIsNone(ok("usnm API (dev)", "title", env="dev"))
        self.assertIsNotNone(ok("usnewsmap API", "title"))
        self.assertIsNotNone(ok("usnm API (prod)", "title", env="dev"))
        # A workbook title over its limit fails like any other name.
        self.assertIsNone(ok("usnm " + "A" * 249 + " (dev)", "title", env="dev"))
        self.assertIn("261 characters, over the 260", ok("usnm " + "A" * 250 + " (dev)", "title", env="dev"))
        self.assertIsNone(ok("usnm-xabcd", "shortName", env="xabcd"))
        self.assertIsNotNone(ok("usnm", "shortName"))
        self.assertIsNone(ok("", "scopeEnv"))
        self.assertIsNotNone(ok("/prod", "scopeEnv"))


TABLES = {
    "nameTypes": {"apiApp": "ca", "rg": "rg", "nic": "rg", "tiles": "st"},
    "functionWords": WORDS,
    "typeAbbreviations": ABBRS,
    "createOnlyKeys": ["nic"],
}


class CheckNames(unittest.TestCase):
    def test_conventional_map(self):
        names = {"apiApp": "usnm-api-ca-dev", "rg": "usnm-core-rg-dev", "nic": "usnm-core-x-rg-dev",
                 "tiles": "stusnmtabc123"}
        self.assertEqual(cn.check_names(names, "dev", "abc123", False, TABLES,
                                        {"tiles": {"scope": "all"}}), [])
        # A prod-only exception doesn't cover the conventional form.
        out = cn.check_names(names, "dev", "abc123", False, TABLES, {"tiles": {}})
        self.assertEqual(len(out), 1)
        self.assertIn("tiles", out[0])

    def test_pinned_needs_exceptions_and_drops_stale_ones(self):
        names = {"apiApp": "ca-usnm-prod", "rg": "usnm-core-rg-prod", "nic": "", "tiles": "stusnmtffapzy"}
        out = cn.check_names(names, "prod", "ffapzy", True, TABLES, {"tiles": {}})
        self.assertEqual(len(out), 1)
        self.assertIn("apiApp", out[0])
        out = cn.check_names(names, "prod", "ffapzy", True, TABLES,
                             {"apiApp": {}, "rg": {}, "tiles": {}})
        self.assertEqual(len(out), 1)
        self.assertIn("remove its exception", out[0])

    def test_keys_match_the_types(self):
        out = cn.check_names({"apiApp": "usnm-api-ca-dev"}, "dev", "abc123", False, TABLES, {})
        self.assertIn("keys and nameTypes differ", out[0])


class Prod(unittest.TestCase):
    def test_a_rename_needs_prod_json(self):
        self.assertEqual(cn.check_prod({"rg": "rg-usnm-prod"}, {"$comment": "x", "rg": "rg-usnm-prod"}), [])
        out = cn.check_prod({"rg": "usnm-core-rg-prod"}, {"rg": "rg-usnm-prod"})
        self.assertIn("prod.json", out[0])


class Exceptions(unittest.TestCase):
    GOOD = {"reason": "r", "adr": "ADR-0017, Exceptions", "until": "phase-2"}

    def test_fields_and_order(self):
        self.assertEqual(cn.check_exceptions({"a": self.GOOD, "b": {**self.GOOD, "until": "#309"}}, None), [])
        out = cn.check_exceptions({"b": self.GOOD, "a": {"until": "later"}}, None)
        self.assertTrue(any("sorted" in o for o in out))
        self.assertTrue(any("no reason" in o for o in out))
        self.assertTrue(any("isn't phase" in o for o in out))

    def test_only_shrinks(self):
        out = cn.check_exceptions({"a": self.GOOD, "b": self.GOOD}, {"a": self.GOOD})
        self.assertEqual(len(out), 1)
        self.assertIn("exception b is new", out[0])

    def test_a_wider_scope_is_new(self):
        # prod (the default) to all excuses the conventional form too.
        out = cn.check_exceptions({"a": {**self.GOOD, "scope": "all"}}, {"a": self.GOOD})
        self.assertEqual(len(out), 1)
        self.assertIn("exception a widened from scope prod to all", out[0])
        out = cn.check_exceptions({"a": {**self.GOOD, "scope": "all"}}, {"a": {**self.GOOD, "scope": "prod"}})
        self.assertEqual(len(out), 1)
        # An unchanged or narrower scope passes.
        self.assertEqual(cn.check_exceptions({"a": {**self.GOOD, "scope": "all"}},
                                             {"a": {**self.GOOD, "scope": "all"}}), [])
        self.assertEqual(cn.check_exceptions({"a": self.GOOD}, {"a": {**self.GOOD, "scope": "all"}}), [])


class Sources(unittest.TestCase):
    def check(self, text):
        with tempfile.TemporaryDirectory(dir=cn.ROOT) as d:
            p = Path(d) / "m.bicep"
            p.write_text(text)
            return cn.check_sources([p])

    def test_a_name_built_in_a_module(self):
        out = self.check("resource x 'T' = {\n  name: 'caj-usnm-ingest-${env}'\n}\n")
        self.assertEqual(len(out), 1)
        self.assertIn("names()", out[0])
        self.assertEqual(len(self.check("  displayName: 'usnewsmap API'\n")), 1)
        self.assertEqual(len(self.check("    groupShortName: 'usnm'\n")), 1)
        out = self.check("  { name: 'pe-usnm-blob', target: storageId, description: 'x' }\n")
        self.assertEqual(len(out), 1)
        self.assertEqual(self.check("  { name: e.name, description: 'usnm-api answered' }\n"), [])

    def test_parameters_children_and_comments_pass(self):
        self.assertEqual(self.check("  name: name\n  name: 'usnm'\n  // name: 'usnm-x'\n"
                                    "  { name: 'USNM_BACKEND', value: 'x' }\n"), [])

    def test_guid_seeds(self):
        self.assertEqual(self.check("  name: guid(resourceGroup().id, 'usnm-swa-deployer')\n"), [])
        out = self.check("  name: guid(resourceGroup().id, 'usnm-new-seed')\n")
        self.assertIn("frozen seed", out[0])

    def test_a_name_split_over_lines(self):
        # The value on the next line, a ternary over three lines, and a
        # concatenation continued on the next line: each still counts, at
        # the property's own line.
        out = self.check("resource x 'T' = {\n  name:\n    'caj-usnm-ingest-${env}'\n}\n")
        self.assertEqual(len(out), 1)
        self.assertTrue(out[0].endswith("names() in infra/naming.bicep (ADR-0017)"))
        self.assertIn("m.bicep:2:", out[0])
        out = self.check("resource x 'T' = {\n  name: pinned\n    ? 'ca-usnm-${env}'\n    : n.apiApp\n"
                         "  location: location\n}\n")
        self.assertEqual(len(out), 1)
        self.assertIn("m.bicep:2:", out[0])
        out = self.check("resource x 'T' = {\n  displayName: concat(\n    'usnewsmap ',\n    env)\n}\n")
        self.assertEqual(len(out), 1)
        # The next property isn't read as part of the value.
        self.assertEqual(self.check("resource x 'T' = {\n  name: n.apiApp\n  tags: { a: 'usnm-x' }\n}\n"), [])

    def test_a_guid_call_split_over_lines(self):
        out = self.check("resource r 'T' = {\n  name: guid(\n    resourceGroup().id,\n"
                         "    'usnm-new-seed'\n  )\n  properties: {}\n}\n")
        self.assertEqual(len(out), 1)
        self.assertIn("m.bicep:4: guid() seed 'usnm-new-seed'", out[0])
        self.assertEqual(self.check("resource r 'T' = {\n  name: guid(\n    resourceGroup().id,\n"
                                    "    'usnm-swa-deployer'\n  )\n  properties: { scope: '/dbs/usnm' }\n}\n"),
                         [])

    def test_comments_are_skipped_but_strings_kept(self):
        self.assertEqual(self.check("  name: n.x // was 'caj-usnm-x'\n  /* name: 'ca-usnm-y'\n"
                                    "  name: 'ca-usnm-z' */\n"), [])
        out = self.check("  name: 'https://ca-usnm-x' // a comment\n")
        self.assertEqual(len(out), 1)


class Indirection(unittest.TestCase):
    """A name taken from a var, a parameter default or a func of the same file."""

    check = Sources.check

    def test_a_var(self):
        out = self.check("var localName = 'usnm-new-ca-dev'\n"
                         "resource x 'T' = {\n  name: localName\n}\n")
        self.assertEqual(len(out), 1)
        self.assertIn("m.bicep:3: name built from 'usnm-new-ca-dev' (through localName (line 1))", out[0])

    def test_a_nested_var(self):
        out = self.check("var inner = 'usnm-new'\nvar outer = inner\nvar localName = outer\n"
                         "resource x 'T' = {\n  name: localName\n}\n")
        self.assertEqual(len(out), 1)
        self.assertIn("'usnm-new' (through localName (line 3) then outer (line 2) then inner (line 1))",
                      out[0])

    def test_a_var_built_by_interpolation(self):
        out = self.check("var prefix = 'usnm-new'\nvar localName = '${prefix}-ca-${env}'\n"
                         "resource x 'T' = {\n  name: '${localName}-x'\n}\n")
        self.assertEqual(len(out), 1)
        self.assertIn("'usnm-new' (through localName (line 2) then prefix (line 1))", out[0])
        # Interpolated straight into the name, as a ternary's branch, and
        # as a function's argument.
        self.assertEqual(len(self.check("var p = 'usnm'\nresource x 'T' = {\n  name: '${p}-ca-dev'\n}\n")), 1)
        self.assertEqual(len(self.check("var a = 'usnm-a'\nresource x 'T' = {\n  name: pinned\n"
                                        "    ? a\n    : n.apiApp\n}\n")), 1)
        self.assertEqual(len(self.check("var a = 'usnm-a'\nresource x 'T' = {\n  name: toLower(a)\n}\n")), 1)

    def test_a_parameter_default_and_a_func(self):
        out = self.check("param appName string = 'usnm-new-ca-dev'\n"
                         "resource x 'T' = {\n  name: appName\n}\n")
        self.assertEqual(len(out), 1)
        out = self.check("func appName(env string) string => 'usnm-new-ca-${env}'\n"
                         "resource x 'T' = {\n  name: appName(env)\n}\n")
        self.assertEqual(len(out), 1)
        self.assertIn("through appName", out[0])

    def test_names_and_parameters_pass(self):
        # From names(), a parameter without a default, another value's
        # property, and a var whose name is used only as a property.
        self.assertEqual(self.check(
            "import { names } from '../naming.bicep'\nparam appName string\n"
            "var n = names(env, sfx, pinned)\nvar localName = n.apiApp\nvar tags = { app: 'usnm-x' }\n"
            "var id = 'usnm-x'\n"
            "resource x 'T' = {\n  name: localName\n}\nresource y 'T' = {\n  name: appName\n}\n"
            "resource z 'T' = {\n  name: other.id\n  tags: tags\n}\n"
            "resource w 'T' = {\n  name: n.cosmos\n  properties: { id: id }\n}\n"), [])
        # The Cosmos database, named exactly usnm, through a var too; but
        # not usnm as a part of a name.
        self.assertEqual(self.check("var db = 'usnm'\nresource d 'T' = {\n  name: db\n}\n"), [])
        self.assertEqual(len(self.check("resource d 'T' = {\n  name: concat('usnm', '-x')\n}\n")), 1)
        # A cycle doesn't loop.
        self.assertEqual(self.check("var a = b\nvar b = a\nresource x 'T' = {\n  name: a\n}\n"), [])


def rust_lists(types, compact):
    """Sanitizer source with the two lists, the first over several lines."""
    def items(xs):
        return ", ".join(f'"{x}"' for x in xs)
    return (f"const TYPE_ABBREVIATIONS: [&str; {len(types)}] = [\n    {items(types)},\n];\n"
            f"const COMPACT_ABBREVIATIONS: [&str; {len(compact)}] = [{items(compact)}];\n")


class Sanitizer(unittest.TestCase):
    TABLE = {**{k: dict(v) for k, v in ABBRS.items()},
             "role": {"form": "hyphenated", "maxLength": 512, "display": True}}
    for k in ("title", "shortName", "scopeEnv"):
        TABLE[k]["display"] = True

    def test_match(self):
        text = rust_lists(["ca", "caj", "cosno", "st", "rg"], ["st"])
        self.assertEqual(cn.check_sanitizer(self.TABLE, text), [])

    def test_mismatch(self):
        out = cn.check_sanitizer(self.TABLE, rust_lists(["ca", "caj", "cosno", "st", "title"], ["st", "ca"]))
        self.assertEqual(len(out), 2)
        self.assertIn("TYPE_ABBREVIATIONS differs", out[0])
        self.assertIn("missing ['rg'], extra ['title']", out[0])
        self.assertIn("COMPACT_ABBREVIATIONS differs", out[1])
        out = cn.check_sanitizer(self.TABLE, rust_lists(["ca", "caj", "cosno", "st", "rg", "rg"], ["st"]))
        self.assertEqual(len(out), 1)
        self.assertIn("twice", out[0])
        # An item in a comment doesn't count.
        text = rust_lists(["ca", "caj", "cosno", "st"], ["st"]).replace("\n];", '\n    // "rg",\n];')
        out = cn.check_sanitizer(self.TABLE, text)
        self.assertEqual(len(out), 1)
        self.assertIn("missing ['rg']", out[0])
        out = cn.check_sanitizer(self.TABLE, "const TYPE_ABBREVIATIONS: [&str; 0] = [];\n")
        self.assertTrue(any("no `const COMPACT_ABBREVIATIONS" in o for o in out))

    def test_the_real_file_parses(self):
        text = cn.SANITIZER.read_text(encoding="utf-8")
        self.assertIn("caj", cn.rust_list(text, "TYPE_ABBREVIATIONS"))
        self.assertEqual(sorted(cn.rust_list(text, "COMPACT_ABBREVIATIONS")), ["cr", "kv", "st"])


class Base(unittest.TestCase):
    """--base and --require-base: a base that can't be read is a problem."""

    def test_no_base(self):
        self.assertEqual(cn.base_exceptions("", False), (None, []))
        base, out = cn.base_exceptions("", True)
        self.assertIsNone(base)
        self.assertIn("--require-base", out[0])

    def test_a_revision_that_doesnt_resolve(self):
        base, out = cn.base_exceptions("refs/heads/no-such-branch-309", False)
        self.assertIsNone(base)
        self.assertIn("isn't a revision here", out[0])

    def test_the_current_revision(self):
        # HEAD has both files (in a checkout with history at all).
        if cn.git("cat-file", "-e", "HEAD:infra/names/exceptions.json").returncode != 0:
            self.skipTest("no committed exceptions.json at HEAD")
        base, out = cn.base_exceptions("HEAD", True)
        self.assertEqual(out, [])
        self.assertIsInstance(base, dict)


if __name__ == "__main__":
    unittest.main()
