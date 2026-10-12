#!/usr/bin/env python3
"""An environment's committed settings, infra/env/<env>.json (#295).

Each file holds one environment's public choices (flags, sizes, caps,
schedules), keys sorted; infra/env/schema.json lists every setting
infra/main.bicepparam reads and says where each lives (x-usnm-visibility):
public in the committed file, private in the settings store under the
environment's label, local in the local settings file the scripts write.
scripts/lib/env.sh reads the files through this script; CI runs --check.

    scripts/lib/env_settings.py --check
    scripts/lib/env_settings.py --env prod --shell
    scripts/lib/env_settings.py --env prod --set USNM_JA_LATIN --value true
    scripts/lib/env_settings.py --env prod --unset USNM_JA_LATIN

--check validates the schema against infra/main.bicepparam (the same
settings, the same types) and every infra/env/<env>.json against the schema:
known public keys only, typed values, keys sorted, in the canonical layout
(json.dumps with two-space indents and a final newline). --shell prints
bash assignments for scripts/lib/env.sh to eval: the public, private and
local key lists, and the committed settings as KEY="value" lines. --set and
--unset edit the committed file; an empty --value unsets. USNM_ENV_DIR
replaces infra/env (the tests' own directory). Standard library only.
"""

import argparse
import json
import os
import re
import shlex
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
VISIBILITIES = ("public", "private", "local")
TYPES = {"boolean": bool, "integer": int, "string": str}
# The schema's own key, for editors.
SCHEMA_KEY = "$schema"
# ADR-0017's closed list, as scripts/lib/env-name.sh (ENV_NAME_RE) and
# infra/naming.bicep (validEnvName) have it: prod, dev, or x plus 1-4
# lowercase letters or digits (fullmatch, so a trailing newline fails too).
ENV_NAME = re.compile(r"prod|dev|x[a-z0-9]{1,4}")
ENV_NAME_RULE = "prod, dev, or x plus 1-4 lowercase letters or digits (ADR-0017)"


class Invalid(Exception):
    pass


def env_dir():
    return Path(os.environ.get("USNM_ENV_DIR") or ROOT / "infra" / "env")


def load_schema(directory):
    path = directory / "schema.json"
    try:
        schema = json.loads(path.read_text())
    except (OSError, ValueError) as e:
        raise Invalid(f"{path}: {e}")
    props = schema.get("properties")
    if not isinstance(props, dict) or schema.get("additionalProperties") is not False:
        raise Invalid(f"{path}: needs properties and additionalProperties: false")
    kinds = {}
    for key, spec in props.items():
        if key == SCHEMA_KEY:
            continue
        vis = spec.get("x-usnm-visibility")
        if vis not in VISIBILITIES:
            raise Invalid(f"{path}: {key}: x-usnm-visibility must be one of {', '.join(VISIBILITIES)}")
        if vis == "public" and spec.get("type") not in TYPES:
            raise Invalid(f"{path}: {key}: a public setting needs a type ({', '.join(TYPES)})")
        if vis != "public" and spec.get("not") != {}:
            # A generic JSON Schema validator then refuses it too.
            raise Invalid(f'{path}: {key}: a {vis} setting needs "not": {{}}')
        kinds[key] = spec
    return kinds


def render(value):
    """A value as the environment variable infra/main.bicepparam reads."""
    if isinstance(value, bool):
        return "true" if value else "false"
    return str(value)


def canonical(data):
    return json.dumps(data, indent=2, ensure_ascii=False) + "\n"


def check_value(kinds, key, value, where):
    spec = kinds.get(key)
    if spec is None:
        raise Invalid(f"{where}: {key} isn't a setting (infra/env/schema.json)")
    vis = spec["x-usnm-visibility"]
    if vis == "private":
        raise Invalid(f"{where}: {key} is private: it goes in the settings store, under the environment's label, "
                      "never in the repository (docs/operations.md, \"Settings\")")
    if vis == "local":
        raise Invalid(f"{where}: {key} is local: the scripts keep it in the local settings file")
    want = TYPES[spec["type"]]
    # bool is an int in Python: a boolean isn't an integer here.
    if type(value) is not want:
        raise Invalid(f"{where}: {key} must be a {spec['type']}, not {json.dumps(value)}")
    if isinstance(value, str) and ("\n" in value or value == ""):
        raise Invalid(f"{where}: {key}: a value is one non-empty line (leave a setting out for its default)")


def load_env(directory, kinds, path):
    try:
        text = path.read_text()
        data = json.loads(text)
    except (OSError, ValueError) as e:
        raise Invalid(f"{path}: {e}")
    if not isinstance(data, dict):
        raise Invalid(f"{path}: must be an object")
    for key, value in data.items():
        if key == SCHEMA_KEY:
            if value != "./schema.json":
                raise Invalid(f'{path}: "$schema" must be "./schema.json"')
            continue
        check_value(kinds, key, value, path)
    if list(data) != sorted(data):
        raise Invalid(f"{path}: keys must be sorted")
    if text != canonical(data):
        raise Invalid(f"{path}: not in the canonical format (two-space indents, a final newline)")
    return data


def env_file(directory, env):
    if not ENV_NAME.fullmatch(env):
        raise Invalid(f"invalid environment name: {env}; it must be {ENV_NAME_RULE}")
    return directory / f"{env}.json"


def bicepparam_keys():
    """{key: type} for every setting infra/main.bicepparam reads."""
    text = (ROOT / "infra" / "main.bicepparam").read_text()
    keys = {}
    for wrap, key in re.findall(r"(bool\(|int\()?readEnvironmentVariable\('([A-Za-z0-9_]+)'", text):
        keys[key] = {"bool(": "boolean", "int(": "integer", "": "string"}[wrap]
    return keys


def check(directory):
    errors = []
    try:
        kinds = load_schema(directory)
    except Invalid as e:
        return [str(e)]
    params = bicepparam_keys()
    for key in sorted(set(params) - set(kinds)):
        errors.append(f"infra/main.bicepparam reads {key}, which infra/env/schema.json doesn't list")
    for key in sorted(set(kinds) - set(params)):
        errors.append(f"infra/env/schema.json lists {key}, which infra/main.bicepparam doesn't read")
    for key in sorted(set(kinds) & set(params)):
        spec = kinds[key]
        if spec["x-usnm-visibility"] == "public" and spec["type"] != params[key]:
            errors.append(f"infra/env/schema.json: {key} is a {spec['type']}; infra/main.bicepparam reads a {params[key]}")
    files = sorted(p for p in directory.glob("*.json") if p.name != "schema.json")
    for path in files:
        if not ENV_NAME.fullmatch(path.stem):
            errors.append(f"{path}: not an environment name: {ENV_NAME_RULE}")
            continue
        try:
            load_env(directory, kinds, path)
        except Invalid as e:
            errors.append(str(e))
    return errors


def shell(directory, env):
    kinds = load_schema(directory)
    path = env_file(directory, env)
    data = load_env(directory, kinds, path) if path.exists() else {}
    lines = []
    for key, value in data.items():
        if key == SCHEMA_KEY:
            continue
        v = render(value)
        for c in ("\\", '"', "$", "`"):
            v = v.replace(c, "\\" + c)
        lines.append(f'{key}="{v}"')
    out = {}
    for vis in VISIBILITIES:
        out[f"SETTINGS_{vis.upper()}"] = " ".join(k for k, s in kinds.items() if s["x-usnm-visibility"] == vis)
    out["COMMITTED_SETTINGS"] = "\n".join(lines)
    return "".join(f"{k}={shlex.quote(v)}\n" for k, v in out.items())


def parse(spec, raw, key):
    """A command-line value as the schema's type."""
    t = spec["type"]
    if t == "boolean":
        if raw not in ("true", "false"):
            raise Invalid(f"{key} is true or false, not {raw!r}")
        return raw == "true"
    if t == "integer":
        if not re.fullmatch(r"-?[0-9]+", raw):
            raise Invalid(f"{key} is a whole number, not {raw!r}")
        return int(raw, 10)
    return raw


def edit(directory, env, key, raw):
    """Sets key to raw, or removes it when raw is None or empty."""
    kinds = load_schema(directory)
    path = env_file(directory, env)
    data = load_env(directory, kinds, path) if path.exists() else {SCHEMA_KEY: "./schema.json"}
    if raw is None or raw == "":
        if key not in kinds:
            raise Invalid(f"{key} isn't a setting (infra/env/schema.json)")
        data.pop(key, None)
    else:
        spec = kinds.get(key)
        value = parse(spec, raw, key) if spec and spec["x-usnm-visibility"] == "public" else raw
        check_value(kinds, key, value, path)
        data[key] = value
    data = dict(sorted(data.items()))
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(canonical(data))
    tmp.replace(path)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--check", action="store_true", help="check the schema and every committed file")
    ap.add_argument("--env", help="the environment")
    ap.add_argument("--shell", action="store_true", help="print the settings as bash assignments")
    ap.add_argument("--set", metavar="KEY", help="set KEY in the committed file")
    ap.add_argument("--value", help="--set's value (empty unsets)")
    ap.add_argument("--unset", metavar="KEY", help="remove KEY from the committed file")
    args = ap.parse_args()
    directory = env_dir()
    try:
        if args.check:
            errors = check(directory)
            for e in errors:
                print(f"error: {e}", file=sys.stderr)
            if errors:
                return 1
            print(f"{directory}: the schema and every environment's settings check out")
            return 0
        if not args.env:
            ap.error("--env is required (or --check)")
        if args.shell:
            sys.stdout.write(shell(directory, args.env))
        elif args.set:
            if args.value is None:
                ap.error("--set needs --value")
            edit(directory, args.env, args.set, args.value)
        elif args.unset:
            edit(directory, args.env, args.unset, None)
        else:
            ap.error("one of --shell, --set or --unset")
    except Invalid as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
