"""scripts/ci/no-positional-args.py: scripts read named arguments, not positional ones (#277)."""

import importlib.util
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("check", HERE / "no-positional-args.py")
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)


def sh(body: str) -> list[int]:
    return check.findings("x.sh", "#!/usr/bin/env bash\n" + body)


def py(body: str) -> list[int]:
    return check.findings("x.py", body)


class Shell(unittest.TestCase):
    def test_a_top_level_read_is_found(self):
        self.assertEqual(sh('ENV_NAME=$1\nSPAN=${3:-1d}\n'), [2, 3])
        self.assertEqual(sh('echo "using $2"\n'), [2])

    def test_the_parsing_loop_is_allowed(self):
        self.assertEqual(sh('while [ $# -gt 0 ]; do\n  case $1 in\n    --env) ENV=$2; shift 2 ;;\n'
                            '    *) echo "unknown $1" >&2; exit 2 ;;\n  esac\ndone\n'), [])
        self.assertEqual(sh('while [[ $# -gt 0 ]]; do\n  x=$1; shift\ndone\ny=$1\n'), [5])

    def test_a_function_reads_its_own_arguments(self):
        self.assertEqual(sh('die() { echo "error: $1" >&2; exit 1; }\n'), [])
        self.assertEqual(sh('start() {\n  local a=$1\n  if true; then\n    echo "$2"\n  fi\n}\nx=$1\n'), [8])
        self.assertEqual(sh('sub() (\n  ENV_NAME=$1\n)\n'), [])
        self.assertEqual(sh('function f {\n  :\n}\n'), [])

    def test_comments_single_quotes_and_quoted_heredocs_are_not_reads(self):
        self.assertEqual(sh('# The image ($1) runs\n'), [])
        self.assertEqual(sh("awk '{ print $1 }' f\n"), [])
        self.assertEqual(sh("python3 -c '\nimport sys\nprint($1)\n'\n"), [])
        self.assertEqual(sh('cat <<\'EOF\'\n$1\nEOF\n'), [])
        self.assertEqual(sh('cat <<EOF\n$1\nEOF\n'), [3])
        self.assertEqual(sh('echo "\\$1 is literal"\n'), [])
        self.assertEqual(sh('n=${#files[@]}\necho "$@"\n[ $# -eq 0 ]\n'), [])

    def test_a_heredoc_in_a_function_is_the_function_s(self):
        self.assertEqual(sh('conf() {\n  cat <<YAML\nnode: $1\nYAML\n}\n'), [])


class Python(unittest.TestCase):
    def test_argv_is_found(self):
        self.assertEqual(py("import sys\nx = sys.argv[1]\ny = sys.argv[1:]\n"), [2, 3])

    def test_a_positional_argparse_argument_is_found(self):
        self.assertEqual(py('ap.add_argument("env")\nap.add_argument("--env")\nap.add_argument("-v", "--verbose")\n'),
                         [1])

    def test_subcommands_are_allowed(self):
        self.assertEqual(py('ap.add_argument("command", choices=["run", "audit"])\nap.add_subparsers(dest="cmd")\n'),
                         [])

    def test_argv_handed_to_a_mock_is_not_a_read(self):
        self.assertEqual(py('with mock.patch("sys.argv", ["x", "--env", "dev"]):\n    pass\n'), [])


class Rust(unittest.TestCase):
    def test_any_read_of_the_arguments_is_found(self):
        # A CLI uses clap; a hand-written parser is reviewed into HAND_PARSED.
        self.assertEqual(check.findings("x.rs", "let p = std::env::args().nth(1);\n// env::args().nth(1)\n"), [1])
        self.assertEqual(check.findings("x.rs", "let mut a = std::env::args().skip(1).peekable();\n"), [1])
        self.assertEqual(check.findings("x.rs", "for a in std::env::args_os() {}\n"), [1])

    def test_a_clap_field_without_long_is_positional(self):
        src = """#[derive(Parser)]
struct Cli {
    /// The store (https://...).
    #[arg(long, env = "X")]
    store: String,
    #[arg(
        long = "version",
        value_name = "V"
    )]
    version: Option<String>,
    #[command(flatten)]
    stores: Stores,
    #[command(subcommand)]
    command: Command,
    path: PathBuf,
}

#[derive(Subcommand)]
enum Command {
    Run {
        #[arg(short, long)]
        max: Option<usize>,
        batch: String,
    },
    Plain,
}

fn main() {
    let x = Run { batch: String::new(), max: None };
}
"""
        self.assertEqual(check.findings("x.rs", src), [15, 23])


class Allowed(unittest.TestCase):
    def test_counts_must_match(self):
        files = {"a.sh": "#!/usr/bin/env bash\nx=$1\n", "b.sh": "#!/usr/bin/env bash\n"}
        self.assertEqual(check.check(files, {"a.sh": 1}), [])
        self.assertIn("reads positional arguments", check.check(files, {})[0])
        self.assertIn("lower its entry", check.check(files, {"a.sh": 1, "b.sh": 1})[0])
        self.assertIn("not tracked", check.check(files, {"a.sh": 1, "c.sh": 1})[0])

    def test_the_repository_passes(self):
        self.assertEqual(check.main(), 0)


if __name__ == "__main__":
    unittest.main()
