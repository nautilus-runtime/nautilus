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
    -p "$2" > "$work/$1.jsonl"
}
agent read 'Use read to open ticket.txt and tell me the identifier it contains.'
agent write 'Now use write to save that same identifier in answer.txt. The content must be the exact identifier from the prior read.'
node -e 'const fs = require("node:fs");
const tools = [];
for (const [i, file] of process.argv.slice(1).entries()) {
  const events = fs.readFileSync(file, "utf8").trim().split("\n").map(line => JSON.parse(line));
  const assistant = events.filter(e => e.type === "message_end" && e.message?.role === "assistant")
    .map(e => e.message);
  const ends = events.filter(e => e.type === "tool_execution_end");
  const wanted = ["read", "write"][i];
  if (!assistant.length || assistant.at(-1).stopReason !== "stop" ||
      assistant.some(m => m.errorMessage || ["aborted", "error"].includes(m.stopReason)) ||
      ends.some(e => e.isError) || !ends.some(e => e.toolName === wanted))
    throw Error("agent did not successfully complete the " + wanted + " turn");
  tools.push(...ends.map(e => e.toolName));
}
if (fs.readFileSync("ticket.txt", "utf8").trim() !== fs.readFileSync("answer.txt", "utf8").trim())
  throw Error("agent wrote an incorrect identifier");
console.log(JSON.stringify({tools, verified: true}));' "$work/read.jsonl" "$work/write.jsonl"
