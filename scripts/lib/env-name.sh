# shellcheck shell=bash
# The environment name rule, shared by scripts/lib/env.sh and
# scripts/bootstrap.sh (which checks it before it signs in).
#
# The name becomes part of resource names, the stack's name (usnm-<env>)
# and the settings path. ADR-0017 makes it a closed list: prod, dev, or x
# plus 1-4 lowercase letters or digits for a short-lived experiment, at
# most 5 characters, so every resource name stays within its type's limit
# and no environment can take another stack's name (usnm-guardrails,
# usnm-archive). infra/naming.bicep (validEnvName) has the same rule.
ENV_NAME_RE='^(prod|dev|x[a-z0-9]{1,4})$'
valid_env_name() {
  [[ $1 =~ $ENV_NAME_RE ]] || {
    echo "error: the environment name must be prod, dev, or x plus 1-4 lowercase letters or digits for an experiment (ADR-0017), not \"$1\"" >&2
    exit 2
  }
}
