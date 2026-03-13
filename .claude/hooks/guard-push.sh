#!/bin/bash
# PreToolUse hook: blocks "git push" unless pre-push checks have passed.
# The /pre-push skill creates a .pre-push-passed flag after all checks pass.

INPUT=$(cat)
COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // empty')

# Only care about git push commands
if ! echo "$COMMAND" | grep -qE '\bgit\s+push\b'; then
  exit 0
fi

FLAG_FILE="${CLAUDE_PROJECT_DIR:-.}/.pre-push-passed"

# Check if the flag exists and is less than 10 minutes old
if [ -f "$FLAG_FILE" ]; then
  FLAG_AGE=$(( $(date +%s) - $(stat -c %Y "$FLAG_FILE" 2>/dev/null || echo 0) ))
  if [ "$FLAG_AGE" -lt 600 ]; then
    # Checks passed recently — allow the push and clean up the flag
    rm -f "$FLAG_FILE"
    exit 0
  fi
fi

# Block the push
echo "BLOCKED: Run /pre-push before pushing. Pre-push checks (format, lint, unit tests, integration tests) must all pass first." >&2
exit 2
