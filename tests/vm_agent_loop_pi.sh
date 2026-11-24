#!/bin/sh
# Run inside the agent image, with a model URL accessible from the Pod.
set -eu
: "${MODEL_URL:?set the OpenAI-compatible base URL, including /v1}"
: "${PI_BIN:=pi}"
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd "$work"
export HOME="$work/home"
export PI_CODING_AGENT_DIR="$HOME/.pi/agent"
mkdir -p "$PI_CODING_AGENT_DIR" workspace
# Use Node (already required by this harness image) to encode the URL as JSON.
node -e 'const fs = require("node:fs");
const provider = {baseUrl: process.env.MODEL_URL, api: "openai-completions",
  apiKey: "local", models: [{id: "nautilus-bench", name: "CPU model",
  reasoning: false, input: ["text"], contextWindow: 8192, maxTokens: 512}]};
fs.writeFileSync(process.env.PI_CODING_AGENT_DIR + "/models.json",
  JSON.stringify({providers: {bench: provider}}));'
cd workspace
cut -c1-8 /proc/sys/kernel/random/uuid > ticket.txt
agent() {
  "$PI_BIN" --offline --no-approve --no-mcp --no-skills --no-extensions \
    --no-context-files --session "$work/session.jsonl" --mode json \
    --tools "$1" --model bench/nautilus-bench --thinking off \
    --system-prompt 'You are a coding assistant with read and write tools. Use the requested tool.' \
    -p "$2" >> "$work/events.jsonl"
}
agent read 'Use read to open ticket.txt and tell me the identifier it contains.'
agent write 'Now use write to save that same identifier in answer.txt. The content must be the exact identifier from the prior read.'
node -e 'const fs = require("node:fs");
const lines = fs.readFileSync(process.argv[1], "utf8").trim().split("\n");
const events = lines.filter(Boolean).map(line => JSON.parse(line));
const tools = events.filter(e => e.type === "tool_execution_end" && !e.isError)
  .map(e => e.toolName);
if (tools.indexOf("read") < 0 || tools.lastIndexOf("write") <= tools.indexOf("read"))
  throw Error("agent did not read then write: " + tools.join(",") + ". " +
    (events.filter(e => e.message?.errorMessage).at(-1)?.message.errorMessage ?? ""));
if (fs.readFileSync("ticket.txt", "utf8").trim() !== fs.readFileSync("answer.txt", "utf8").trim())
  throw Error("agent wrote an incorrect identifier");
console.log(JSON.stringify({tools, verified: true}));' "$work/events.jsonl"
