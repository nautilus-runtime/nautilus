"""CPU coding-agent workload for the Pod VM benchmark (OpenAI-compatible chat API)."""
import json
import secrets
import time
import urllib.request


# This small application renders a list using the collation selected in its
# config file. Its acceptance test compares the rendered file with a fixture.
SETUP = """set -eu
dir=$(mktemp -d /tmp/nautilus-agent.XXXXXX)
cd "$dir"
printf '%s\n' kiwi pear apple pear > input
printf '%s\n' raw > mode
cat > render <<'SH'
case "$(cat mode)" in
  ascending) sort input ;;
  descending) sort -r input ;;
  unique) sort -u input ;;
  raw) cat input ;;
  *) exit 2 ;;
esac
SH
case "$1" in
  ascending) sort input ;;
  descending) sort -r input ;;
  unique) sort -u input ;;
esac > expected
printf '%s' "$dir"
"""

TEST = """set -eu
cd "$1"
sh render > actual
if cmp -s actual expected; then
  echo PASS
else
  echo FAIL
  echo Expected; cat expected
  echo Actual; cat actual
fi
"""

MODES = ("ascending", "descending", "unique")
SYSTEM = """Repair the renderer in the workspace. Its output must match the expected
fixture including order and duplicates. You have four tools. test runs the
acceptance test and prints expected and actual output on failure. ascending,
descending, and unique edit the config to select ascending sort, descending
sort, or ascending sort with duplicates removed, respectively. Inspect the
test output before choosing a repair. After editing, run test to verify.
Respond with a JSON object containing only the tool name in the tool field."""

def select_tool(messages, model_url, model, tools):
    schema = {"type": "object", "properties": {"tool": {"type": "string", "enum": tools}},
              "required": ["tool"], "additionalProperties": False}
    payload = {"model": model, "messages": messages, "temperature": 0, "max_tokens": 32,
               "response_format": {"type": "json_schema",
                                   "json_schema": {"name": "action", "strict": True, "schema": schema}}}
    request = urllib.request.Request(model_url.rstrip("/") + "/chat/completions",
                                     data=json.dumps(payload).encode(),
                                     headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=120) as response:
        content = json.load(response)["choices"][0]["message"]["content"]
    tool = json.loads(content)["tool"]
    if tool not in tools:
        raise ValueError(f"unknown agent tool {tool!r}")
    return tool, content


def run_agent(invoke, model_url, model):
    """Run one session. invoke(argv) executes in this container and returns text.

    first_tool_at is a monotonic clock reading after the first model-selected
    tool returns. tool_ms excludes model and workspace preparation time.
    """
    target = secrets.choice(MODES)
    workspace = invoke(["/bin/sh", "-c", SETUP, "setup", target]).strip()
    if not workspace.startswith("/tmp/nautilus-agent.") or "/" in workspace[5:]:
        raise RuntimeError("invalid guest workspace")
    messages = [{"role": "system", "content": SYSTEM},
                {"role": "user", "content": "The renderer test fails. Diagnose, fix and verify it."}]
    tool_ms = []
    actions = []
    inference_ms = []
    first_tool_at = None
    remaining = list(MODES)
    available_tools = ["test"]
    edited = False
    for _ in range(8):
        start = time.monotonic()
        tool, content = select_tool(messages, model_url, model, available_tools)
        before = time.monotonic()
        inference_ms.append((before - start) * 1000)
        if tool == "test":
            output = invoke(["/bin/sh", "-c", TEST, "test", workspace]).strip()
        else:
            output = invoke(["/bin/sh", "-c", 'cd "$1" && echo "$2" > mode && cat mode',
                             "edit", workspace, tool]).strip()
            remaining.remove(tool)
            available_tools = ["test"]
            edited = True
        after = time.monotonic()
        if first_tool_at is None:
            first_tool_at = after
        tool_ms.append((after - before) * 1000)
        actions.append(tool)
        if tool == "test" and output == "PASS" and edited:
            repairs = [action for action in actions if action != "test"]
            return dict(first_tool_at=first_tool_at, tool_ms=tool_ms, actions=actions,
                        target=target, first_repair_correct=repairs[0] == target,
                        inference_ms=inference_ms)
        if tool == "test" and output.startswith("FAIL"):
            if not remaining:
                raise RuntimeError("no repair passed the acceptance test")
            available_tools = remaining
        messages.extend([{"role": "assistant", "content": content},
                         {"role": "user", "content": f"Tool {tool} returned:\n{output}\nAvailable tools: {available_tools}"}])
    raise RuntimeError(f"agent exhausted its tool budget: {actions}")
