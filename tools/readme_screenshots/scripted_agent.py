#!/usr/bin/env python3
"""The scripted agent behind the README screenshots, a filter on the stdio of the fake ACP agent
(crates/branchyard-runtime/src/bin/fake-acp-agent.rs).

    scripted_agent.py SCRIPT.json -- AGENT [ARG...]

SCRIPT.json lists scenes, each keyed by a prompt as the person typed it. For a `session/prompt`
whose text contains a scene's prompt (after whatever instructions Branchyard put around it), the
filter writes the scene's files into its working directory, which is the branch's worktree or the
session's copy. It then reports the scene's text and tool calls as `session/update`
notifications and sends the agent one directive in place of the prompt. `REPLY_FILE <reply>`
makes the fake agent answer with the reply's text and do nothing else, which is why the filter
does the file work itself. A scene without a reply sends `HANG`, so its turn runs until someone
cancels it. A scene with `asks` sends `PERMISSION WRITE <asks>` instead: the fake agent asks the
server for permission to write and the turn waits, parked on that approval, until a person
answers. Every other message passes through untouched, so the server records the prompt as typed
and the fake agent answers the rest of the protocol. The filter exits with the agent.

    scripted_agent.py SCRIPT.json --stream-json [ARG...]

plays the same scenes as Claude Code's stream-json protocol, with no agent behind it, for the
branches, whose harness is the `claude-code` profile: a harness that reports what it spends. It
answers the control requests (`initialize`, `interrupt`, `end_session`), and for each user
message whose text contains a scene's prompt it writes the scene's files, then sends the scene's
text and each tool call as `assistant` frames, `step` seconds apart (SCRIPT.json's `step`, default
0), each carrying the usage of one model call, which the engine prices as the turn runs. A scene
with a reply ends with it and a `result` carrying the scene's `cost` as the session's
`total_cost_usd`; one that asks sends a `can_use_tool` request for its write and waits for the
answer (an allow writes the file, then the turn ends as one with a reply); one with neither runs
until it is interrupted. The ARGs, the flags the driver passes, are ignored.

A scene, in SCRIPT.json's `scenes` list:

    {"prompt": "...", "say": "text before the tool calls",
     "tools": [{"kind": "read", "title": "Read src/record.rs"}],
     "files": {"src/record.rs": "..."}, "reply": "/abs/reply.txt", "asks": "NOTES.md=draft",
     "cost": 0.42}
"""

from __future__ import annotations

import contextlib
import json
import os
import subprocess
import sys
import threading
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, BinaryIO

TOOL_KINDS = {"read", "edit", "delete", "move", "search", "execute", "think", "fetch", "other"}


@dataclass(frozen=True)
class Tool:
    kind: str
    title: str


@dataclass(frozen=True)
class Scene:
    prompt: str
    say: str | None = None
    tools: tuple[Tool, ...] = ()
    files: tuple[tuple[str, str], ...] = ()
    reply: str | None = None
    asks: str | None = None
    cost: float | None = None


def scene(value: dict[str, Any]) -> Scene:
    """One scene from SCRIPT.json, refusing what the filter could not act on."""
    prompt = value["prompt"]
    if not isinstance(prompt, str) or not prompt.strip():
        raise ValueError("a scene needs a prompt")
    tools = tuple(Tool(t["kind"], t["title"]) for t in value.get("tools", []))
    for tool in tools:
        if tool.kind not in TOOL_KINDS:
            raise ValueError(f"tool kind {tool.kind!r} is not one of ACP's")
    files = tuple(value.get("files", {}).items())
    for path, text in files:
        pure = PurePosixPath(path)
        if pure.is_absolute() or ".." in pure.parts:
            raise ValueError(f"file {path} is not inside the working directory")
        if not isinstance(text, str):
            raise ValueError(f"file {path} needs its text")
    asks = value.get("asks")
    if asks is not None and (not isinstance(asks, str) or "=" not in asks or any(c.isspace() for c in asks)):
        raise ValueError("asks is one PATH=CONTENT word, with no whitespace")
    cost = value.get("cost")
    if cost is not None and (isinstance(cost, bool) or not isinstance(cost, (int, float)) or cost < 0):
        raise ValueError("cost is a number of dollars, not negative")
    return Scene(prompt, value.get("say"), tools, files, value.get("reply"), asks, cost)


def load(path: Path) -> list[Scene]:
    scenes = [scene(value) for value in json.loads(path.read_text(encoding="utf-8"))["scenes"]]
    prompts = [s.prompt for s in scenes]
    if len(set(prompts)) != len(prompts):
        raise ValueError("two scenes share a prompt")
    return scenes


def step(path: Path) -> float:
    """How long the stream-json player waits before each frame after the first, in seconds."""
    value = json.loads(path.read_text(encoding="utf-8")).get("step", 0)
    if isinstance(value, bool) or not isinstance(value, (int, float)) or value < 0:
        raise ValueError("step is a number of seconds, not negative")
    return float(value)


def prompt_text(message: Any) -> str | None:
    """The text of a `session/prompt` request, or None for any other message."""
    if not isinstance(message, dict) or message.get("method") != "session/prompt" or "id" not in message:
        return None
    blocks = message.get("params", {}).get("prompt", [])
    return "\n".join(b.get("text", "") for b in blocks if b.get("type") == "text")


def match(scenes: list[Scene], text: str) -> Scene | None:
    """The scene whose prompt the text contains, or the longest when one prompt holds another."""
    found = [s for s in scenes if s.prompt in text]
    return max(found, key=lambda s: len(s.prompt)) if found else None


def directive(scene: Scene) -> str:
    if scene.asks:
        return f"PERMISSION WRITE {scene.asks}"
    return f"REPLY_FILE {scene.reply}" if scene.reply else "HANG"


def rewrite(message: dict[str, Any], scene: Scene) -> dict[str, Any]:
    """The prompt request with its text replaced by the scene's one directive."""
    params = dict(message["params"], prompt=[{"type": "text", "text": directive(scene)}])
    return dict(message, params=params)


def updates(scene: Scene, message: dict[str, Any]) -> list[dict[str, Any]]:
    """What the agent says before the reply to this prompt: its text, then each tool call. A scene
    that hangs leaves its last call in progress, as a turn that is still working would."""
    said = []
    if scene.say:
        said.append({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": scene.say}})
    for i, tool in enumerate(scene.tools, 1):
        running = scene.reply is None and i == len(scene.tools)
        said.append(
            {
                "sessionUpdate": "tool_call",
                "toolCallId": f"scripted-{message['id']}-{i}",
                "title": tool.title,
                "kind": tool.kind,
                "status": "in_progress" if running else "completed",
            }
        )
    session = message["params"]["sessionId"]
    return [
        {"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": update}}
        for update in said
    ]


def write_files(scene: Scene, root: Path) -> None:
    for path, text in scene.files:
        target = root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="utf-8")


class Filter:
    """Both directions of the agent's stdio. Both write to `out`, so every write takes the lock."""

    def __init__(self, scenes: list[Scene], agent: subprocess.Popen[bytes], out: BinaryIO) -> None:
        assert agent.stdin is not None and agent.stdout is not None
        self.scenes = scenes
        self.agent = agent
        self.out = out
        self.lock = threading.Lock()

    def send(self, line: bytes) -> None:
        with self.lock:
            self.out.write(line)
            self.out.flush()

    def from_agent(self) -> None:
        for line in self.agent.stdout or ():
            self.send(line)

    def from_client(self, line: bytes) -> bytes:
        """What the agent gets for this line from the client, after acting out a scene's prompt."""
        try:
            message = json.loads(line)
        except ValueError:
            return line
        text = prompt_text(message)
        found = match(self.scenes, text) if text is not None else None
        if found is None:
            return line
        write_files(found, Path.cwd())
        for update in updates(found, message):
            self.send(json.dumps(update).encode() + b"\n")
        return json.dumps(rewrite(message, found)).encode() + b"\n"

    def from_clients(self, lines: BinaryIO) -> None:
        stdin = self.agent.stdin
        assert stdin is not None
        with contextlib.suppress(BrokenPipeError):
            for line in lines:
                stdin.write(self.from_client(line))
                stdin.flush()
            stdin.close()


# The model each call is billed as, one the engine's price catalog knows, and the usage of one
# call: about three and a half cents a call at that model's prices.
MODEL = "claude-sonnet-4-6"
CALL_USAGE = {"input_tokens": 3000, "output_tokens": 900, "cache_read_input_tokens": 40000}


def user_text(message: Any) -> str | None:
    """The text of a stream-json `user` message, or None for any other message."""
    if not isinstance(message, dict) or message.get("type") != "user":
        return None
    content = message.get("message", {}).get("content", [])
    if isinstance(content, str):
        return content
    return "\n".join(b.get("text", "") for b in content if isinstance(b, dict) and b.get("type") == "text")


def ask_path(scene: Scene) -> tuple[str, str]:
    path, _, content = (scene.asks or "").partition("=")
    return path, content


class StreamJson:
    """Claude Code's stream-json protocol, played from the script with no agent behind it. The
    reader answers control requests while a turn plays on its own thread, so an interrupt stops a
    turn between frames; every write takes the lock."""

    def __init__(self, scenes: list[Scene], out: BinaryIO, pause: float = 0, root: Path | None = None) -> None:
        self.scenes = scenes
        self.out = out
        self.pause = pause
        self.root = root or Path.cwd()
        self.lock = threading.Lock()
        self.session = "scripted-session"
        self.started = False
        self.calls = 0
        self.turn: threading.Thread | None = None
        self.stop = threading.Event()
        self.answer = threading.Event()
        # Set while the turn waits on a line yet to come: an answer, or an interrupt.
        self.blocked = threading.Event()
        self.allowed = False
        self.ended = False

    def send(self, message: dict[str, Any]) -> None:
        with self.lock:
            self.out.write(json.dumps(message).encode() + b"\n")
            self.out.flush()

    def reply(self, request_id: str) -> None:
        self.send(
            {"type": "control_response", "response": {"subtype": "success", "request_id": request_id, "response": {}}}
        )

    def frame(self, block: dict[str, Any]) -> None:
        """One `assistant` frame: one model call, with its usage, saying `block`."""
        self.calls += 1
        message = {
            "id": f"msg-scripted-{self.calls}",
            "type": "message",
            "role": "assistant",
            "model": MODEL,
            "content": [block],
            "usage": CALL_USAGE,
        }
        self.send({"type": "assistant", "message": message, "parent_tool_use_id": None, "session_id": self.session})

    def result(self, scene: Scene, text: str | None, interrupted: bool = False) -> None:
        with self.lock:
            if self.ended:
                return
            self.ended = True
        message: dict[str, Any] = {
            "type": "result",
            "subtype": "error_during_execution" if interrupted else "success",
            "is_error": interrupted,
            "num_turns": 1,
            "session_id": self.session,
        }
        if interrupted:
            message["terminal_reason"] = "aborted_streaming"
        else:
            message["result"] = text or ""
        if scene.cost is not None:
            message["total_cost_usd"] = scene.cost
        self.send(message)

    def waited(self) -> bool:
        """Wait out one step; whether the turn was interrupted meanwhile."""
        return self.stop.wait(self.pause)

    def play(self, scene: Scene) -> None:
        if scene.say:
            self.frame({"type": "text", "text": scene.say})
        for i, tool in enumerate(scene.tools, 1):
            if self.waited():
                return
            self.frame(
                {"type": "tool_use", "id": f"toolu-scripted-{self.calls + 1}-{i}", "name": tool.title, "input": {}}
            )
        if scene.asks:
            if self.waited():
                return
            path, content = ask_path(scene)
            request = {
                "subtype": "can_use_tool",
                "tool_name": "Write",
                "input": {"file_path": path, "content": content},
            }
            self.send({"type": "control_request", "request_id": "scripted-ask-1", "request": request})
            self.blocked.set()
            while not self.answer.wait(0.1):
                if self.stop.is_set():
                    return
            self.blocked.clear()
            if not self.allowed:
                self.result(scene, f"Not allowed to write {path}.")
                return
            (self.root / path).write_text(content, encoding="utf-8")
            if scene.reply is None:
                self.result(scene, f"Wrote {path}.")
                return
        if scene.reply is None:
            self.blocked.set()
            self.stop.wait()
            return
        if self.waited():
            return
        said = Path(scene.reply).read_text(encoding="utf-8")
        self.frame({"type": "text", "text": said})
        self.result(scene, said)

    def prompted(self, text: str) -> None:
        if not self.started:
            self.started = True
            self.send({"type": "system", "subtype": "init", "session_id": self.session, "model": MODEL})
        found = match(self.scenes, text)
        if found is None:
            self.result(Scene(prompt=text), "No scene for this prompt.")
            return
        write_files(found, self.root)
        self.stop.clear()
        self.answer.clear()
        self.blocked.clear()
        self.ended = False
        self.turn = threading.Thread(target=self.play, args=(found,), daemon=True)
        self.turn.start()

    def run(self, lines: BinaryIO) -> int:
        current: Scene | None = None
        for line in lines:
            try:
                message = json.loads(line)
            except ValueError:
                continue
            kind = message.get("type") if isinstance(message, dict) else None
            if kind == "control_request":
                subtype = message.get("request", {}).get("subtype")
                self.reply(message.get("request_id", ""))
                if subtype == "interrupt" and self.turn is not None and self.turn.is_alive():
                    self.stop.set()
                    self.turn.join()
                    self.result(current or Scene(prompt=""), None, interrupted=True)
                elif subtype == "end_session":
                    return 0
            elif kind == "control_response":
                behavior = message.get("response", {}).get("response", {}).get("behavior")
                self.allowed = behavior == "allow"
                self.answer.set()
            else:
                text = user_text(message)
                if text is not None:
                    current = match(self.scenes, text)
                    self.prompted(text)
        # Input closed: let a turn finish what it can still do, then stop the one that waits on
        # input that will not come.
        while (
            self.turn is not None and self.turn.is_alive() and not (self.blocked.is_set() and not self.answer.is_set())
        ):
            self.turn.join(0.05)
        self.stop.set()
        return 0


def main(argv: list[str]) -> int:
    if len(argv) >= 2 and argv[1] == "--stream-json":
        script = Path(argv[0])
        return StreamJson(load(script), sys.stdout.buffer, step(script)).run(sys.stdin.buffer)
    if len(argv) < 3 or argv[1] != "--":
        print("usage: scripted_agent.py SCRIPT.json -- AGENT [ARG...]", file=sys.stderr)
        print("       scripted_agent.py SCRIPT.json --stream-json [ARG...]", file=sys.stderr)
        return 2
    scenes = load(Path(argv[0]))
    agent = subprocess.Popen(argv[2:], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    relay = Filter(scenes, agent, sys.stdout.buffer)
    threading.Thread(target=relay.from_clients, args=(sys.stdin.buffer,), daemon=True).start()
    relay.from_agent()
    return agent.wait()


if __name__ == "__main__":
    status = main(sys.argv[1:])
    sys.stderr.flush()
    # The client's reader may still hold stdin's lock, which a normal interpreter exit waits on.
    os._exit(status)
