#!/usr/bin/env -S uv run
"""Regenerate the snouty "gallery" against fresh, live run IDs.

The gallery is a set of Markdown files, one per "story". Each story models a
hypothetical user with a concrete *goal*, runs the snouty command that user
would run, captures the output, and records a plain-language *rubric* for how a
human or LLM reviewer should judge whether the output satisfied that goal. Every
story is also gated by a programmatic check; a degenerate example (an empty
table, an unintended error, a filter that didn't narrow) fails the run rather
than being silently emitted.

There are three kinds of story:

  * goal stories — capture one command's output and judge it against a user goal
    (the bulk of the gallery; slugs like `runs-events-single`).
  * help stories — capture `snouty <cmd> --help` next to that command's default
    output (slugs like `help-runs-properties`) and judge whether the help is
    informative, clear, concise, consistent, and *aligned* with what the command
    prints. Commands that mutate state or need an interactive arg (launch,
    debug, validate, update, completions) are help-only. An automated check
    verifies any column/field the help names actually appears in the output.
  * TTY stories — drive an interactive command (today only `snouty login`) on a
    real pseudo-terminal and record the conversation: one *frame* per prompt,
    an asciinema recording to replay, and the files the command persisted. See
    "TTY stories" below.

Credentials come from the usual ANTITHESIS_* environment variables (snouty reads
them). Behaviour is controlled with flags, not env vars:

    uv run scripts/gen-gallery.py --out ./out
    uv run scripts/gen-gallery.py --only runs-events-single
    uv run scripts/gen-gallery.py --list

Nothing is written to ./gallery; output goes to a tempdir (or --out) so you
never accidentally commit it. Diff successive runs (or against ./gallery) to see
how snouty changes affect command output.
"""

from __future__ import annotations

import argparse
import codecs
import fnmatch
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Callable

import pexpect
import pyte

# A syntactically-valid but nonexistent run id, for clean-error stories.
UNKNOWN_RUN = "ffffffffffffffffffffffffffffffff-54-5"

# Stories are captured concurrently (each is one or two snouty subprocesses that
# can each block on snouty's 60s timeout); checks are then evaluated serially.
CAPTURE_WORKERS = 6

# Committed `snouty validate` sample projects (their goal is documented in the
# comment atop each docker-compose.yaml). The validate stories run `snouty
# validate` against these.
SAMPLES_DIR = Path(__file__).resolve().parent.parent / "tests" / "fixtures" / "validate"

# The live samples bake their test commands into images; `snouty validate`
# never builds or pulls, so the images must exist first. This script builds
# them (and pulls the glibc base) and is run before the live validate stories.
BUILD_SAMPLES_SCRIPT = Path(__file__).resolve().parent / "build-validate-samples.sh"

# Keywords probed (in order) to sample a real event from a run. --match is
# required, so a truly unfiltered call isn't possible; ordering broadest-first
# (a near-universal needle, then common log words) makes the common case a
# single probe per run before falling back to narrower terms.
# Keyword order probed when picking the completed run's event stories.
# Distinctive words first: a stopword like "the" matches nearly every event,
# so a story built on it exercises the result cap instead of the search and
# reads as unrealistic. A run matching none of these is skipped by discovery.
EVENT_KEYWORDS = ["error", "start", "setup", "client", "test", "info"]

# Keyword order probed for an incomplete run's events story: its narrative is
# "events around the failure", so try "error" before the general needles.
INCOMPLETE_EVENT_KEYWORDS = ["error", *EVENT_KEYWORDS]


class GalleryError(Exception):
    """A precondition could not be met; refuse to emit a partial gallery."""


# ---------------------------------------------------------------------------
# snouty runner
# ---------------------------------------------------------------------------


@dataclass
class Result:
    args: list[str]
    stdout: str
    stderr: str
    returncode: int

    @property
    def ok(self) -> bool:
        return self.returncode == 0

    @property
    def combined(self) -> str:
        # Stories showcase verbose/error output, which snouty writes to stderr.
        return self.stdout if not self.stderr else f"{self.stdout}{self.stderr}"


class Snouty:
    """Shells out to the snouty binary. The gallery is a black-box test of the
    CLI, so we never reach into snouty's internals — we run it and read what a
    user would see."""

    def __init__(self, binary: Path):
        self.binary = binary
        # `runs show --web` shells out to xdg-open/open. Drop no-op shims on PATH
        # (and point $BROWSER at one) so regenerating never spawns a browser.
        self._shim = Path(tempfile.mkdtemp(prefix="snouty-gallery-shim."))
        for prog in ("xdg-open", "open"):
            shim = self._shim / prog
            shim.write_text("#!/bin/sh\nexit 0\n")
            shim.chmod(0o755)
        self._env = dict(os.environ)
        self._env["PATH"] = f"{self._shim}{os.pathsep}{self._env.get('PATH', '')}"
        self._env["BROWSER"] = str(self._shim / "xdg-open")

    def cleanup(self) -> None:
        shutil.rmtree(self._shim, ignore_errors=True)

    def env_with(self, overrides: dict[str, str | None]) -> dict[str, str]:
        """The environment a snouty child inherits, with `overrides` applied: a
        string sets a var, None removes it (so a story can model an environment
        missing some credential). Everything else inherits the live
        ANTITHESIS_* env."""
        env = dict(self._env)
        for key, value in overrides.items():
            if value is None:
                env.pop(key, None)
            else:
                env[key] = value
        return env

    def run(
        self,
        args: list[str],
        env: dict[str, str | None] | None = None,
    ) -> Result:
        proc = subprocess.run(
            [str(self.binary), *args],
            capture_output=True,
            text=True,
            env=self._env if env is None else self.env_with(env),
        )
        return Result(args, proc.stdout, proc.stderr, proc.returncode)

    def json_lines(self, args: list[str], env: dict[str, str | None] | None = None) -> list[dict]:
        """Run `snouty --json <args>` and parse NDJSON rows.

        A non-zero exit is a hard error (timeout, 5xx, auth, DNS, ...) and raises
        — discovery distinguishes this from an empty-but-healthy result (exit 0,
        no rows). This is exact, unlike grepping stderr for known phrases."""
        res = self.run(["--json", *args], env)
        if not res.ok:
            raise GalleryError(
                f"`snouty {' '.join(args)}` failed (exit {res.returncode}): "
                f"{res.stderr.strip() or '<no stderr>'}"
            )
        rows = []
        for line in res.stdout.splitlines():
            line = line.strip()
            if line:
                rows.append(json.loads(line))
        return rows

    def json_obj(self, args: list[str]) -> dict:
        res = self.run(["--json", *args])
        if not res.ok:
            raise GalleryError(
                f"`snouty {' '.join(args)}` failed (exit {res.returncode}): "
                f"{res.stderr.strip() or '<no stderr>'}"
            )
        return json.loads(res.stdout)


# ---------------------------------------------------------------------------
# TTY driver: run an interactive command on a real pseudo-terminal.
#
# `snouty login` prompts through `inquire`, which engages only when stdin is a
# terminal — piping answers at it takes the non-interactive path instead. A TTY
# story therefore spawns snouty on a pseudo-terminal and types at it.
#
# What the terminal receives is not readable as it stands: `inquire` redraws the
# whole prompt on every keystroke, in ANSI escapes. `pyte` replays that stream
# the way a terminal would, so a *frame* is the screen exactly as it stood at
# one moment. The same stream is written out as an asciinema v2 recording, which
# `asciinema play` replays at its original speed.
# ---------------------------------------------------------------------------


# The keys a dialogue step types: either a literal string, or — when they depend
# on what snouty drew — a function of the settled screen (see `pick`).
Keys = str | Callable[[list[str]], str]

# One step of a dialogue: text to wait for on screen, then the keys to type once
# it is there. Every send is gated on its prompt being rendered, so no keystroke
# can race ahead of the prompt meant to read it.
Step = tuple[str, Keys]

# One frame: the prompt that was waiting, and the screen at that moment.
Frame = tuple[str, str]

# Keys a step can send. `inquire` holds the terminal in raw mode for a prompt's
# whole lifetime, so these arrive as key events rather than line-edited text.
ENTER = "\r"
DOWN = "\x1b[B"
UP = "\x1b[A"

# The pseudo-terminal's size. 120 columns keeps snouty's own lines — which carry
# absolute paths under the throwaway `$HOME` — clear of the wrap boundary, so a
# reviewer judges snouty's wording rather than the sandbox's path length. 40
# rows fits a whole login on one screen.
TTY_COLS = 120
TTY_ROWS = 40

# How long to wait for one prompt. Generous: a healthy exchange completes in
# milliseconds, but the credential menu waits on snouty probing the tenant for
# its OAuth configuration first.
PROMPT_TIMEOUT = 30

# How long the output must stay quiet before a frame is taken. A prompt matches
# mid-stream, so without this the frame would catch a half-drawn screen.
SETTLE = 0.2

# `inquire`'s row prefixes (see its RenderConfig defaults): the prompt still
# being answered leads with `?`, one already answered with `>`, the highlighted
# menu row with `>`, and every other menu row with a space. Each is followed by
# one more space.
LIVE_PROMPT = "? "
HIGHLIGHTED = "> "
UNHIGHLIGHTED = "  "


def pick(prompt: str, label: str) -> Step:
    """A step that chooses `label` from an `inquire` menu: move the highlight
    onto that row, then select it.

    The keys are read off the screen rather than fixed, because the menu's
    contents depend on the tenant — `snouty login` offers single sign-on only
    where the tenant enables it — so a fixed count of arrow presses would land
    on the wrong row."""

    def keys(screen: list[str]) -> str:
        options = _menu_options(screen)
        labels = [text for text, _ in options]
        if label not in labels:
            raise GalleryError(f"menu has no {label!r} option; it offers {labels}")
        highlighted = next((i for i, (_, on) in enumerate(options) if on), 0)
        distance = labels.index(label) - highlighted
        arrow = DOWN if distance > 0 else UP
        return arrow * abs(distance) + ENTER

    return (prompt, keys)


def _menu_options(screen: list[str]) -> list[tuple[str, bool]]:
    """Every option of the `inquire` menu on `screen`: its label, and whether it
    is the highlighted one.

    The menu sits under the prompt still being answered — the last line leading
    with `?`, since an answered prompt is redrawn with `>`. Its options are the
    rows below that, up to the first row that is neither highlighted nor
    indented (the help line, or a blank row past the end of the menu)."""
    live = [i for i, line in enumerate(screen) if line.startswith(LIVE_PROMPT)]
    if not live:
        return []
    options: list[tuple[str, bool]] = []
    for line in screen[live[-1] + 1 :]:
        if line.startswith(HIGHLIGHTED):
            options.append((line[2:].strip(), True))
        elif line.startswith(UNHIGHLIGHTED) and line.strip():
            options.append((line[2:].strip(), False))
        else:
            break
    return options


class _Recorder:
    """Sink for everything the child writes.

    `pexpect` hands its `logfile_read` every byte it reads, once and in order —
    unlike `before`/`after`, which repeat whatever is still in its buffer. Each
    chunk is timestamped for the recording and fed to the terminal emulator for
    the frames.

    Both outputs decode incrementally, because a read can split a multi-byte
    character: `pyte.ByteStream` carries the leftover bytes into the next chunk,
    and the recording's own decoder does the same. Decoding each chunk on its own
    would turn a split `↑` into replacement characters in the recording while the
    frames still rendered it correctly."""

    def __init__(self, screen: pyte.Screen):
        self.started = time.monotonic()
        self.events: list[tuple[float, str]] = []
        self.stream = pyte.ByteStream(screen)
        self._decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def write(self, data: bytes) -> None:
        if not data:
            return
        self.stream.feed(data)
        text = self._decoder.decode(data)
        # A chunk that held only the first bytes of a split character decodes to
        # nothing; its bytes arrive with the next one.
        if text:
            self.events.append((time.monotonic() - self.started, text))

    def flush(self) -> None:
        pass


class TtySession:
    """One interactive snouty run on a pseudo-terminal.

    Holds the child, the bytes it has written (kept whole, for the recording),
    and a terminal emulator fed the same bytes (for the frames)."""

    def __init__(self, binary: Path, args: list[str], env: dict[str, str]):
        self.screen = pyte.Screen(TTY_COLS, TTY_ROWS)
        self.recorder = _Recorder(self.screen)
        # `echo=False` stops the terminal driver echoing what we type, so a
        # secret can only reach the screen if snouty itself renders it — which
        # is exactly what the `secrets_absent` check asserts.
        self.child = pexpect.spawn(
            str(binary),
            args,
            env=env,
            dimensions=(TTY_ROWS, TTY_COLS),
            timeout=PROMPT_TIMEOUT,
            echo=False,
            encoding=None,
        )
        # `logfile_read` is an attribute rather than a constructor argument.
        # Setting it here still catches every byte: pexpect reads the child only
        # when asked to, and nothing has asked yet.
        self.child.logfile_read = self.recorder

    def wait_for(self, needle: str) -> None:
        """Consume output through `needle`, then let the prompt finish drawing."""
        self.child.expect_exact(needle.encode())
        self.settle()

    def settle(self) -> None:
        """Wait until the output has stayed quiet for `SETTLE`, so a frame
        catches a settled screen rather than a half-drawn one. A prompt matches
        mid-stream, and `inquire` draws a menu's rows after its question.
        Matching on TIMEOUT is how pexpect waits for silence."""
        self.child.expect([pexpect.TIMEOUT, pexpect.EOF], timeout=SETTLE)

    def send(self, keys: str) -> None:
        self.child.send(keys.encode())

    def frame(self) -> str:
        """The screen as it stands, trailing blank rows dropped."""
        rows = [row.rstrip() for row in self.screen.display]
        while rows and not rows[-1]:
            rows.pop()
        return "\n".join(rows)

    def finish(self, timeout: float) -> int:
        """Wait up to `timeout` for the child to exit and return its status.

        A dialogue that ran to the end leaves a child that is already exiting, so
        the wait is short in practice. A story that broke off early leaves one
        sitting at a prompt no one will answer, and waiting the full prompt
        budget for an exit that cannot come only adds dead time to a run that has
        already failed — such a caller passes a short timeout."""
        try:
            self.child.expect(pexpect.EOF, timeout=timeout)
        except pexpect.TIMEOUT:
            # The child is still up after a dialogue that should have ended it —
            # a stalled story, already recorded as a marker frame. Fall through
            # to the forced close, which is what ends it.
            pass
        self.child.close(force=True)
        # A child killed after a stall has no exit status of its own; report it
        # as a failure so the story's check fails rather than reading as clean.
        return 1 if self.child.exitstatus is None else self.child.exitstatus

    def cast(self, command: str) -> str:
        """The session as an asciinema v2 recording: a header line, then one
        `[time, "o", data]` line per chunk the terminal received."""
        header = {
            "version": 2,
            "width": TTY_COLS,
            "height": TTY_ROWS,
            "timestamp": int(time.time()),
            "command": command,
            "env": {"TERM": "xterm-256color"},
        }
        lines = [json.dumps(header)]
        for at, text in self.recorder.events:
            lines.append(json.dumps([round(at, 6), "o", text]))
        return "\n".join(lines) + "\n"


def drive_tty(
    binary: Path,
    args: list[str],
    env: dict[str, str],
    dialogue: tuple[Step, ...],
) -> tuple[Result, list[Frame], str]:
    """Run `snouty <args>` on a pseudo-terminal, typing `dialogue` at it.

    Returns the run's result (its output is the final screen), one frame per
    step — the screen the user faced when that prompt asked for an answer — plus
    a closing frame, and the asciinema recording.

    A step that cannot be taken — its prompt never arrives, or the screen does
    not offer what the step meant to choose — is reported in the frames and as a
    failing exit, so it shows up in that one story rather than aborting the whole
    gallery."""
    session = TtySession(binary, args, env)
    frames: list[Frame] = []
    completed = True
    for prompt, keys in dialogue:
        try:
            session.wait_for(prompt)
        except (pexpect.TIMEOUT, pexpect.EOF) as e:
            stalled = "no output" if isinstance(e, pexpect.TIMEOUT) else "snouty exited"
            frames.append((f"[gallery] waiting for {prompt!r}: {stalled}", session.frame()))
            completed = False
            break
        frames.append((prompt, session.frame()))
        try:
            session.send(keys if isinstance(keys, str) else keys(session.screen.display))
        except GalleryError as e:
            frames.append((f"[gallery] answering {prompt!r}: {e}", session.frame()))
            completed = False
            break
    # A story that broke off leaves snouty waiting at a prompt, so give it only
    # long enough to be sure rather than the whole prompt budget.
    returncode = session.finish(PROMPT_TIMEOUT if completed else SETTLE)
    # The child has exited, so nothing can feed the emulator from here: one
    # closing screen serves as both the last frame and the story's output.
    closing = session.frame()
    frames.append(("[after it exits]", closing))
    return (
        Result(args, closing, "", returncode),
        frames,
        session.cast(_command_line(args)),
    )


# ---------------------------------------------------------------------------
# Discovery: pick runs and derive values that make each story meaningful.
# ---------------------------------------------------------------------------


@dataclass
class Discovery:
    # All fields default so `--list` can build an empty Discovery() just to
    # enumerate slugs, immune to future field additions.
    success: str = ""  # completed run that drives the event/logs/property stories
    fail: str = ""  # an incomplete run
    cancelled: str = ""  # a cancelled run
    launcher: str = ""  # a real launcher value (for the --launcher story)
    created_after: str = ""  # a timestamp with runs after it
    window_after: str = ""
    window_before: str = ""
    event_keyword: str = ""
    event_kw2: str = ""
    event_hash: str = ""
    # The exact text of the JSON number, so positional moment arguments reach
    # the API byte-identically.
    event_vtime: str = "0"
    fail_prop: str = ""  # failing event property whose detail shows counter-examples
    pass_event_prop: str = ""  # passing event property whose detail shows examples
    nonevent_prop: str = ""  # non-event property whose detail shows a real value
    name_filter: str = ""  # substring matching exactly one property name
    fail_hash: str = ""
    fail_vtime: str = ""
    fail_event_kw: str = "error"  # keyword the incomplete run's events story matches on


def _first_run(sn: Snouty, *filters: str) -> str | None:
    rows = sn.json_lines(["runs", "list", *filters, "-n", "1"])
    return rows[0]["run_id"] if rows else None


def _sample_event(sn: Snouty, run: str) -> dict | None:
    """Return the first event matching any probe keyword that is usable for the
    event/logs stories, or None if the run has no such event. Raises GalleryError
    if the endpoint is unreachable.

    "Usable" means it has a moment with both coordinates (for the logs stories)
    *and* yields a distinctive second needle (for the multi-match story); we stash
    both on the returned row so discovery doesn't have to re-derive them."""
    for kw in EVENT_KEYWORDS:
        rows = sn.json_lines(["runs", "events", run, "--match", kw])
        for row in rows:
            moment = row.get("moment") or {}
            if not (moment.get("input_hash") and moment.get("vtime")):
                continue
            second = _pick_second_needle(row, kw)
            if second is None:
                continue
            row["_keyword"] = kw
            row["_second_needle"] = second
            return row
    return None


@dataclass
class CompletedPick:
    """A completed run that can drive *every* completed-run story, plus the
    per-story selections derived from it (so discovery doesn't re-derive them)."""

    run: str
    event: dict
    fail_prop: str
    pass_prop: str
    nonevent_prop: str
    name_filter: str


def _pick_completed_run(sn: Snouty, scan: int) -> CompletedPick:
    """Pick a completed run that can drive *all* the completed-run stories, not
    just the event/logs ones. Earlier this committed to the first run with
    sampleable events, then `discover` separately demanded that same run also
    render failing/passing/non-event property moments — so a run with events but
    no failing-property moments (the first completed run on a tenant routinely is
    one) sank the whole gallery. Instead, scan recent completed runs and take the
    first that satisfies every requirement at once."""
    runs = sn.json_lines(["runs", "list", "--status", "completed", "-n", str(scan)])
    if not runs:
        raise GalleryError("no completed runs found on this tenant")
    last_reason = "none had sampleable events"
    for r in runs:
        run = r["run_id"]
        try:
            event = _sample_event(sn, run)
        except GalleryError:
            print(f"  skip {run}: events endpoint unreachable", file=sys.stderr)
            continue
        if event is None:
            print(f"  skip {run}: no sampleable events", file=sys.stderr)
            continue
        # Has events; now require it to drive the property stories too. Each
        # picker raises GalleryError if this run can't satisfy its story — catch
        # it and move on rather than committing to a run that fails downstream.
        props = sn.json_lines(["runs", "properties", run])
        try:
            fail_prop = _pick_property_with_moments(sn, run, props, "Failing")
            pass_prop = _pick_property_with_moments(sn, run, props, "Passing")
            nonevent_prop = _pick_nonevent_property(sn, run, props)
            name_filter = _pick_name_filter([p["name"] for p in props])
        except GalleryError as e:
            print(f"  skip {run}: {e}", file=sys.stderr)
            last_reason = str(e)
            continue
        print(
            f"  completed run : {run} (events matched '{event['_keyword']}')",
            file=sys.stderr,
        )
        return CompletedPick(
            run, event, fail_prop, pass_prop, nonevent_prop, name_filter
        )
    raise GalleryError(
        f"none of the {len(runs)} most recent completed runs can drive every "
        f"completed-run story (need sampleable events plus failing/passing/"
        f"non-event property moments and a unique name filter); last reason: "
        f"{last_reason}"
    )


def _real_failure_moment(moment: dict) -> bool:
    """Whether a run's failure moment is a real, streamable coordinate. The API
    reports the sentinel `input_hash "0"`, `vtime 0` for an incomplete run with no
    specific failure point (a timeout or kill, not a moment-pinned failure) and
    omits the fields for some runs; neither yields any logs. snouty emits vtime
    as a JSON number, so the zero check is numeric."""
    h, v = moment.get("input_hash"), moment.get("vtime")
    if not h or v is None:
        return False
    return not (str(h) == "0" and float(v) == 0.0)


def _pick_incomplete_run(sn: Snouty, scan: int) -> tuple[str, dict, str]:
    """Pick an incomplete run that makes the incomplete-run stories meaningful,
    scanning the recent ones rather than blindly taking the first — which is
    routinely a timeout with a 0/0 failure moment and no error events, leaving the
    logs/events stories empty. The chosen run must have a real failure moment whose
    logs are non-empty (so runs-logs-incomplete streams lines and runs-show-incomplete
    renders a moment) AND events matching a probe keyword (for runs-events-incomplete).
    Returns (run_id, failure_moment, event_keyword)."""
    runs = sn.json_lines(["runs", "list", "--status", "incomplete", "-n", str(scan)])
    if not runs:
        raise GalleryError("no incomplete run found — incomplete stories cannot run")
    for r in runs:
        run = r["run_id"]
        moment = sn.json_obj(["runs", "show", run]).get("failure_moment") or {}
        if not _real_failure_moment(moment):
            print(f"  skip {run}: no real failure moment (0/0 sentinel)", file=sys.stderr)
            continue
        h, v = str(moment["input_hash"]), str(moment["vtime"])
        try:
            logs = sn.json_lines(["runs", "logs", run, h, v])
        except GalleryError:
            logs = []
        if not logs:
            print(f"  skip {run}: no logs at the failure moment", file=sys.stderr)
            continue
        kw = None
        for k in INCOMPLETE_EVENT_KEYWORDS:
            try:
                if sn.json_lines(["runs", "events", run, "--match", k]):
                    kw = k
                    break
            except GalleryError:
                break  # events endpoint unreachable for this run; move on
        if kw is None:
            print(f"  skip {run}: no events match a probe keyword", file=sys.stderr)
            continue
        print(
            f"  incomplete run: {run} (logs at failure moment; events match '{kw}')",
            file=sys.stderr,
        )
        return run, moment, kw
    raise GalleryError(
        f"none of the {len(runs)} most recent incomplete runs has a real failure "
        "moment with logs and matching events — refusing to write a gallery with "
        "the incomplete event/logs stories degenerate"
    )


# Envelope keys and ubiquitous JSON literals that occur on (nearly) every raw
# NDJSON event line. A second --match needle drawn from these cannot narrow the
# result set, so the multi-match story's `len(rows) < single` check is
# unsatisfiable — exclude them entirely.
_UBIQUITOUS_TOKENS = frozenset(
    {
        "output_text",
        "moment",
        "input_hash",
        "vtime",
        "source",
        "container",
        "stream",
        "name",
        "level",
        "true",
        "false",
        "null",
    }
)


def _pick_second_needle(event: dict, keyword: str) -> str | None:
    """A token that co-occurs with the keyword in this event and is *distinctive*
    enough to narrow a search — drawn from the event's output_text content, never
    from envelope keys. Returns None when no distinctive token exists so the
    caller can try a different event/run rather than committing to a needle that
    cannot narrow the multi-match story."""
    kw = keyword.lower()
    for t in re.findall(r"[A-Za-z_]{4,}", event.get("output_text") or ""):
        low = t.lower()
        if low != kw and low not in _UBIQUITOUS_TOKENS:
            return t
    return None


def _render_property(sn: Snouty, run: str, name: str) -> str:
    # `--name` is a substring filter; passing the exact name selects it (plus any
    # other name it's a substring of, which is fine for these render probes).
    return sn.run(["runs", "properties", run, "--name", name, "--detail"]).combined


_MOMENT_ROW = re.compile(r"-?\d{6,}\s+\d+\.\d+")  # long hash + float vtime


def _has_moment_rows(rendered: str) -> bool:
    return _MOMENT_ROW.search(rendered) is not None


def _moments(values: list) -> list:
    """The subset of an examples/counterexamples array that has a moment with
    both coordinates — exactly the elements snouty renders as HASH/VTIME rows."""
    out = []
    for v in values or []:
        moment = (v.get("moment") if isinstance(v, dict) else None) or {}
        if moment.get("input_hash") and moment.get("vtime"):
            out.append(v)
    return out


def _pick_property_with_moments(sn: Snouty, run: str, props: list[dict], status: str) -> str:
    """Pick a property of the given status that has example/counter-example moment
    rows. The properties JSON already carries the full `examples`/`counterexamples`
    arrays snouty renders, so we select straight from it (most moments first) and
    only render-probe the one chosen property as a cheap safety net."""
    arr_key = "counterexamples" if status == "Failing" else "examples"
    candidates = [
        (p, m)
        for p in props
        if p.get("status") == status and (m := _moments(p.get(arr_key) or []))
    ]
    candidates.sort(key=lambda c: len(c[1]), reverse=True)
    for p, _ in candidates:
        if _has_moment_rows(_render_property(sn, run, p["name"])):
            return p["name"]
        break  # the array said it has moments but the render disagreed — bail
    raise GalleryError(
        f"no {status} property on {run} renders example moments — cannot build "
        f"the {'failing' if status == 'Failing' else 'passing'} property story"
    )


# A non-event ("system") property renders its value under a `Result` label
# (see render_result in src/runs.rs) — `Result   <scalar>` inline, or a bare
# `Result` label line above indented JSON (no colon) for an object/array —
# never as a moment HASH/VTIME row.
_NONEVENT_RESULT = re.compile(r"^\s*Result\b", re.MULTILINE)


def _has_real_value(value) -> bool:
    """Whether a non-event example renders as a usable value — i.e. a scalar or a
    non-empty collection (an empty one renders as the `(no value)` placeholder)."""
    if isinstance(value, (list, dict)):
        return len(value) > 0
    return True  # scalars (incl. False/0) all render to a visible block


def _pick_nonevent_property(sn: Snouty, run: str, props: list[dict]) -> str:
    """Pick a non-event property that renders a real example value. We read the
    example arrays straight from the JSON and only render-probe the chosen one."""
    # `--name` is a substring filter, so the chosen name must not be a substring
    # of any *other* property's name — otherwise the --detail probe would also
    # expand that other property, and an event sibling's moment rows would make us
    # think we'd misclassified. Require the name to match only itself.
    lower_names = [p["name"].lower() for p in props]
    candidates = []
    for p in props:
        if p.get("is_event") is not False:
            continue
        if sum(p["name"].lower() in n for n in lower_names) != 1:
            continue
        values = (p.get("examples") or []) + (p.get("counterexamples") or [])
        if any(_has_real_value(v) for v in values):
            candidates.append(p)
    candidates.sort(key=lambda p: p.get("example_count") or 0, reverse=True)
    # Render-probe candidates (most examples first) until one renders the
    # non-event shape: a `Result` label block and no moment rows (which would mean
    # we misclassified an event property). Try several rather than bailing on the
    # first miss, so one oddly-rendering top candidate can't sink the whole story.
    for p in candidates[:8]:
        rendered = _render_property(sn, run, p["name"])
        if _NONEVENT_RESULT.search(rendered) and not _has_moment_rows(rendered):
            return p["name"]
    raise GalleryError(f"no non-event property on {run} renders a usable value")


def _pick_name_filter(prop_names: list[str]) -> str:
    """A case-insensitive word contained in exactly one property name — so the
    `--name` filter story narrows the list to a single, predictable property."""
    lower = [n.lower() for n in prop_names]
    for name in prop_names:
        for word in re.findall(r"[A-Za-z]{4,}", name):
            w = word.lower()
            if sum(w in n for n in lower) == 1:
                return word
    raise GalleryError("no substring matches exactly one property")


def discover(sn: Snouty, scan: int) -> Discovery:
    print("discovering runs via the live API…", file=sys.stderr)

    pick = _pick_completed_run(sn, scan)
    success, event = pick.run, pick.event

    fail, fail_moment, fail_event_kw = _pick_incomplete_run(sn, scan)
    cancelled = _first_run(sn, "--status", "cancelled")
    if not cancelled:
        raise GalleryError("no cancelled run found — the cancelled story cannot run")
    print(f"  cancelled run : {cancelled}", file=sys.stderr)

    # Dynamic listing params from real runs, so listing stories aren't empty.
    recent = sn.json_lines(["runs", "list", "-n", "30"])
    if not recent:
        raise GalleryError("no runs found at all")
    launcher = next((r["launcher"] for r in recent if r.get("launcher")), "")
    if not launcher:
        raise GalleryError("no run has a launcher — the --launcher story cannot run")
    by_time = sorted(recent, key=lambda r: r["created_at"])
    # created-after: a timestamp with several runs after it.
    created_after = by_time[max(0, len(by_time) - 6)]["created_at"]
    # created-window: brackets the middle of the recent runs.
    window_after = by_time[0]["created_at"]
    window_before = by_time[-1]["created_at"]

    keyword = event["_keyword"]
    moment = event["moment"]

    disc = Discovery(
        success=success,
        fail=fail,
        cancelled=cancelled,
        launcher=launcher,
        created_after=created_after,
        window_after=window_after,
        window_before=window_before,
        event_keyword=keyword,
        event_kw2=event["_second_needle"],
        event_hash=moment["input_hash"],
        event_vtime=str(moment["vtime"]),
        # Property-story selections were derived against `success` during the
        # holistic run pick, so reuse them rather than re-probing the API.
        fail_prop=pick.fail_prop,
        pass_event_prop=pick.pass_prop,
        nonevent_prop=pick.nonevent_prop,
        name_filter=pick.name_filter,
        # _pick_incomplete_run guarantees a real (non-0/0) failure moment.
        fail_hash=str(fail_moment["input_hash"]),
        fail_vtime=str(fail_moment["vtime"]),
        fail_event_kw=fail_event_kw,
    )
    return disc


# ---------------------------------------------------------------------------
# Checks: each returns (passed, detail). They validate the captured output so a
# degenerate story can never pass silently.
# ---------------------------------------------------------------------------


@dataclass
class Story:
    slug: str
    title: str
    goal: str
    judge: str
    args: list[str]
    check: Callable[["StoryRun", "Registry"], tuple[bool, str]]
    json_capable: bool = True  # can we re-run with --json for structured rows?
    # -- help stories ------------------------------------------------------
    # When `help_cmd` is set the story is a "help story": it captures
    # `snouty <help_cmd> --help` and renders it next to the command's default
    # output (`args`, plus any `samples`), so a reviewer can judge whether the
    # help is informative/clear/concise/consistent *and* matches what the
    # command actually prints. `args` may be empty (help-only, for commands we
    # must not run because they mutate state, e.g. launch/debug).
    help_cmd: list[str] | None = None
    # Extra labelled default-output captures shown after the primary one
    # (e.g. `runs list --detail`): list of (label, args).
    samples: list[tuple[str, list[str]]] | None = None
    # Tokens that must appear in BOTH the help text and the default output —
    # an automated "help aligns with output" gate (e.g. column headers the help
    # promises). Only enforced when there is default output to compare against.
    align_tokens: tuple[str, ...] = ()
    # Whether the default-output command is expected to exit 0. True for read
    # commands; set False for a command that legitimately exits non-zero while
    # still printing representative output (e.g. `doctor` when a check fails).
    expect_ok: bool = True
    # Per-story ANTITHESIS_* env overrides for the default-output command: a
    # string sets a var, None unsets it. Used by the doctor stories to model a
    # specific credential setup regardless of the operator's real environment.
    env: dict[str, str | None] | None = None
    # Run with the global config dir isolated to an empty throwaway
    # `$XDG_CONFIG_HOME`. `env` only controls ANTITHESIS_* *env* credentials, but
    # `snouty login` persists credentials to `credentials.toml` / `settings.toml`
    # under `$XDG_CONFIG_HOME/snouty` — those would otherwise leak in and mask a
    # story that models an unconfigured machine (e.g. the no-auth doctor stories).
    isolate_config: bool = False
    # The story starts real containers (a live `snouty validate` sample), as
    # opposed to the static checks that fail before any container starts. All
    # validate stories require a container runtime (see `ensure_validate_runtime`);
    # this flag just documents which ones spin up live containers.
    needs_docker: bool = False
    # -- TTY stories -------------------------------------------------------
    # A story with a `dialogue` is a TTY story: snouty is spawned on a real
    # pseudo-terminal and the dialogue is typed at it, one `(wait for, send)`
    # step per prompt. It runs in a throwaway `$HOME` so the credentials and
    # settings it persists never touch the operator's real config.
    dialogue: tuple[Step, ...] | None = None
    # Files to pre-write into the throwaway HOME before running, keyed by
    # HOME-relative path — models pre-existing state (a prior login, a broken
    # settings file). Mirrors the spec fixtures' txtar `-- path --` sections.
    seed_files: dict[str, str] | None = None
    # HOME-relative files to read back after the run and render under a
    # "Persisted state" section (secrets are redacted before embedding).
    post_capture: tuple[str, ...] = ()


@dataclass
class StoryRun:
    story: Story
    result: Result
    rows: list[dict] | None  # structured rows from the --json variant, if any
    help_result: Result | None = None  # `<help_cmd> --help` capture, for help stories
    sample_results: list[tuple[str, Result]] | None = None  # extra labelled captures
    # (rel_path, contents|None) for each `post_capture` file, in order; None means
    # the file was not written (which some TTY stories assert on).
    captured_files: list[tuple[str, str | None]] | None = None
    # TTY stories: the screen at each prompt, and the asciinema recording of the
    # whole session.
    frames: list[Frame] | None = None
    cast: str | None = None


class Registry:
    """Holds per-slug row counts so dependent checks (e.g. "narrowed vs the bare
    keyword search") can compare against an earlier story."""

    def __init__(self) -> None:
        self.row_counts: dict[str, int] = {}


# -- check factories --------------------------------------------------------


def non_empty_table(sr: StoryRun, reg: Registry) -> tuple[bool, str]:
    n = len(sr.rows or [])
    return (n > 0, f"{n} rows")


def rows_at_most(limit: int):
    def chk(sr: StoryRun, reg: Registry) -> tuple[bool, str]:
        n = len(sr.rows or [])
        return (1 <= n <= limit, f"{n} rows (limit {limit})")

    return chk


def rows_at_most_with_limit_note(limit: int):
    """`rows_at_most`, plus the stderr note that names the limit.

    The note claims more results may exist, so it belongs only when the server
    filled the limit. Fewer rows than asked for means the result set is
    exhausted, and the note must be absent.
    """

    def chk(sr: StoryRun, reg: Registry) -> tuple[bool, str]:
        n = len(sr.rows or [])
        noted = f"Showing up to {limit} results." in sr.result.stderr
        ok = 1 <= n <= limit and noted == (n == limit)
        return (ok, f"{n} rows (limit {limit}), note={noted}")

    return chk


def all_status(status: str):
    def chk(sr: StoryRun, reg: Registry) -> tuple[bool, str]:
        rows = sr.rows or []
        bad = [r.get("status") for r in rows if r.get("status") != status]
        return (bool(rows) and not bad, f"{len(rows)} rows, all status={status}")

    return chk


def properties_pass_and_fail(sr: StoryRun, reg: Registry) -> tuple[bool, str]:
    rows = sr.rows or []
    has_p = any(r.get("status") == "Passing" for r in rows)
    has_f = any(r.get("status") == "Failing" for r in rows)
    return (has_p and has_f, f"{len(rows)} props, passing={has_p} failing={has_f}")


def all_launcher(value: str):
    def chk(sr: StoryRun, reg: Registry) -> tuple[bool, str]:
        rows = sr.rows or []
        bad = [r for r in rows if r.get("launcher") != value]
        return (bool(rows) and not bad, f"{len(rows)} rows, all launcher={value!r}")

    return chk


def all_created_after(ts: str):
    def chk(sr: StoryRun, reg: Registry) -> tuple[bool, str]:
        rows = sr.rows or []
        lo = datetime.fromisoformat(ts)
        bad = [r for r in rows if datetime.fromisoformat(r["created_at"]) < lo]
        return (bool(rows) and not bad, f"{len(rows)} rows, all >= {ts}")

    return chk


def all_created_within(after: str, before: str):
    def chk(sr: StoryRun, reg: Registry) -> tuple[bool, str]:
        rows = sr.rows or []
        lo, hi = datetime.fromisoformat(after), datetime.fromisoformat(before)
        bad = [r for r in rows if not (lo <= datetime.fromisoformat(r["created_at"]) <= hi)]
        return (bool(rows) and not bad, f"{len(rows)} rows, all in [{after}, {before}]")

    return chk


def expect_message(*needles: str):
    def chk(sr: StoryRun, reg: Registry) -> tuple[bool, str]:
        text = sr.result.combined.lower()
        hit = [n for n in needles if n.lower() in text]
        return (bool(hit), f"matched {hit!r}" if hit else f"expected one of {needles!r}")

    return chk


def contains_all(*needles: str):
    def chk(sr: StoryRun, reg: Registry) -> tuple[bool, str]:
        text = sr.result.combined
        missing = [n for n in needles if n not in text]
        return (not missing, "all present" if not missing else f"missing {missing!r}")

    return chk


# -- login (interactive/stateful) check factories ---------------------------
#
# These read `sr.captured_files` (the `post_capture` files read back from the
# throwaway HOME) rather than stdout, because the interesting outcome of a
# stateful command is the file it wrote, not what it printed.


def _captured(sr: StoryRun, rel_path: str) -> str | None:
    for path, contents in sr.captured_files or []:
        if path == rel_path:
            return contents
    return None


def _shown(sr: StoryRun) -> str:
    """Everything a TTY story put on screen: every frame, not just the last one.
    A menu is erased once chosen, so its options survive only in the frame taken
    while it was up."""
    return "\n".join(screen for _, screen in sr.frames or [])


def tty_persisted(
    *,
    prompts: tuple[str, ...] = (),
    absent_prompts: tuple[str, ...] = (),
    files: tuple[tuple[str, tuple[str, ...]], ...] = (),
    absent_files: tuple[str, ...] = (),
    secrets_absent: tuple[str, ...] = (),
    expect_ok: bool = True,
):
    """Gate a TTY story. Combines the concerns one such story cares about: the
    command exits with the expected success/failure; the expected `prompts` all
    appear on screen and no `absent_prompts` do; each `(rel_path, needles)` in
    `files` was written and contains every needle; each path in `absent_files`
    was NOT written; and no raw `secrets_absent` value ever reached the screen.
    Any failure lists what went wrong."""

    def chk(sr: StoryRun, reg: Registry) -> tuple[bool, str]:
        text = _shown(sr)
        problems: list[str] = []

        if sr.result.ok != expect_ok:
            problems.append(f"exit={sr.result.returncode} (want ok={expect_ok})")

        missing_prompts = [p for p in prompts if p not in text]
        if missing_prompts:
            problems.append(f"missing prompts={missing_prompts!r}")
        shown_absent = [p for p in absent_prompts if p in text]
        if shown_absent:
            problems.append(f"unexpected prompts={shown_absent!r}")

        for rel_path, needles in files:
            contents = _captured(sr, rel_path)
            if contents is None:
                problems.append(f"{rel_path} not written")
                continue
            missing = [n for n in needles if n not in contents]
            if missing:
                problems.append(f"{rel_path} missing {missing!r}")

        for rel_path in absent_files:
            if _captured(sr, rel_path) is not None:
                problems.append(f"{rel_path} was written (expected absent)")

        leaked = [s for s in secrets_absent if s in text]
        if leaked:
            problems.append(f"secret reached the screen={leaked!r}")

        return (not problems, "; ".join(problems) or "prompts + persisted state as expected")

    return chk


def doctor_check(
    *,
    contains: tuple[str, ...],
    absent: tuple[str, ...] = (),
    ok: bool | None = None,
):
    """Gate a doctor story: every `contains` needle must appear, no `absent`
    needle may, and (when `ok` is given) the exit status must match. `ok` is left
    None for the api-key/legacy stories because their overall pass/fail also
    depends on the machine's container runtime — only the auth lines, which this
    asserts on, are deterministic."""

    def chk(sr: StoryRun, reg: Registry) -> tuple[bool, str]:
        text = sr.result.combined
        missing = [n for n in contains if n not in text]
        unexpected = [n for n in absent if n in text]
        exit_matches = ok is None or sr.result.ok == ok
        passed = not missing and not unexpected and exit_matches
        bits = []
        if missing:
            bits.append(f"missing={missing!r}")
        if unexpected:
            bits.append(f"unexpected={unexpected!r}")
        if not exit_matches:
            bits.append(f"exit_ok={sr.result.ok} (want {ok})")
        return (passed, "; ".join(bits) or "auth output as expected")

    return chk


def doctor_json_check(sr: StoryRun, reg: Registry) -> tuple[bool, str]:
    """Gate the `doctor --json` story: stdout must be a parseable report with a
    boolean `ok` and a non-empty `checks` array of well-formed records (name,
    status, message), it must include the api_key check, and a missing required
    credential must drive `ok` false."""
    try:
        data = json.loads(sr.result.stdout)
    except json.JSONDecodeError as e:
        return (False, f"stdout is not valid JSON: {e}")
    checks = data.get("checks")
    if not isinstance(data.get("ok"), bool) or not isinstance(checks, list) or not checks:
        return (False, f"missing ok/checks ({data!r:.80})")
    well_formed = all(
        isinstance(c.get("name"), str)
        and c.get("status") in ("ok", "warn", "error")
        and isinstance(c.get("message"), str)
        for c in checks
    )
    names = {c.get("name") for c in checks}
    passed = well_formed and "api_key" in names and data["ok"] is False
    return (passed, f"ok={data['ok']}, {len(checks)} checks, well_formed={well_formed}")


def verbose_api_calls(sr: StoryRun, reg: Registry) -> tuple[bool, str]:
    has_get = "> GET" in sr.result.stderr or "> GET" in sr.result.stdout
    has_table = "STATUS" in sr.result.combined or "RUN" in sr.result.combined.upper()
    return (has_get and has_table, f"api_calls={has_get} table={has_table}")


def event_multi_match(needle: str, second: str):
    """Both needles must appear in every returned row's raw JSON: the search
    backend ANDs them server-side. No count comparison against the
    single-needle story — that one runs on the GET events backend, whose
    curated haystack (output text, assertion messages, function names, test
    commands) is narrower than the raw JSON the search backend matches, so
    the two row counts are not comparable."""

    def chk(sr: StoryRun, reg: Registry) -> tuple[bool, str]:
        rows = sr.rows or []
        n1, n2 = needle.lower(), second.lower()
        bad = [r for r in rows if not (n1 in json.dumps(r).lower() and n2 in json.dumps(r).lower())]
        ok = bool(rows) and not bad
        return (ok, f"{len(rows)} rows, all contain both needles={not bad}")

    return chk


def event_keyword_present(keyword: str):
    def chk(sr: StoryRun, reg: Registry) -> tuple[bool, str]:
        rows = sr.rows or []
        present = keyword.lower() in sr.result.combined.lower()
        return (bool(rows) and present, f"{len(rows)} rows, keyword shown={present}")

    return chk


def property_has_examples(sr: StoryRun, reg: Registry) -> tuple[bool, str]:
    ok = _has_moment_rows(sr.result.combined)
    return (ok, "shows example moments" if ok else "no example moments (degenerate)")


def property_non_event_result(sr: StoryRun, reg: Registry) -> tuple[bool, str]:
    """A non-event property shows its value(s) under a `Result` label, with no
    per-moment HASH/VTIME rows (those belong to event properties' Examples)."""
    text = sr.result.combined
    has_result = _NONEVENT_RESULT.search(text) is not None
    no_moments = not _has_moment_rows(text)
    ok = has_result and no_moments
    return (ok, f"result={has_result}, no moments={no_moments}")


def _exit_with(*needles: str, want_ok: bool):
    """Story check: the command must exit with the expected success/failure AND
    every needle must appear in its output. Requiring the exit polarity stops a
    command that merely prints a needle (while exiting the other way) from
    passing as the wrong kind of story."""

    def chk(sr: StoryRun, reg: Registry) -> tuple[bool, str]:
        text = sr.result.combined
        missing = [n for n in needles if n not in text]
        ok = (sr.result.ok == want_ok) and not missing
        return (ok, f"exit={sr.result.returncode}, missing={missing!r}")

    return chk


def succeeds_with(*needles: str):
    """A clean-success story: exit zero AND every needle appears in the output."""
    return _exit_with(*needles, want_ok=True)


def fails_with(*needles: str):
    """A clean-error story: exit non-zero AND every needle appears in the output."""
    return _exit_with(*needles, want_ok=False)


def logs_non_empty(sr: StoryRun, reg: Registry) -> tuple[bool, str]:
    n = len(sr.rows or [])
    return (n > 0, f"{n} log lines")


def logs_begin_at(begin: str):
    def chk(sr: StoryRun, reg: Registry) -> tuple[bool, str]:
        rows = sr.rows or []
        if not rows:
            return (False, "no log lines")
        first = float(rows[0].get("moment", {}).get("vtime", rows[0].get("vtime", 0.0)))
        ok = first >= float(begin) - 1e-9
        return (ok, f"{len(rows)} lines, first vtime {first} >= {begin}")

    return chk


def help_story_check(sr: StoryRun, reg: Registry) -> tuple[bool, str]:
    """Gate a help story: the `--help` must be well-formed (exit 0, has a Usage
    line), any documented `align_tokens` must appear in BOTH the help and the
    default output (the "help matches output" guarantee), and a command that is
    supposed to print default output must actually print something. The
    subjective judgment — is the help clear/concise/consistent? — is left to the
    reviewer; this only stops a degenerate help/output pair from passing."""
    h = sr.help_result
    if h is None or not h.ok or "Usage:" not in h.combined:
        rc = "none" if h is None else h.returncode
        return (False, f"help missing or malformed (exit {rc})")
    help_text = h.combined
    story = sr.story

    # Help-only story (mutating command we don't run): just the help is enough.
    if not story.args:
        return (True, "help only")

    out = sr.result.combined
    if not out.strip():
        return (False, "default output is empty")

    # A non-zero exit means `out` is probably an error (auth/API failure) captured
    # as if it were the command's normal output — don't pass it off as a sample.
    # `doctor` opts out (expect_ok=False): it exits non-zero on a failed check yet
    # still prints representative output.
    if story.expect_ok and not sr.result.ok:
        return (False, f"default command failed (exit {sr.result.returncode})")

    if story.align_tokens:
        miss_help = [t for t in story.align_tokens if t not in help_text]
        miss_out = [t for t in story.align_tokens if t not in out]
        if miss_help or miss_out:
            return (False, f"misaligned — absent from help={miss_help} output={miss_out}")
        return (True, f"help + output aligned on {list(story.align_tokens)}")
    return (True, "help + default output present")


# ---------------------------------------------------------------------------
# Story definitions
# ---------------------------------------------------------------------------


# `doctor` never calls the API, so each doctor story drives one specific output
# with a controlled ANTITHESIS_* env, independent of the operator's shell. Each
# flag says whether that var should be *set* for the story: a set var inherits
# the operator's real value when present (so the stories reflect the real
# environment) and falls back to a placeholder otherwise, so the state still
# holds on a machine missing that credential. doctor only checks presence, never
# the value, so an inherited secret is never printed and a placeholder is purely
# a stand-in.
def _doctor_env(
    *, api_key: bool, username: bool, password: bool, tenant: bool, repo: bool
) -> dict[str, str | None]:
    def want(name: str, set_it: bool, placeholder: str) -> str | None:
        return (os.environ.get(name) or placeholder) if set_it else None

    return {
        "ANTITHESIS_API_KEY": want("ANTITHESIS_API_KEY", api_key, "demo-api-key"),
        "ANTITHESIS_USERNAME": want("ANTITHESIS_USERNAME", username, "demo-user"),
        "ANTITHESIS_PASSWORD": want("ANTITHESIS_PASSWORD", password, "demo-pass"),
        "ANTITHESIS_TENANT": want("ANTITHESIS_TENANT", tenant, "demo-tenant"),
        "ANTITHESIS_REPOSITORY": want(
            "ANTITHESIS_REPOSITORY", repo, "registry.example.com/acme/demo"
        ),
    }


# The doctor stories that assert a *reachable* API can't use `_doctor_env`'s
# synthesized creds — a placeholder tenant resolves to an unreachable host. They
# run against the operator's real credentials instead (gen-gallery already
# requires them; discovery hits the same API), dropping only any ambient legacy
# username/password so the API key is reported on its own.
def _reachable_doctor_env() -> dict[str, str | None]:
    return {"ANTITHESIS_USERNAME": None, "ANTITHESIS_PASSWORD": None}


def build_stories(d: Discovery) -> list[Story]:
    kw, kw2 = d.event_keyword, d.event_kw2
    # `--begin-vtime` for the logs skip-ahead story: just before the sampled
    # moment. A lower bound, so the float round-trip is harmless here.
    vmin = f"{max(0.0, float(d.event_vtime) - 0.5):.3f}"
    stories = [
        # -- listing --------------------------------------------------------
        Story(
            "runs",
            "Quickly check what test runs are around",
            "I just want to glance at what test runs exist without recalling any subcommands.",
            "A readable table of recent runs (id, status, title, time) appears — `runs` behaves like `runs list`.",
            ["runs"],
            non_empty_table,
        ),
        Story(
            "runs-list",
            "List recent runs to find one to inspect",
            "I want to scan recent runs and pick one to dig into.",
            "Up to 10 recent runs, newest first, with legible id/status/title/time columns; "
            "when more runs exist, a stderr note says the output stopped at the limit.",
            ["runs", "list", "-n", "10"],
            rows_at_most(10),
        ),
        Story(
            "runs-list--limit",
            "Show me just the last three runs",
            "I only care about the very latest handful of runs.",
            "At most 3 rows, the most recent ones; when more runs exist, a stderr note "
            "says the output stopped at the limit.",
            ["runs", "list", "-n", "3"],
            rows_at_most(3),
        ),
        Story(
            "runs-list--detail",
            "Get full descriptions instead of truncated titles",
            "Default titles are truncated; I want to read the full descriptions.",
            "Descriptions are shown in full (longer than the default view), one row per run.",
            ["runs", "list", "-n", "6", "--detail"],
            # --detail can't be combined with --json, so validate the rendered
            # key-value blocks directly rather than re-running for JSON rows. Check
            # the always-present title-case labels (the default table uses
            # UPPERCASE headers and no "Launcher"); "Description" is omitted when a
            # run has none, so requiring it would falsely fail on description-less
            # runs even though the detailed view rendered correctly.
            contains_all("Run ID", "Created", "Launcher"),
            json_capable=False,
        ),
        Story(
            "runs-list--status-completed",
            "Only show runs that finished cleanly",
            "I want to ignore in-flight/failed runs and see only completed ones.",
            "Every row has status=completed.",
            ["runs", "list", "-n", "8", "--status", "completed"],
            all_status("completed"),
        ),
        Story(
            "runs-list--status-incomplete",
            "Find recent failures to triage",
            "I'm triaging and want only runs that ended incomplete.",
            "Every row has status=incomplete.",
            ["runs", "list", "-n", "8", "--status", "incomplete"],
            all_status("incomplete"),
        ),
        Story(
            "runs-list--launcher",
            f"Show only {d.launcher}-launched runs",
            "I want to see only the runs kicked off by one particular launcher.",
            f"Non-empty, and every row's launcher is {d.launcher!r}.",
            ["runs", "list", "-n", "8", "--launcher", d.launcher],
            all_launcher(d.launcher),
        ),
        Story(
            "runs-list--created-after",
            "What runs have we kicked off recently?",
            "I want runs created on or after a given date.",
            f"Non-empty, and every row was created at/after {d.created_after}.",
            ["runs", "list", "--created-after", d.created_after],
            all_created_after(d.created_after),
        ),
        Story(
            "runs-list--created-window",
            "Look at runs from a specific window",
            "I want runs created within a specific time window.",
            f"Non-empty, and every row was created within [{d.window_after}, {d.window_before}].",
            [
                "runs",
                "list",
                "--created-after",
                d.window_after,
                "--created-before",
                d.window_before,
            ],
            all_created_within(d.window_after, d.window_before),
        ),
        Story(
            "runs-verbose",
            "See the API calls printed while you list runs",
            "I'm debugging and want to see the HTTP requests snouty makes.",
            "The run table prints, and stderr shows the `> GET` request lines (tokens redacted). "
            "This is a debugging flag: the FULL request AND response is intended output, "
            "including bulky response headers (e.g. content-security-policy) and long lines — "
            "do not treat header verbosity or line width here as a defect.",
            ["runs", "list", "-n", "3", "--verbose"],
            verbose_api_calls,
        ),
        # -- single-run metadata -------------------------------------------
        Story(
            "runs-show",
            "Peek at the metadata for a completed run",
            "I want the metadata for one specific run.",
            "Shows the run id, status, timestamps, launcher, and links.",
            ["runs", "show", d.success],
            contains_all(d.success, "completed"),
            json_capable=False,
        ),
        Story(
            "runs-show--web",
            "Jump straight to the triage report in the browser",
            "I want to open this run's triage report in my browser.",
            "Prints the report URL and exits cleanly (the browser is shimmed to a no-op here).",
            ["runs", "show", d.success, "--web"],
            succeeds_with("http"),
            json_capable=False,
        ),
        Story(
            "runs-show-incomplete",
            "Inspect a run that aborted early",
            "A run ended incomplete; I want to see where it died (failure vtime/hash).",
            "Status is incomplete and the failure moment (vtime/hash) is shown.",
            ["runs", "show", d.fail],
            # The rubric promises a failure moment; assert the real hash renders so
            # a run without one (the 0/0 sentinel) can't pass silently.
            contains_all("incomplete", d.fail_hash),
            json_capable=False,
        ),
        Story(
            "runs-show-cancelled",
            "What does a cancelled run look like?",
            "I want to see the metadata of a cancelled run.",
            "Status is shown as cancelled.",
            ["runs", "show", d.cancelled],
            contains_all("cancelled"),
            json_capable=False,
        ),
        Story(
            "runs-wait",
            "Block until a run reaches a terminal state",
            "I scripted a launch and want one command that blocks until the run finishes, "
            "instead of writing my own polling loop.",
            "A single final-status line once the run is terminal — immediately here, since "
            "this run is already completed.",
            ["runs", "wait", d.success],
            contains_all(d.success, "completed"),
            json_capable=False,
        ),
        # -- properties -----------------------------------------------------
        Story(
            "runs-properties",
            "See all properties — pass and fail",
            "I want the full property list for a completed run.",
            "A table with both passing and failing properties present.",
            ["runs", "properties", d.success],
            properties_pass_and_fail,
        ),
        Story(
            "runs-properties--passing",
            "List only the green properties",
            "I want to see only the properties that passed.",
            "Every row is a passing property.",
            ["runs", "properties", d.success, "--passing"],
            all_status("Passing"),
        ),
        Story(
            "runs-properties--failing",
            "Focus on the properties that broke",
            "I want to see only the properties that failed.",
            "Every row is a failing property.",
            ["runs", "properties", d.success, "--failing"],
            all_status("Failing"),
        ),
        Story(
            "runs-properties-incomplete",
            "Properties for a run that never finished",
            "I try to view properties on an incomplete run.",
            "A clean error explaining the properties aren't available (because the run is incomplete) — not a crash or stack trace.",
            ["runs", "properties", d.fail],
            # The friendly error (explain_properties_error in src/runs.rs) says the
            # run "is incomplete"; require that distinctive word. A bare "404"/"not
            # found" would mean the friendly message regressed, and "no properties"
            # also matches the success-path "No properties found." — so neither is
            # a safe needle here.
            expect_message("incomplete"),
            json_capable=False,
        ),
        # -- property detail (`properties --name <x> --detail`) -------------
        Story(
            "runs-properties-detail-failing",
            "Drill into a failing property's counter-examples",
            "A property failed; I want to see concrete counter-examples I can debug.",
            "Shows the property plus at least one counter-example with a moment (hash/vtime) — not an empty `unreachable`.",
            ["runs", "properties", d.success, "--name", d.fail_prop, "--detail"],
            property_has_examples,
            json_capable=False,
        ),
        Story(
            "runs-properties-detail-passing",
            "Look at the examples behind a passing property",
            "A property passed; I want to see example moments that satisfied it.",
            "Shows at least one example with a moment (hash/vtime).",
            ["runs", "properties", d.success, "--name", d.pass_event_prop, "--detail"],
            property_has_examples,
            json_capable=False,
        ),
        Story(
            "runs-properties-detail-non-event",
            "Detail a non-event property — its result value",
            "I want to inspect a non-event ('system') property, whose value is data rather than moments.",
            "Shows the value under a 'Result' label (scalar inline, or JSON for an object/array), with no per-moment hash/vtime rows.",
            ["runs", "properties", d.success, "--name", d.nonevent_prop, "--detail"],
            property_non_event_result,
            json_capable=False,
        ),
        Story(
            "runs-properties--name",
            "Filter the property list by name substring",
            "I want to narrow the property list to ones whose name matches a substring.",
            "Non-empty: every shown property's name contains the substring (case-insensitive).",
            ["runs", "properties", d.success, "--name", d.name_filter],
            non_empty_table,
        ),
        Story(
            "runs-properties--name-no-match",
            "A name filter that matches nothing",
            "I filter on a substring no property has; I want a friendly empty result.",
            "A clear 'No properties match' message — not an error or a crash.",
            ["runs", "properties", d.success, "--name", "this property does not exist"],
            expect_message("No properties match"),
            json_capable=False,
        ),
        # -- events ---------------------------------------------------------
        # Both searches pass the same explicit --limit, so the AND-narrowing
        # check can tell a genuinely smaller result from one the server capped.
        Story(
            "runs-events-single",
            f"Find events that mention '{kw}'",
            "I want to find events that mention a particular keyword.",
            f"At least one matching event row, and the keyword '{kw}' appears in the output. "
            "Each row is one `HASH VTIME SOURCE OUTPUT` line whose HASH and VTIME are "
            "copyable into `runs logs`. When more events match than the default limit of "
            "50, a stderr note says the output stopped at the limit.",
            ["runs", "events", d.success, "--match", kw],
            event_keyword_present(kw),
        ),
        Story(
            "runs-events-multi-match",
            "AND-narrow with two --match needles",
            "I want to narrow results to events that mention BOTH of two terms. "
            "Several terms route through the events-search API (the `runs-search` "
            "unstable feature), which ANDs them server-side against the raw event JSON.",
            f"At least one row, and every row's raw JSON contains both '{kw}' and '{kw2}'. "
            "When more events match than the default limit of 50, a stderr note says "
            "the output stopped at the limit.",
            ["runs", "events", d.success, "--match", kw, "--match", kw2],
            event_multi_match(kw, kw2),
            env={"SNOUTY_UNSTABLE_FEATURES": "runs-search"},
        ),
        Story(
            "runs-events-multi-gated",
            "Two --match needles without the events-search API",
            "I pass two terms but the `runs-search` feature is off; I want a clear "
            "refusal that names the fix, not a silent approximation.",
            "A clear error saying multiple terms require the events-search API, with the "
            "feature variable to set or the advice to search one term. Non-zero exit.",
            ["runs", "events", d.success, "--match", kw, "--match", kw2],
            expect_message("multiple search terms require the events-search API"),
            json_capable=False,
            expect_ok=False,
            env={"SNOUTY_UNSTABLE_FEATURES": None},
        ),
        Story(
            "runs-events-no-results",
            "Search events that don't match anything",
            "I search for a string that doesn't occur; I want a friendly empty result.",
            "A clear 'No events matched' message — not an error or a crash.",
            ["runs", "events", d.success, "--match", "this string will not appear anywhere"],
            expect_message("No events matched"),
            json_capable=False,
        ),
        Story(
            "runs-events-incomplete",
            "Search events on an incomplete run for failure context",
            "An incomplete run failed; I want events around the failure.",
            f"At least one event row matching '{d.fail_event_kw}' from the incomplete run.",
            ["runs", "events", d.fail, "--match", d.fail_event_kw],
            non_empty_table,
        ),
        # -- search (event-set DSL; gated behind the runs-search feature) ----
        Story(
            "runs-search-contains",
            f"Query events with the DSL: contains '{kw}'",
            "I want to run an event-set DSL query and read the matching events.",
            f"At least one matching event line, keyword '{kw}' visible. Each row is one "
            "`HASH VTIME SOURCE OUTPUT` line whose HASH and VTIME are copyable into "
            "`runs logs`; output should be legible without a table header. When more "
            "events match than the default limit of 50, a stderr note says the output "
            "stopped at the limit.",
            ["runs", "search", d.success, f'contains({{output_text: "{kw}"}})'],
            event_keyword_present(kw),
            env={"SNOUTY_UNSTABLE_FEATURES": "runs-search"},
        ),
        Story(
            "runs-search-limit",
            "Cap a DSL query at -n 3",
            "I want only the first few matches, not the whole stream — and I want to "
            "know whether the first few are all of them.",
            "Exactly at most 3 event lines, then the command exits promptly; a stderr "
            "note names the limit and says more results may be available.",
            ["runs", "search", d.success, f'contains({{output_text: "{kw}"}})', "-n", "3"],
            rows_at_most_with_limit_note(3),
            env={"SNOUTY_UNSTABLE_FEATURES": "runs-search"},
        ),
        Story(
            "runs-search-no-results",
            "A DSL query that matches nothing",
            "My query matches no events; I want a friendly empty result.",
            "A clear 'No events matched the query' message — not an error, not silence.",
            [
                "runs",
                "search",
                d.success,
                'contains({output_text: "this string will not appear anywhere"})',
            ],
            expect_message("No events matched the query"),
            json_capable=False,
            env={"SNOUTY_UNSTABLE_FEATURES": "runs-search"},
        ),
        Story(
            "runs-search-check-valid",
            "Validate a query without running it",
            "I want to check my query's syntax before running an expensive search.",
            "A clear 'query is valid' confirmation and exit 0.",
            ["runs", "search", d.success, 'contains({output_text: "x"})', "--check"],
            expect_message("query is valid"),
            json_capable=False,
            env={"SNOUTY_UNSTABLE_FEATURES": "runs-search"},
        ),
        Story(
            "runs-search-check-invalid",
            "Validate an invalid query",
            "My query has a bad verb; I want the validator to tell me what is wrong. "
            "(Known server limitation: current tenants answer an invalid query with a "
            "generic 500 instead of the documented 400, so judge how snouty presents "
            "that unhelpful answer.)",
            "A non-zero exit with an error a human can act on — at minimum, it must be "
            "clear the query (not snouty) is the likely problem.",
            ["runs", "search", d.success, 'bogus_verb({x: "y"})', "--check"],
            expect_message("API error"),
            json_capable=False,
            expect_ok=False,
            env={"SNOUTY_UNSTABLE_FEATURES": "runs-search"},
        ),
        Story(
            "runs-search-gated",
            "Invoke `runs search` while the feature is off",
            "I typed `runs search` without enabling the unstable feature; I want to "
            "learn how to enable it, not a bare 'unrecognized subcommand'.",
            "A clear refusal naming the feature and the exact variable to set, plus a "
            "pointer to `--help`. Non-zero exit.",
            ["runs", "search", d.success, 'contains({output_text: "x"})'],
            expect_message("unstable feature"),
            json_capable=False,
            expect_ok=False,
            env={"SNOUTY_UNSTABLE_FEATURES": None},
        ),
        # -- logs -----------------------------------------------------------
        Story(
            "runs-logs",
            "Stream logs at a specific moment",
            "I want the log lines at a particular moment of the run.",
            "At least one log line is streamed at/around the moment.",
            ["runs", "logs", d.success, d.event_hash, d.event_vtime],
            logs_non_empty,
        ),
        Story(
            "runs-logs-begin-vtime",
            "Skip ahead — start from a later moment",
            "I want to start streaming from a later moment instead of the root.",
            f"At least one line, and the stream starts at/after vtime {vmin} (not the root).",
            [
                "runs",
                "logs",
                d.success,
                d.event_hash,
                d.event_vtime,
                "--begin-vtime",
                vmin,
                "--begin-input-hash",
                d.event_hash,
            ],
            logs_begin_at(vmin),
        ),
        Story(
            "runs-logs-bad-moment",
            "Try logs with a moment that doesn't exist",
            "I ask for a moment that isn't in this run; I want a clean error.",
            "A clean error, not a crash or stack trace.",
            ["runs", "logs", d.success, "0", "999999.0"],
            expect_message("error", "not found", "no ", "invalid", "bad"),
            json_capable=False,
        ),
        Story(
            "runs-logs-incomplete",
            "Stream logs at the failure moment of an incomplete run",
            "I want the logs right at the moment an incomplete run failed.",
            "At least one log line at the failure moment.",
            ["runs", "logs", d.fail, d.fail_hash, d.fail_vtime],
            logs_non_empty,
        ),
        # -- build logs -----------------------------------------------------
        Story(
            "runs-build-logs",
            "Stream the build logs to see how a run was set up",
            "I want to see the build/setup logs for a run.",
            "At least one build-log line is streamed.",
            ["runs", "build-logs", d.success],
            logs_non_empty,
        ),
        Story(
            "runs-build-logs-unknown",
            "Wrong run ID — build-logs reports a clean error",
            "I pass a run id that doesn't exist; I want a clean error.",
            "A clean error, not a crash or stack trace.",
            ["runs", "build-logs", UNKNOWN_RUN],
            expect_message("error", "not found", "no ", "invalid"),
            json_capable=False,
        ),
        # -- doctor (one story per distinct auth output) --------------------
        Story(
            "doctor-api-key",
            "Confirm my environment is ready with an API key",
            "I've configured snouty with an API key and want doctor to confirm I'm set up.",
            "doctor reports the API key as set without mentioning username/password, and also "
            "contacts the API to confirm it's reachable and report the API and tenant versions.",
            ["doctor"],
            doctor_check(
                contains=("API key provided", "Antithesis API reachable"),
                absent=("ANTITHESIS_USERNAME", "ANTITHESIS_PASSWORD"),
            ),
            json_capable=False,
            env=_reachable_doctor_env(),
        ),
        Story(
            "doctor-offline",
            "Check my setup without touching the network",
            "I don't want snouty making any network calls; I just want to validate my local "
            "tooling and environment variables.",
            "doctor runs every local check but skips the API connectivity/version check entirely "
            "— there is no 'Antithesis API' line — and still reports the rest.",
            ["doctor", "--offline"],
            doctor_check(
                contains=("API key provided",),
                absent=("Antithesis API",),
            ),
            json_capable=False,
            env=_doctor_env(api_key=True, username=False, password=False, tenant=True, repo=True),
        ),
        Story(
            "doctor-api-unreachable",
            "The Antithesis API can't be reached",
            "I run doctor but the API host is unreachable (wrong tenant, blocked network, or the "
            "service is down); I want a clear failure fast, not a hang.",
            "doctor reports the API as unreachable and fails (non-zero exit), and it returns "
            "promptly — the connect timeout bounds a black-holed or unresolvable host rather than "
            "letting doctor hang.",
            ["doctor"],
            doctor_check(contains=("Antithesis API unreachable",), ok=False),
            json_capable=False,
            # Point the client at a reserved, unroutable address (RFC 5737
            # TEST-NET-1) so the connect attempt is black-holed.
            env={
                **_doctor_env(api_key=True, username=False, password=False, tenant=True, repo=True),
                "ANTITHESIS_BASE_URL": "http://192.0.2.1",
            },
        ),
        Story(
            "doctor-verbose",
            "See the API request doctor makes",
            "I'm debugging connectivity and want to see the exact request doctor sends to the "
            "Antithesis API.",
            "doctor's report prints, and stderr shows the `> GET .../api/version` request (auth "
            "token redacted) for the version check. This is a debugging flag: the full request "
            "AND response is intended output, including bulky response headers and long lines — "
            "do not treat header verbosity or line width here as a defect.",
            ["doctor", "--verbose"],
            doctor_check(contains=("> GET", "Antithesis API reachable")),
            json_capable=False,
            env=_reachable_doctor_env(),
        ),
        Story(
            "doctor-api-key-and-legacy",
            "Both an API key and leftover username/password are set",
            "I have an API key but also still have ANTITHESIS_USERNAME/PASSWORD exported; "
            "I want to know which one snouty uses.",
            "doctor reports only the API key (it takes precedence) and does not mention the "
            "legacy username/password at all.",
            ["doctor"],
            doctor_check(
                contains=("API key provided",),
                absent=("ANTITHESIS_USERNAME", "ANTITHESIS_PASSWORD"),
            ),
            json_capable=False,
            env=_doctor_env(api_key=True, username=True, password=True, tenant=True, repo=True),
        ),
        Story(
            "doctor-no-auth",
            "Fresh install — doctor tells me what to configure",
            "I just installed snouty and haven't set any credentials; I want doctor to tell me what I need.",
            "doctor states an API key is required and points ONLY at ANTITHESIS_API_KEY — it must not "
            "steer me toward username/password, which is legacy auth (issue #145).",
            ["doctor"],
            doctor_check(
                contains=(
                    "No Antithesis credentials found",
                    "requires an API key",
                    "ask Antithesis support",
                ),
                absent=("ANTITHESIS_USERNAME", "ANTITHESIS_PASSWORD"),
                ok=False,
            ),
            json_capable=False,
            env=_doctor_env(
                api_key=False, username=False, password=False, tenant=False, repo=False
            ),
            isolate_config=True,
        ),
        Story(
            "doctor-legacy-auth",
            "I only have a legacy username and password",
            "I authenticate with a username/password and no API key; I want doctor to tell me whether that's enough.",
            "doctor warns the API key is missing (so `snouty runs` and other API commands won't work), "
            "flags username/password as deprecated and limited to `snouty launch`/`snouty debug`, and "
            "steers me toward setting an API key.",
            ["doctor"],
            doctor_check(
                contains=(
                    "API key not provided",
                    "ANTITHESIS_USERNAME",
                    "deprecated",
                    "snouty launch",
                    "ask Antithesis support",
                ),
            ),
            json_capable=False,
            env=_doctor_env(api_key=False, username=True, password=True, tenant=True, repo=True),
        ),
        Story(
            "doctor-shadowed-credentials",
            "I ran `snouty login` but doctor still reports my old credentials",
            "I stored an API key with `snouty login`, but a username and password are still "
            "exported in my shell; I want to know why doctor keeps reporting the old ones.",
            "doctor warns that more than one credential source is available, names the credential "
            "it uses and where it comes from, names the stored credential it ignores, and states "
            "the action that hands the run to the stored one (issue #292).",
            ["doctor"],
            doctor_check(
                contains=(
                    "more than one credential source is available",
                    "snouty uses the username and password",
                    "snouty ignores the API key",
                    "unset [ANTITHESIS_USERNAME, ANTITHESIS_PASSWORD]",
                ),
            ),
            json_capable=False,
            env=_doctor_env(api_key=False, username=True, password=True, tenant=True, repo=True),
            isolate_config=True,
            seed_files={_CREDS: _SEED_CREDS_TOML},
        ),
        Story(
            "doctor-json",
            "Gate CI on a ready environment with --json",
            "I want to check my environment in a script/CI step and parse the result, "
            "not scrape human text.",
            "`doctor --json` prints a structured report — a top-level `ok` boolean and a `checks` "
            "array, each with name/status/message and any notes — and exits non-zero when a required "
            "check fails, so CI can gate on it.",
            ["doctor", "--json"],
            doctor_json_check,
            json_capable=False,
            env=_doctor_env(
                api_key=False, username=False, password=False, tenant=False, repo=False
            ),
            isolate_config=True,
        ),
    ]
    return stories + build_help_stories(d)


# ---------------------------------------------------------------------------
# Help stories: render each command's `--help` next to its default output, with
# rubrics that ask whether the help is informative, clear, concise, consistent,
# and aligned with what the command actually prints. Commands that mutate state
# (launch, debug, validate, update) or need an interactive arg (completions) are
# help-only — `args=[]` so nothing is executed.
# ---------------------------------------------------------------------------


# How a reviewer should judge every help story (shared rubric, kept in one place
# so the bar is consistent across commands).
HELP_RUBRIC = (
    "The `--help` should be **informative** (says what the command does and, for "
    "read commands, how to read the output and the obvious next command), "
    "**clear**, **concise** (no wall of text), and **consistent** with the other "
    "commands' help in tone, layout, and flag ordering. Where default output is "
    "shown, the help must **align** with it: any columns/fields the help names "
    "must actually appear, and nothing in the output should be unexplained."
)

_OUTPUT_RUBRIC = (
    " Compare the help against the default output shown below it."
)
_HELP_ONLY_RUBRIC = (
    " This command mutates state or needs an interactive argument, so only its "
    "help is shown — judge the help text on its own merits and for consistency "
    "with its siblings."
)


def _help_story(
    slug: str,
    title: str,
    goal: str,
    help_cmd: list[str],
    args: list[str] | None = None,
    *,
    samples: list[tuple[str, list[str]]] | None = None,
    align: tuple[str, ...] = (),
    expect_ok: bool = True,
) -> Story:
    args = args or []
    judge = HELP_RUBRIC + (_OUTPUT_RUBRIC if args else _HELP_ONLY_RUBRIC)
    return Story(
        slug=slug,
        title=title,
        goal=goal,
        judge=judge,
        args=args,
        check=help_story_check,
        json_capable=False,
        help_cmd=help_cmd,
        samples=samples,
        align_tokens=align,
        expect_ok=expect_ok,
    )


def build_help_stories(d: Discovery) -> list[Story]:
    s = d.success
    return [
        # -- top level + read commands with default output ------------------
        _help_story(
            "help-root",
            "Discover what snouty can do",
            "I just installed snouty and run `snouty --help` to see what's available.",
            [],
        ),
        _help_story(
            # The parent help is an overview/index of subcommands, not a column
            # legend (it points at `runs list` for the table), so no align tokens.
            "help-runs",
            "Understand the runs command group",
            "I run `snouty runs --help` to learn how to work with test runs.",
            ["runs"],
            ["runs"],
        ),
        _help_story(
            "help-runs-list",
            "Learn to list runs, including the detailed view",
            "I want to know what `runs list` shows and how the columns map to the output, "
            "including the fuller `--detail` view.",
            ["runs", "list"],
            ["runs", "list", "-n", "6"],
            samples=[("with --detail", ["runs", "list", "-n", "3", "--detail"])],
            align=("RUN ID", "STATUS", "CREATED", "TEST NAME"),
        ),
        _help_story(
            "help-runs-show",
            "Learn what `runs show` reports",
            "I want to confirm the help explains the metadata fields and the failure "
            "moment shown for incomplete runs.",
            ["runs", "show"],
            ["runs", "show", s],
            samples=[("an incomplete run (shows the failure moment)", ["runs", "show", d.fail])],
            # show prints a key/value card (prose help vs Title-Case labels), not a
            # columnar table — no strict token alignment; the reviewer compares the
            # prose field list and the failure-moment claim against the two samples.
        ),
        _help_story(
            "help-runs-wait",
            "Learn how to wait for a run to finish",
            "I want the help to explain the terminal states, that an `unknown` status "
            "fails the command, and what --poll-interval and --timeout do.",
            ["runs", "wait"],
            ["runs", "wait", s],
            # wait on a terminal run prints one status line; nothing columnar
            # to align tokens against.
        ),
        _help_story(
            "help-runs-properties",
            "Learn the properties table, filters, and --detail",
            "I want the help to explain the STATUS/EXAMPLES/NAME columns and the "
            "examples/counterexamples count, and the --name/--group/--detail flags "
            "(including how --detail feeds a moment into `runs logs`).",
            ["runs", "properties"],
            ["runs", "properties", s],
            samples=[
                ("--failing only", ["runs", "properties", s, "--failing"]),
                (
                    "--name <x> --detail (one property's moments)",
                    ["runs", "properties", s, "--name", d.pass_event_prop, "--detail"],
                ),
            ],
            align=("STATUS", "EXAMPLES", "NAME"),
        ),
        _help_story(
            "help-runs-events",
            "Learn to search events and chain into logs",
            "I want the help to explain the one-line `HASH VTIME SOURCE OUTPUT` format, "
            "that a moment feeds `runs logs`, and when multiple terms need the "
            "events-search feature.",
            ["runs", "events"],
            ["runs", "events", s, "--match", d.event_keyword],
        ),
        _help_story(
            "help-runs-search",
            "Learn the event-set DSL query command",
            "I want the help to explain the QUERY syntax (verbs), the unstable-feature "
            "gate and how to enable it, the mode switches, and the output line format. "
            "Help-only: the command is gated, so no default output is captured.",
            ["runs", "search"],
        ),
        _help_story(
            "help-runs-logs",
            "Learn what the positional moment vs --begin-vtime do",
            "I want the help to make clear that the positional moment streams logs up to "
            "it and --begin-vtime sets the start, and to describe the line format.",
            ["runs", "logs"],
            ["runs", "logs", s, d.event_hash, d.event_vtime],
        ),
        _help_story(
            "help-runs-build-logs",
            "Learn what build-logs streams",
            "I want the help to tell me this is the build/setup log and the line format.",
            ["runs", "build-logs"],
            ["runs", "build-logs", s],
        ),
        _help_story(
            "help-doctor",
            "Learn what doctor checks",
            "I run `snouty doctor --help` to see what it verifies, then run it.",
            ["doctor"],
            ["doctor"],
            # `doctor` exits non-zero when a check fails but still prints its
            # findings — that output is exactly what we want to show.
            expect_ok=False,
        ),
        _help_story(
            "help-version",
            "Check the version command's help",
            "I want `version --help` to be a clear, minimal description.",
            ["version"],
            ["version"],
        ),
        # -- help-only (mutating / interactive) -----------------------------
        _help_story(
            "help-launch",
            "Understand how to launch a run",
            "I run `snouty launch --help` to learn how to start a test run.",
            ["launch"],
        ),
        _help_story(
            "help-debug",
            "Understand how to open a debugging session",
            "I run `snouty debug --help` to learn how to debug a moment.",
            ["debug"],
        ),
        _help_story(
            "help-validate",
            "Understand local validation",
            "I run `snouty validate --help` to learn how to validate my config.",
            ["validate"],
        ),
        _help_story(
            "help-completions",
            "Generate shell completions",
            "I run `snouty completions --help` to learn how to install completions.",
            ["completions"],
        ),
        _help_story(
            "help-update",
            "Check for updates",
            "I run `snouty update --help` to understand what updating does.",
            ["update"],
        ),
        # -- docs (help-only: output depends on a downloaded docs DB) --------
        _help_story(
            "help-docs",
            "Understand the docs command group",
            "I run `snouty docs --help` to see how to search the documentation.",
            ["docs"],
        ),
        _help_story(
            "help-docs-search",
            "Learn to search the docs",
            "I run `snouty docs search --help` to learn the search syntax and output.",
            ["docs", "search"],
        ),
        _help_story(
            "help-docs-tree",
            "Learn to browse the docs tree",
            "I run `snouty docs tree --help` to learn how to browse documentation paths.",
            ["docs", "tree"],
        ),
        _help_story(
            "help-docs-show",
            "Learn to show a docs page",
            "I run `snouty docs show --help` to learn how to read a page.",
            ["docs", "show"],
        ),
        _help_story(
            "help-docs-sqlite",
            "Locate the docs database",
            "I run `snouty docs sqlite --help` to find the cached documentation DB.",
            ["docs", "sqlite"],
        ),
    ]


def resolve_compose_command() -> list[str] | None:
    """Resolve a Docker Compose v2 command: the standalone `docker-compose`
    binary or the `docker compose` plugin (snouty supports either). Returns the
    command as a list of argv tokens, or None if neither is a working Compose v2.

    A v1 `docker-compose` is rejected — its `version` banner reads
    `docker-compose version 1.x` (hyphenated), while v2 (binary or plugin) reads
    `Docker Compose version v2.x`, so requiring the un-hyphenated 'docker compose'
    substring accepts exactly Compose v2."""
    for cmd in (["docker-compose"], ["docker", "compose"]):
        try:
            ver = subprocess.run([*cmd, "version"], capture_output=True, text=True)
        except FileNotFoundError:
            continue
        if ver.returncode == 0 and "docker compose" in ver.stdout.lower():
            return cmd
    return None


def ensure_validate_runtime() -> None:
    """Verify a container runtime is ready for the validate stories and build the
    sample images, raising GalleryError if anything is missing.

    gen-gallery is a developer tool, so a container runtime is required — not
    optional. `snouty validate` resolves docker/podman before it even inspects a
    config, so *every* validate story (including the static misconfiguration
    checks) needs the runtime, and the live samples additionally need a running
    daemon to start containers. The live samples bake their test commands into
    images and `snouty validate` never builds or pulls, so the images must exist
    first — built here via scripts/build-validate-samples.sh."""
    try:
        if subprocess.run(["docker", "info"], capture_output=True).returncode != 0:
            raise GalleryError(
                "Docker daemon not reachable (`docker info` failed). gen-gallery "
                "is a developer tool and requires a running container runtime — "
                "start Docker and retry."
            )
    except FileNotFoundError as e:
        raise GalleryError(
            f"container runtime not found ({e.filename}): gen-gallery requires "
            "docker on PATH."
        ) from e
    if resolve_compose_command() is None:
        raise GalleryError(
            "Docker Compose v2 not available: gen-gallery needs either the "
            "`docker-compose` binary or the `docker compose` plugin (whichever is "
            "present must report Compose v2). Install one and retry."
        )
    print("building validate sample images…", file=sys.stderr)
    build = subprocess.run(
        ["bash", str(BUILD_SAMPLES_SCRIPT)], capture_output=True, text=True
    )
    if build.returncode != 0:
        tail = (build.stderr or build.stdout).strip()
        raise GalleryError(f"validate sample image build failed:\n{tail}")


def build_validate_stories(ephemeral: Path | None) -> list[Story]:
    """`snouty validate` stories, run against the committed sample projects under
    tests/fixtures/validate (each sample has its own README). The static
    misconfiguration samples fail fast (but still need the runtime resolved); the
    live ones (`needs_docker`) start real containers. All require a container
    runtime — `ensure_validate_runtime` enforces that before these run.

    Two degenerate inputs can't be committed — a non-existent directory and an
    empty `manifests/` dir (git can't track an empty directory) — so they're
    synthesized under `ephemeral` at run time. In `--list` mode `ephemeral` is
    None and they fall back to placeholder paths (their args are never run)."""
    s = SAMPLES_DIR
    missing_dir = s / "does-not-exist"  # deliberately never created
    if ephemeral is not None:
        empty_manifests = ephemeral / "empty-manifests"
        (empty_manifests / "manifests").mkdir(parents=True, exist_ok=True)
    else:
        empty_manifests = s / "empty-manifests"  # placeholder; not run for --list

    def v(slug, title, goal, judge, sample_args, check, *, needs_docker=False):
        return Story(
            slug=slug,
            title=title,
            goal=goal,
            judge=judge,
            args=["validate", *sample_args],
            check=check,
            json_capable=False,  # validate isn't a list/stream command
            needs_docker=needs_docker,
        )

    return [
        # -- static misconfigurations (detected before any container starts) --
        v(
            "validate-not-a-config",
            "Validate a directory that isn't a config",
            "I point validate at a directory with no compose file or manifests/.",
            "The error says no docker-compose.yaml or manifests/ was found, so I know what's missing.",
            [str(s / "neither")],
            fails_with("does not contain a docker-compose.yaml file or a manifests/ subdirectory"),
        ),
        v(
            "validate-missing-dir",
            "Validate a path that doesn't exist",
            "I mistype the path to my config directory.",
            "The error says the path is not a directory, rather than something cryptic.",
            [str(missing_dir)],
            fails_with("is not a directory"),
        ),
        v(
            "validate-wrong-extension",
            "Validate a .yml compose file",
            "My compose file is named docker-compose.yml instead of .yaml.",
            "The error names the wrong filename and suggests the exact rename.",
            [str(s / "wrong-extension")],
            fails_with("not the required docker-compose.yaml", "rename it to docker-compose.yaml"),
        ),
        v(
            "validate-ambiguous",
            "Validate a dir with both compose and manifests",
            "My directory has both a docker-compose.yaml and a manifests/ subdirectory.",
            "The error explains the ambiguity and says to provide one or the other.",
            [str(s / "ambiguous")],
            fails_with("contains both docker-compose.yaml and a manifests/ subdirectory"),
        ),
        v(
            "validate-empty-manifests",
            "Validate an empty manifests/ directory",
            "My manifests/ directory is empty.",
            "The error says the manifests/ dir is empty, not something obscure further along.",
            [str(empty_manifests)],
            fails_with("empty manifests/ subdirectory"),
        ),
        v(
            "validate-malformed-compose",
            "Validate a broken compose file",
            "My docker-compose.yaml has a YAML syntax error.",
            "The error shows compose config failed and includes the parser's message.",
            [str(s / "malformed-compose")],
            # snouty echoes the compose command it used, which differs between the
            # standalone binary (`docker-compose config`) and the plugin (`docker
            # compose config`); assert only the spelling-agnostic tail.
            fails_with("compose config' failed"),
        ),
        v(
            "validate-no-services",
            "Validate a compose file with no services",
            "My compose file declares no services.",
            "The error states plainly that no services were found.",
            [str(s / "no-services")],
            fails_with("no services found in docker-compose.yaml"),
        ),
        v(
            "validate-external-network",
            "Validate a compose file with an external network",
            "My compose file references an external network.",
            "The error explains an external network can't work on Antithesis.",
            [str(s / "external-network")],
            fails_with("declared as external"),
        ),
        v(
            "validate-missing-image",
            "Validate when a service image is missing locally",
            "A service references an image I haven't built or pulled.",
            "The error lists the missing image and reminds me snouty does not pull on its own.",
            [str(s / "missing-image")],
            fails_with("some images are not available locally"),
            needs_docker=True,
        ),
        # -- live container runs (require a Docker daemon) --------------------
        v(
            "validate-valid",
            "Validate a correct harness",
            "I validate a well-formed harness before launching it.",
            "Setup-complete is detected and the discovered test commands are summarized; exit is clean.",
            [str(s / "valid")],
            succeeds_with("Setup-complete event detected", "Setup validation successful"),
            needs_docker=True,
        ),
        v(
            "validate-timeout",
            "Validate a harness that never signals setup-complete",
            "My harness never emits the setup-complete event.",
            "snouty waits up to --timeout and then fails with a clear timeout message.",
            [str(s / "timeout"), "--timeout", "5"],
            fails_with("timed out waiting for setup-complete event"),
            needs_docker=True,
        ),
        v(
            "validate-unrecognized-command",
            "Validate a harness with an unknown test command",
            "One of my test commands has a name with no recognized prefix.",
            "Discovery fails and the offending command is named.",
            [str(s / "unrecognized-command")],
            fails_with("test command discovery failed", "Unrecognized command names"),
            needs_docker=True,
        ),
        v(
            "validate-non-executable-command",
            "Validate a harness with a non-executable test command",
            "One of my test commands is missing its executable bit.",
            "Discovery fails and the non-executable command is named.",
            [str(s / "non-executable-command")],
            fails_with("test command discovery failed", "are not executable"),
            needs_docker=True,
        ),
        v(
            "validate-stranded",
            "Validate a harness whose init container exits",
            "A one-shot container holds test commands but exits during startup.",
            "snouty warns those commands are stranded but still validates successfully.",
            [str(s / "stranded")],
            succeeds_with("their containers exited", "Setup validation successful"),
            needs_docker=True,
        ),
    ]


# ---------------------------------------------------------------------------
# TTY stories: `snouty login` holds a conversation on a terminal and writes
# settings.toml/credentials.toml as it goes. Each story runs in a throwaway
# `$HOME`, types a scripted dialogue at a real pseudo-terminal, and captures both
# the screens and the files written, so a reviewer judges the conversation AND
# its result. No API, discovery, or container runtime is needed. On Linux the
# keychain is a no-op, so credentials land in the file backend — the realistic
# default here.
#
# The tenant is contacted for real: `snouty login` asks it whether single
# sign-on is available before it draws the credential menu, so the menu a story
# shows is the menu that tenant gives. That is the point — the gallery shows
# what a human would see. It also means the menu's contents are not fixed, which
# is why `pick` finds its row on screen instead of counting keypresses.
# ---------------------------------------------------------------------------

# Fake, obviously-not-real secrets typed at the prompts — never a real
# credential, and redacted again before embedding (see `_redact_secrets`).
_FAKE_KEY = "antithesis_api_key_v2_NOTAREALKEY_7Qxz"
_FAKE_PASS = "FAKE-not-a-real-password"
_SEED_KEY = "antithesis_api_key_v2_NOTAREALSEED_9Pgw"
_TENANT = "acme"
_REPO = "registry.example.com/acme/app"
_SETTINGS = ".config/snouty/settings.toml"
_CREDS = ".config/snouty/credentials.toml"
# A `credentials.toml` exactly as `snouty login` writes it.
_SEED_CREDS_TOML = f'[default]\ntype = "ApiKey"\napi_key = "{_SEED_KEY}"\n'

# Prompts the dialogues wait for, and the credential-menu labels they choose
# between (these match the `Display` impl on snouty's `AuthSetupType`).
_ASK_TENANT = "What Antithesis tenant"
_ASK_REPO = "What container repository"
_ASK_CREDENTIALS = "What kind of credentials"
_ASK_KEY = "Please enter your API Key"
_ASK_USERNAME = "What username"
_ASK_PASSWORD = "Please enter your password"
_API_KEY = "API Key"
_USERNAME_PASSWORD = "Username & password (deprecated)"

# Shared satisfaction rubric for the TTY stories: judge the conversation AND the
# persisted result, not just the exit code.
_TTY_RUBRIC = (
    "Judge the prompt conversation and the persisted state together. Are the "
    "prompts clear, in a sensible order, and only for values not already known? "
    "After it finishes, does the user know WHAT was saved, WHERE, and the next "
    "step (e.g. `snouty doctor`)? Are secrets never echoed? For the error/repair "
    "paths, is the message clear about what went wrong and how to recover, and is "
    "a mutating overwrite done safely (warn + back up)?"
)


def _tty_story(
    slug: str,
    title: str,
    goal: str,
    args: list[str],
    dialogue: tuple[Step, ...],
    check,
    *,
    seed_files: dict[str, str] | None = None,
    post_capture: tuple[str, ...] = (_SETTINGS, _CREDS),
) -> Story:
    return Story(
        slug=slug,
        title=title,
        goal=goal,
        judge=_TTY_RUBRIC,
        args=args,
        check=check,
        json_capable=False,
        dialogue=dialogue,
        seed_files=seed_files,
        post_capture=post_capture,
    )


def build_tty_stories() -> list[Story]:
    return [
        _tty_story(
            "login-fresh-apikey",
            "First-time setup with an API key",
            "I just installed snouty and want to configure my tenant, repository, and API key.",
            ["login"],
            (
                (_ASK_TENANT, _TENANT + ENTER),
                (_ASK_REPO, _REPO + ENTER),
                pick(_ASK_CREDENTIALS, _API_KEY),
                (_ASK_KEY, _FAKE_KEY + ENTER),
            ),
            tty_persisted(
                prompts=(_ASK_TENANT, _ASK_REPO, _ASK_CREDENTIALS),
                files=(
                    (_SETTINGS, (f'tenant = "{_TENANT}"', f'repository = "{_REPO}"')),
                    (_CREDS, ('type = "ApiKey"', f'api_key = "{_FAKE_KEY}"')),
                ),
                secrets_absent=(_FAKE_KEY,),
            ),
        ),
        _tty_story(
            "login-reuse-default",
            "Re-run login and keep my stored values",
            "I already logged in; re-running should offer my previous tenant, repository, and key "
            "back so I can just hit enter.",
            ["login"],
            (
                (_ASK_TENANT, ENTER),
                (_ASK_REPO, ENTER),
                pick(_ASK_CREDENTIALS, _API_KEY),
                (_ASK_KEY, ENTER),
            ),
            tty_persisted(
                prompts=(_TENANT, _REPO),
                files=(
                    (_SETTINGS, (f'tenant = "{_TENANT}"', f'repository = "{_REPO}"')),
                    # Every answer was a bare Enter, so the stored key must survive.
                    (_CREDS, ('type = "ApiKey"', f'api_key = "{_SEED_KEY}"')),
                ),
                secrets_absent=(_SEED_KEY,),
            ),
            seed_files={
                _SETTINGS: f'tenant = "{_TENANT}"\nrepository = "{_REPO}"\n',
                _CREDS: _SEED_CREDS_TOML,
            },
        ),
        _tty_story(
            "login-password",
            "Set up deprecated username/password auth",
            "I authenticate with a username and password rather than an API key.",
            ["login"],
            (
                (_ASK_TENANT, _TENANT + ENTER),
                (_ASK_REPO, _REPO + ENTER),
                pick(_ASK_CREDENTIALS, _USERNAME_PASSWORD),
                (_ASK_USERNAME, "puser" + ENTER),
                (_ASK_PASSWORD, _FAKE_PASS + ENTER),
            ),
            tty_persisted(
                prompts=(_ASK_CREDENTIALS, _USERNAME_PASSWORD),
                files=((_CREDS, ('type = "Password"', 'username = "puser"')),),
                secrets_absent=(_FAKE_PASS,),
            ),
        ),
    ]


# ---------------------------------------------------------------------------
# Rendering + main
# ---------------------------------------------------------------------------


# Default-output samples in a help story are capped — the point there is to see
# the *shape* of the output next to the help, not the full stream. The
# goal-based stories (above) still capture full, untruncated output.
HELP_SAMPLE_MAX_LINES = 18


def _command_line(args: list[str]) -> str:
    """`snouty <args>` as a line a reader can paste into a shell.

    Arguments are quoted where a shell would need it: several stories pass a
    value that contains spaces (`--launcher 'Basic Test git'`, a multi-word
    `--match` needle), and joining on a bare space would show a command that
    means something different from the one that ran."""
    return f"snouty {shlex.join(args)}"


def _shell_block(args: list[str], text: str, returncode: int, cap: int | None = None) -> str:
    """A ```shell block showing `$ snouty <args>`, its output, and the exit code
    on the line below — so a reviewer can judge whether the return code makes
    sense given the output (e.g. a clean error should be non-zero; a healthy
    listing should be zero). When `cap` is set the output is truncated to that
    many lines with a marker (help-story samples cap; full goal output does not)."""
    lines = text.rstrip("\n").split("\n")
    if cap is not None and len(lines) > cap:
        hidden = len(lines) - cap
        lines = lines[:cap] + [f"… ({hidden} more lines)"]
    body = "\n".join(lines)
    return f"```shell\n$ {_command_line(args)}\n{body}\n```\nExit code: `{returncode}`"


def _write_help_story(out_dir: Path, story: Story, sr: StoryRun, verdict: str, detail: str) -> None:
    help_text = (sr.help_result.combined if sr.help_result else "").rstrip("\n")
    help_rc = sr.help_result.returncode if sr.help_result else 0
    parts = [
        f"# {story.title}",
        f"**User goal:** {story.goal}",
        f"**Judge satisfaction by:** {story.judge}",
        "## Help text",
        _shell_block([*(story.help_cmd or []), "--help"], help_text, help_rc),
    ]
    if story.args:
        parts.append("## Default output")
        parts.append(
            _shell_block(
                story.args, sr.result.combined, sr.result.returncode, cap=HELP_SAMPLE_MAX_LINES
            )
        )
        for label, res in sr.sample_results or []:
            parts.append(f"### Variant: {label}")
            parts.append(_shell_block(res.args, res.combined, res.returncode, cap=HELP_SAMPLE_MAX_LINES))
    parts.append(f"_Automated check: {verdict} — {detail}_")
    (out_dir / f"{story.slug}.md").write_text("\n\n".join(parts) + "\n")


# Redact the secret values from persisted TOML before embedding it in a story —
# belt-and-suspenders on top of the fake secrets the TTY stories type.
_SECRET_LINE = re.compile(r'^(\s*(?:api_key|password)\s*=\s*)"[^"]*"', re.MULTILINE)


def _redact_secrets(text: str) -> str:
    return _SECRET_LINE.sub(r'\1"[REDACTED]"', text)


def _write_tty_story(out_dir: Path, story: Story, sr: StoryRun, verdict: str, detail: str) -> None:
    # One frame per prompt, so a reviewer sees the screen the user faced at each
    # decision — including the menus, which are erased once chosen and so appear
    # nowhere in the closing screen. The keys typed are not listed separately:
    # `inquire` draws each answer next to its prompt, and the frames carry that.
    parts = [
        f"# {story.title}",
        f"**User goal:** {story.goal}",
        f"**Judge satisfaction by:** {story.judge}",
        "## Conversation",
        f"```shell\n$ {_command_line(story.args)}\n```",
    ]
    for prompt, screen in sr.frames or []:
        parts.append(f"### At `{prompt}`\n```\n{screen}\n```")
    parts.append(f"Exit code: `{sr.result.returncode}`")
    if sr.cast is not None:
        cast_name = f"{story.slug}.cast"
        (out_dir / cast_name).write_text(sr.cast)
        parts.append(f"_Replay the session: `asciinema play {cast_name}`_")
    if story.post_capture:
        parts.append("## Persisted state")
        for rel_path, contents in sr.captured_files or []:
            if contents is None:
                parts.append(f"`~/{rel_path}` — _(not written)_")
            else:
                body = _redact_secrets(contents).rstrip("\n")
                parts.append(f"`~/{rel_path}`\n```toml\n{body}\n```")
    parts.append(f"_Automated check: {verdict} — {detail}_")
    (out_dir / f"{story.slug}.md").write_text("\n\n".join(parts) + "\n")


def write_story(out_dir: Path, story: Story, sr: StoryRun, passed: bool, detail: str) -> None:
    verdict = "PASS" if passed else "FAIL"
    if story.help_cmd is not None:
        _write_help_story(out_dir, story, sr, verdict, detail)
        return
    if story.dialogue is not None:
        _write_tty_story(out_dir, story, sr, verdict, detail)
        return
    md = (
        f"# {story.title}\n\n"
        f"**User goal:** {story.goal}\n\n"
        f"**Judge satisfaction by:** {story.judge}\n\n"
        f"{_shell_block(story.args, sr.result.combined, sr.result.returncode)}\n\n"
        f"_Automated check: {verdict} — {detail}_\n"
    )
    (out_dir / f"{story.slug}.md").write_text(md)


def _write_seed_files(root: Path, seed_files: dict[str, str] | None) -> None:
    """Pre-write a story's `seed_files` under `root`, creating parent dirs."""
    for rel_path, contents in (seed_files or {}).items():
        dest = root / rel_path
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(contents)


def run_tty_story(sn: Snouty, story: Story) -> StoryRun:
    """Run a TTY story in a throwaway `$HOME` so the credentials and settings it
    persists never touch the operator's real config. The home is seeded with
    `seed_files`, the dialogue is typed at a pseudo-terminal, and the
    `post_capture` files are read back before the home is removed."""
    # A short prefix on purpose: this path shows up inside snouty's own output,
    # and a long one would push those lines over the terminal's width for a
    # reason that belongs to the harness rather than to snouty.
    home = Path(tempfile.mkdtemp(prefix="snouty-tty."))

    try:
        _write_seed_files(home, story.seed_files)

        # Pin HOME, clear XDG_CONFIG_HOME (snouty treats empty as unset), and
        # drop any ambient ANTITHESIS_* credentials, so the story shows the state
        # it seeded rather than the operator's own and no real secret can reach a
        # captured file. TERM names a terminal `inquire` can draw on.
        env = sn.env_with(
            {
                "HOME": str(home),
                "TERM": "xterm-256color",
                "XDG_CONFIG_HOME": None,
                **{k: None for k in os.environ if k.startswith("ANTITHESIS_")},
                **(story.env or {}),
            }
        )
        result, frames, cast = drive_tty(sn.binary, story.args, env, story.dialogue or ())

        captured: list[tuple[str, str | None]] = []
        for rel_path in story.post_capture:
            path = home / rel_path
            captured.append((rel_path, path.read_text() if path.is_file() else None))
        return StoryRun(
            story, result, None, captured_files=captured, frames=frames, cast=cast
        )
    finally:
        shutil.rmtree(home, ignore_errors=True)


def _run_isolated_story(sn: Snouty, story: Story) -> StoryRun:
    """Run a story with the global config dir pointed at a throwaway home, so no
    persisted `snouty login` credentials (`credentials.toml` / `settings.toml`)
    leak in. The home starts empty, which models an unconfigured machine;
    `seed_files` writes the persisted state a story needs. `$XDG_CONFIG_HOME`
    points at `<home>/.config`, so a seed path means the same here as in
    `run_tty_story`. The `--json` rows aren't captured (`json_lines` can't take
    an env override, and the isolated stories — all `doctor` stories — validate
    on rendered text anyway)."""
    home = Path(tempfile.mkdtemp(prefix="snouty-gallery-config."))
    try:
        _write_seed_files(home, story.seed_files)

        env = {**(story.env or {}), "XDG_CONFIG_HOME": str(home / ".config")}
        result = sn.run(story.args, env)
        return StoryRun(story, result, None)
    finally:
        shutil.rmtree(home, ignore_errors=True)


def run_story(sn: Snouty, story: Story) -> StoryRun:
    if story.dialogue is not None:
        return run_tty_story(sn, story)
    if story.isolate_config:
        return _run_isolated_story(sn, story)
    # Help-only stories pass no `args`; don't invoke a bare `snouty`.
    result = sn.run(story.args, story.env) if story.args else Result([], "", "", 0)
    rows = None
    if story.json_capable and story.args:
        try:
            rows = sn.json_lines(story.args, story.env)
        except (GalleryError, json.JSONDecodeError):
            rows = None  # error stories are validated on rendered text instead

    help_result = None
    sample_results = None
    if story.help_cmd is not None:
        help_result = sn.run([*story.help_cmd, "--help"])
        if story.samples:
            sample_results = [(label, sn.run(a)) for label, a in story.samples]
    return StoryRun(story, result, rows, help_result, sample_results)


def main() -> int:
    repo_root = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description="Regenerate the snouty gallery.")
    parser.add_argument("--snouty", type=Path, help="snouty binary (default: target/debug/snouty)")
    parser.add_argument(
        "--build",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="cargo build before running (default: yes)",
    )
    parser.add_argument("--out", type=Path, help="output dir (default: a fresh tempdir)")
    parser.add_argument(
        "--runs-to-scan",
        type=int,
        default=15,
        help="recent completed runs to probe for one with events",
    )
    parser.add_argument(
        "--only",
        nargs="+",
        metavar="SLUG",
        help="only generate stories matching these slugs (globs allowed, e.g. 'login-*')",
    )
    parser.add_argument("--list", action="store_true", help="list story slugs and exit")
    parser.add_argument("--fail-fast", action="store_true", help="stop at the first failing story")
    args = parser.parse_args()

    if args.list:
        # An all-default Discovery is enough to enumerate slugs (build_stories
        # only reads a few fields, and event_vtime defaults to a real vtime).
        for s in build_stories(Discovery()):
            print(s.slug)
        for s in build_validate_stories(None):
            print(s.slug)
        for s in build_tty_stories():
            print(s.slug)
        return 0

    snouty_bin = args.snouty
    if snouty_bin is None:
        if args.build:
            print("building snouty (target/debug)…", file=sys.stderr)
            if subprocess.run(["cargo", "build", "-q"], cwd=repo_root).returncode != 0:
                print("error: cargo build failed", file=sys.stderr)
                return 1
        snouty_bin = repo_root / "target" / "debug" / "snouty"
    snouty_bin = snouty_bin.resolve()
    if not snouty_bin.exists():
        print(f"error: snouty binary not found: {snouty_bin}", file=sys.stderr)
        return 1
    print(f"using binary: {snouty_bin}", file=sys.stderr)

    out_dir = args.out or Path(tempfile.mkdtemp(prefix="snouty-gallery."))
    out_dir.mkdir(parents=True, exist_ok=True)

    # The uncommittable validate fixtures (a non-existent dir, an empty
    # manifests/ dir) are synthesized here for this run only.
    fixtures_dir = Path(tempfile.mkdtemp(prefix="snouty-gallery-fixtures."))

    # Which story groups does this run actually need? The TTY stories need no
    # API, discovery, or container runtime, so `--only login-*` must not drag in
    # (and fail on) live-API discovery or a Docker daemon. Decide up front from
    # cheaply-enumerable slugs which groups are in scope.
    def selected(slug: str) -> bool:
        return not args.only or any(fnmatch.fnmatch(slug, pat) for pat in args.only)

    api_slugs = {s.slug for s in build_stories(Discovery())}
    validate_slugs = {s.slug for s in build_validate_stories(None)}
    tty_slugs = {s.slug for s in build_tty_stories()}
    need_api = any(selected(s) for s in api_slugs)
    need_validate = any(selected(s) for s in validate_slugs)

    if args.only:
        known = api_slugs | validate_slugs | tty_slugs
        unmatched = [p for p in args.only if not any(fnmatch.fnmatch(s, p) for s in known)]
        if unmatched:
            print(f"error: --only matched no stories: {unmatched}", file=sys.stderr)
            return 1

    sn = Snouty(snouty_bin)
    failures: list[tuple[str, str]] = []
    try:
        stories: list[Story] = []
        if need_api:
            disc = discover(sn, args.runs_to_scan)
            stories += build_stories(disc)

        if need_validate:
            # Validate stories run against the committed sample projects, all of
            # which require a container runtime (`snouty validate` resolves
            # docker/podman before inspecting any config). gen-gallery is a
            # developer tool, so the runtime is mandatory: build the sample images
            # and hard-fail if it isn't available, rather than silently dropping
            # stories.
            ensure_validate_runtime()
            validate_stories = build_validate_stories(fixtures_dir)
            n_live = sum(s.needs_docker for s in validate_stories)
            print(
                f"including all {len(validate_stories)} validate stories "
                f"({n_live} start live containers)",
                file=sys.stderr,
            )
            stories += validate_stories

        # TTY stories need no external dependencies, so they always run.
        stories += build_tty_stories()

        if args.only:
            stories = [s for s in stories if selected(s.slug)]

        # Capture concurrently (subprocess + API roundtrips dominate), preserving
        # story order in the results list. Checks are then evaluated serially in
        # that order; no check currently depends on another story's registry
        # entry, but the ordered evaluation keeps that option open.
        print(f"capturing {len(stories)} stories…", file=sys.stderr)
        with ThreadPoolExecutor(max_workers=CAPTURE_WORKERS) as pool:
            captured = list(pool.map(lambda s: run_story(sn, s), stories))

        reg = Registry()
        for sr in captured:
            story = sr.story
            if sr.rows is not None:
                reg.row_counts[story.slug] = len(sr.rows)
            passed, detail = story.check(sr, reg)
            write_story(out_dir, story, sr, passed, detail)
            mark = "ok  " if passed else "FAIL"
            print(f"  {mark} {story.slug:<32} {detail}", file=sys.stderr)
            if not passed:
                failures.append((story.slug, detail))
                if args.fail_fast:
                    # Everything was already captured; just stop reporting here.
                    break
    except GalleryError as e:
        # A precondition could not be met. Fail loudly and clearly rather than
        # emitting a partial gallery or dumping a traceback.
        print(f"\nerror: {e}", file=sys.stderr)
        return 1
    finally:
        sn.cleanup()
        shutil.rmtree(fixtures_dir, ignore_errors=True)

    print(file=sys.stderr)
    if failures:
        print(f"{len(failures)} story/stories failed their check:", file=sys.stderr)
        for slug, detail in failures:
            print(f"  - {slug}: {detail}", file=sys.stderr)
        print(f"\ngallery written to:\n{out_dir}", file=sys.stderr)
        return 1
    print("all stories passed their checks", file=sys.stderr)
    print("gallery written to:", file=sys.stderr)
    print(out_dir)
    return 0


if __name__ == "__main__":
    sys.exit(main())
