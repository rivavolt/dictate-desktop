#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Audit STT model availability across dictate-desktop's providers.

For each provider it reports, against the model list the daemon currently exposes
(read live from ../src/config.rs so this never drifts from the code):

  AVAILABLE            (ok)   model the daemon lists and the provider still serves
  DAEMON-LISTED / DEAD (dead) model the daemon lists but the provider rejects (decommissioned)
  NEW                  (new)  model the provider serves that the daemon does not list yet

Run it whenever you're about to revisit the Model menu / config::ALL_MODELS.

Keys: pulled from the running daemon's environment on the workstation (riva by
default) over SSH and injected into this process's env. Values are NEVER printed,
logged, or written anywhere — only the presence of each NAME is shown.

Usage:
  scripts/check-models.py                 # pull keys from riva's daemon, probe all
  scripts/check-models.py --host watts    # pull keys from a different host's daemon
  scripts/check-models.py --local         # use keys already in this shell's env
  scripts/check-models.py --provider groq # restrict to one provider
  scripts/check-models.py --no-color
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.request
import wave
from dataclasses import dataclass, field
from pathlib import Path

PROVIDERS = ("assemblyai", "deepgram", "groq", "fireworks")
KEY_ENV = {
    "assemblyai": "ASSEMBLYAI_API_KEY",
    "deepgram": "DEEPGRAM_API_KEY",
    "groq": "GROQ_API_KEY",
    "fireworks": "FIREWORKS_API_KEY",
}

REPO_ROOT = Path(__file__).resolve().parent.parent
CONFIG_RS = REPO_ROOT / "src" / "config.rs"

# Extra model ids to probe per provider beyond the daemon's list, so the report can
# surface NEW models on providers that have no clean "list models" API (Deepgram).
# Probing a name that doesn't exist returns 400/403, so a 200 here means a real,
# usable model the daemon simply doesn't expose. Keep this list current with each
# provider's published catalogue.
DEEPGRAM_PROBE_EXTRA = [
    "nova-3-general", "nova-3-medical",
    "nova-2-meeting", "nova-2-phonecall", "nova-2-finance", "nova-2-conversationalai",
    "nova-2-voicemail", "nova-2-video", "nova-2-medical", "nova-2-drivethru",
    "nova", "enhanced", "base",
    "whisper-base",
]


def color_enabled(flag: str) -> bool:
    if flag == "always":
        return True
    if flag == "never":
        return False
    return sys.stdout.isatty()


class C:
    enabled = True

    @classmethod
    def wrap(cls, code: str, s: str) -> str:
        return f"\033[{code}m{s}\033[0m" if cls.enabled else s


def green(s):  return C.wrap("32", s)
def red(s):    return C.wrap("31", s)
def yellow(s): return C.wrap("33", s)
def cyan(s):   return C.wrap("36", s)
def dim(s):    return C.wrap("2", s)
def bold(s):   return C.wrap("1", s)


# ---------------------------------------------------------------------------
# Daemon's current model list — parsed from src/config.rs (single source of truth).
# ---------------------------------------------------------------------------

def parse_daemon_models() -> dict[str, list[str]]:
    """Read provider_models() out of src/config.rs so the audit always reflects
    exactly what the daemon ships, not a copy that can rot."""
    if not CONFIG_RS.is_file():
        die(f"cannot find {CONFIG_RS} — run this from inside the repo")
    text = CONFIG_RS.read_text()

    # Grab the body of `pub fn provider_models(...) { match provider { ... } }`.
    m = re.search(r"pub fn provider_models\([^)]*\)\s*->\s*[^{]*\{(.*?)\n\}", text, re.S)
    if not m:
        die("could not locate provider_models() in src/config.rs")
    body = m.group(1)

    models: dict[str, list[str]] = {}
    # Each arm looks like:  "groq" => &["whisper-large-v3-turbo", "whisper-large-v3", ...],
    for prov, arr in re.findall(r'"([a-z]+)"\s*=>\s*&\[([^\]]*)\]', body):
        ids = re.findall(r'"([^"]+)"', arr)
        if ids:
            models[prov] = ids
    if not models:
        die("parsed src/config.rs but found no provider model lists")
    return models


# ---------------------------------------------------------------------------
# Key extraction — names only ever surface, never values.
# ---------------------------------------------------------------------------

def load_keys_from_host(host: str) -> dict[str, str]:
    """SSH into `host`, read the dictate-desktop daemon process environ, and return
    the provider API keys. The values stay inside this process; we only print which
    NAMES were found."""
    names = "|".join(KEY_ENV.values())
    remote = (
        'pid=$(pgrep -f "dictate-desktop daemon" | head -1); '
        '[ -z "$pid" ] && { echo "__NO_DAEMON__" >&2; exit 3; }; '
        f"tr '\\0' '\\n' < /proc/$pid/environ | grep -E '^({names})='"
    )
    try:
        out = subprocess.run(
            ["ssh", host, remote],
            capture_output=True, text=True, timeout=30, check=False,
        )
    except FileNotFoundError:
        die("ssh not found on PATH")
    except subprocess.TimeoutExpired:
        die(f"ssh {host} timed out while reading the daemon environment")

    if out.returncode == 3 or "__NO_DAEMON__" in out.stderr:
        die(f"no 'dictate-desktop daemon' process found on {host} — is it running?")
    if out.returncode != 0:
        die(f"ssh {host} failed: {out.stderr.strip() or 'unknown error'}")

    keys: dict[str, str] = {}
    rev = {v: k for k, v in KEY_ENV.items()}
    for line in out.stdout.splitlines():
        if "=" not in line:
            continue
        name, _, value = line.partition("=")
        prov = rev.get(name.strip())
        if prov and value:
            keys[prov] = value
    return keys


def load_keys_local() -> dict[str, str]:
    return {p: os.environ[v] for p, v in KEY_ENV.items() if os.environ.get(v)}


# ---------------------------------------------------------------------------
# HTTP helper — returns (status, body_text). Never raises on HTTP errors.
# ---------------------------------------------------------------------------

# Groq's edge (and some others) 403 the default "Python-urllib/x" User-Agent, so we
# always present a normal one — without it Groq's /models endpoint is unreachable.
USER_AGENT = "dictate-desktop-model-audit/1.0"


def http(method: str, url: str, headers: dict[str, str],
         data: bytes | None = None, timeout: int = 30) -> tuple[int, str]:
    headers = {"User-Agent": USER_AGENT, **headers}
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")
    except urllib.error.URLError as e:
        return 0, f"<network error: {e.reason}>"
    except Exception as e:  # noqa: BLE001
        return 0, f"<error: {e}>"


def http_retry(method: str, url: str, headers: dict[str, str],
               data: bytes | None = None, timeout: int = 30,
               retries: int = 3) -> tuple[int, str]:
    """http() with a few retries on transient throttling (Groq in particular hands
    back a brief 403/429 under burst). Backs off 1s, 2s, 4s."""
    import time
    st, body = http(method, url, headers, data, timeout)
    attempt = 0
    while st in (403, 429, 500, 502, 503) and attempt < retries:
        time.sleep(2 ** attempt)
        attempt += 1
        st, body = http(method, url, headers, data, timeout)
    return st, body


def silent_wav() -> bytes:
    """0.2 s of 16 kHz mono silence — the cheapest possible probe payload for the
    sync transcription endpoints."""
    import io
    buf = io.BytesIO()
    w = wave.open(buf, "wb")
    w.setnchannels(1)
    w.setsampwidth(2)
    w.setframerate(16000)
    w.writeframes(b"\x00\x00" * 3200)
    w.close()
    return buf.getvalue()


def multipart(fields: dict[str, str], filename: str, filedata: bytes) -> tuple[bytes, str]:
    boundary = "----dictateModelProbe"
    parts: list[bytes] = []
    for k, v in fields.items():
        parts.append(
            f'--{boundary}\r\nContent-Disposition: form-data; name="{k}"\r\n\r\n{v}\r\n'.encode()
        )
    parts.append(
        f'--{boundary}\r\nContent-Disposition: form-data; name="file"; filename="{filename}"\r\n'
        f"Content-Type: audio/wav\r\n\r\n".encode()
    )
    parts.append(filedata)
    parts.append(f"\r\n--{boundary}--\r\n".encode())
    return b"".join(parts), f"multipart/form-data; boundary={boundary}"


# ---------------------------------------------------------------------------
# Result model
# ---------------------------------------------------------------------------

@dataclass
class Row:
    model: str
    status: str            # "ok" | "dead" | "new" | "unknown"
    note: str = ""


@dataclass
class ProviderReport:
    provider: str
    method: str            # how it was determined (live api / per-model probe / docs)
    rows: list[Row] = field(default_factory=list)
    caveat: str = ""       # set when we could NOT enumerate live


# ---------------------------------------------------------------------------
# Per-provider probes
# ---------------------------------------------------------------------------

def check_groq(key: str, listed: list[str]) -> ProviderReport:
    """Groq exposes an OpenAI-style GET /models. We treat that list as the source
    of available audio models, then transcribe-probe any listed-but-missing model to
    capture the exact decommission reason."""
    rep = ProviderReport("groq", "GET /openai/v1/models (live) + transcribe probe")
    status, body = http_retry("GET", "https://api.groq.com/openai/v1/models",
                              {"Authorization": f"Bearer {key}"})
    if status != 200:
        rep.caveat = f"models endpoint returned HTTP {status}; could not enumerate live"
        for m in listed:
            rep.rows.append(Row(m, "unknown", "not verified (list API unavailable)"))
        return rep

    try:
        ids = {m.get("id") for m in json.loads(body).get("data", [])}
    except json.JSONDecodeError:
        rep.caveat = "models endpoint returned non-JSON; could not enumerate live"
        return rep

    audio_ids = {i for i in ids if i and any(
        k in i.lower() for k in ("whisper", "asr", "transcrib", "speech"))}

    wav = silent_wav()
    for m in listed:
        if m in audio_ids:
            rep.rows.append(Row(m, "ok"))
        else:
            # Listed but absent from the live catalogue → confirm with a real call.
            data, ctype = multipart(
                {"model": m, "response_format": "json"}, "probe.wav", wav)
            st, b = http("POST", "https://api.groq.com/openai/v1/audio/transcriptions",
                        {"Authorization": f"Bearer {key}", "Content-Type": ctype}, data)
            reason = extract_msg(b) or f"HTTP {st}"
            rep.rows.append(Row(m, "dead", reason))

    for m in sorted(audio_ids - set(listed)):
        rep.rows.append(Row(m, "new", "in /models, not in daemon list"))
    return rep


def check_deepgram(key: str, listed: list[str]) -> ProviderReport:
    """Deepgram has no public 'list STT models' API, so we probe each candidate
    against the sync /v1/listen endpoint with a silent clip and language=en. A real
    model returns 200 (and echoes metadata.model_info); a bad name returns 400/403.
    We also re-probe with language=multi to flag the known multilingual gap."""
    rep = ProviderReport("deepgram", "per-model probe of /v1/listen (live)")
    wav = silent_wav()

    def probe(model: str, lang: str) -> tuple[int, str]:
        url = (f"https://api.deepgram.com/v1/listen?model={model}"
               f"&language={lang}&smart_format=true")
        return http("POST", url,
                    {"Authorization": f"Token {key}", "Content-Type": "audio/wav"}, wav)

    def model_label(body: str) -> str:
        try:
            info = json.loads(body).get("metadata", {}).get("model_info", {})
            for v in info.values():
                arch = v.get("arch", "")
                name = v.get("name", "")
                ver = v.get("version", "")
                return f"{name} [{arch}] {ver}".strip()
        except Exception:  # noqa: BLE001
            pass
        return ""

    listed_labels: set[str] = set()  # resolved model_info labels of the daemon's models
    for m in listed:
        st, body = probe(m, "en")
        if st == 200:
            label = model_label(body)
            if label:
                listed_labels.add(label)
            note = label
            # Whisper-family models reject language=multi (known issue) — verify.
            st_multi, body_multi = probe(m, "multi")
            if st_multi != 200:
                why = extract_msg(body_multi) or f"HTTP {st_multi}"
                note = (f"{label}; " if label else "") + f"language=multi rejected ({why})"
            rep.rows.append(Row(m, "ok", note))
        else:
            reason = extract_msg(body) or f"HTTP {st}"
            rep.rows.append(Row(m, "dead", reason))

    # Surface NEW models from the curated probe list (skip ones already listed). A
    # candidate whose resolved label matches a listed model is just an alias (e.g.
    # nova-3-general == nova-3), so we tag it rather than present it as a fresh option.
    seen = set(listed)
    for m in DEEPGRAM_PROBE_EXTRA:
        if m in seen:
            continue
        seen.add(m)
        st, body = probe(m, "en")
        if st == 200:
            label = model_label(body)
            alias = " (alias of a listed model)" if label in listed_labels and label else ""
            rep.rows.append(Row(m, "new", (label + alias).strip()))
        seen.add(m)
    return rep


def check_fireworks(key: str, listed: list[str]) -> ProviderReport:
    """Fireworks: try the OpenAI-style models list and the control-plane list. With
    the dictation key these have historically 401'd (account streaming-only, no batch
    entitlement). If so we cannot enumerate live and say so explicitly."""
    rep = ProviderReport("fireworks", "GET model-list endpoints (live)")
    endpoints = [
        ("https://api.fireworks.ai/inference/v1/models", "inference v1/models"),
        ("https://audio-prod.api.fireworks.ai/v1/audio/models", "audio-prod v1/audio/models"),
    ]
    enumerated: set[str] | None = None
    statuses = []
    for url, label in endpoints:
        st, body = http("GET", url, {"Authorization": f"Bearer {key}"})
        statuses.append(f"{label}=HTTP {st}")
        if st == 200:
            try:
                data = json.loads(body)
                items = data.get("data") or data.get("models") or []
                ids = {it.get("id") or it.get("name") for it in items if isinstance(it, dict)}
                ids = {i.split("/")[-1] for i in ids if i}
                enumerated = {i for i in ids if any(
                    k in i.lower() for k in ("whisper", "asr", "transcrib", "speech"))} or ids
            except Exception:  # noqa: BLE001
                enumerated = None
            if enumerated:
                rep.method = f"GET {label} (live)"
                break

    if enumerated is None:
        rep.caveat = ("no model-list endpoint authorized for this key ("
                      + ", ".join(statuses)
                      + "); account is streaming-only (batch/list APIs 401). "
                      "Listed models below are from the daemon's source, NOT verified live.")
        for m in listed:
            rep.rows.append(Row(m, "unknown", "streaming-only key; not verifiable via REST"))
        return rep

    for m in listed:
        rep.rows.append(Row(m, "ok" if m in enumerated else "dead",
                            "" if m in enumerated else "not in live catalogue"))
    for m in sorted(enumerated - set(listed)):
        rep.rows.append(Row(m, "new", "in live catalogue, not in daemon list"))
    return rep


def check_assemblyai(key: str, listed: list[str]) -> ProviderReport:
    """AssemblyAI exposes no STT-model listing API (the v2/v3 model paths 404). We
    confirm the key is live (a cheap authenticated GET) and report the daemon's model
    set with a dated, docs-sourced note rather than pretending to enumerate."""
    rep = ProviderReport("assemblyai", "docs + key-liveness check (no model-list API)")
    st, _ = http("GET", "https://api.assemblyai.com/v2/transcript?limit=1",
                 {"Authorization": key})
    live = " (key authenticates)" if st == 200 else f" (key check returned HTTP {st})"
    rep.caveat = ("AssemblyAI has no model-list API (v2/v3 model paths 404), so models "
                  "cannot be enumerated live" + live + ". The daemon's 'universal' label maps "
                  "to Universal-Streaming (u3-rt-pro, plus whisper-rt for wide-language auto) "
                  "for live and Universal-2 for batch; both current as of 2026-06.")
    for m in listed:
        rep.rows.append(Row(m, "ok", "current per docs (not API-verified)"))
    return rep


def extract_msg(body: str) -> str:
    """Pull a human error string out of a provider error body."""
    try:
        j = json.loads(body)
    except json.JSONDecodeError:
        return body.strip()[:160]
    for path in (("error", "message"), ("error",), ("err_msg",), ("message",)):
        cur = j
        ok = True
        for k in path:
            if isinstance(cur, dict) and k in cur:
                cur = cur[k]
            else:
                ok = False
                break
        if ok and isinstance(cur, str):
            return cur.strip()[:200]
    return body.strip()[:160]


# ---------------------------------------------------------------------------
# Rendering
# ---------------------------------------------------------------------------

BADGE = {
    "ok":      lambda: green("AVAILABLE"),
    "dead":    lambda: red("DEAD"),
    "new":     lambda: cyan("NEW"),
    "unknown": lambda: yellow("UNKNOWN"),
}
MARK = {"ok": lambda: green("ok "), "dead": lambda: red("XX "),
        "new": lambda: cyan(" +"), "unknown": lambda: yellow(" ?")}


def render(reports: list[ProviderReport]) -> None:
    print(bold("\nSTT model availability audit — dictate-desktop"))
    print(dim(f"daemon model list parsed from {CONFIG_RS.relative_to(REPO_ROOT)}\n"))

    totals = {"ok": 0, "dead": 0, "new": 0, "unknown": 0}
    for rep in reports:
        print(bold(f"== {rep.provider} ") + dim(f"({rep.method})"))
        if rep.caveat:
            print("   " + yellow("! " + rep.caveat))
        if not rep.rows:
            print("   " + dim("(no models)"))
        width = max((len(r.model) for r in rep.rows), default=0)
        for r in sorted(rep.rows, key=lambda x: ({"ok": 0, "dead": 1, "unknown": 2, "new": 3}[x.status], x.model)):
            totals[r.status] += 1
            mark = MARK[r.status]()
            badge = BADGE[r.status]()
            note = dim("  " + r.note) if r.note else ""
            print(f"   {mark} {r.model.ljust(width)}  {badge}{note}")
        print()

    print(bold("summary: ")
          + green(f"{totals['ok']} available")
          + "  " + red(f"{totals['dead']} dead")
          + "  " + cyan(f"{totals['new']} new")
          + ("  " + yellow(f"{totals['unknown']} unverified") if totals["unknown"] else ""))
    print(dim("AVAILABLE = daemon lists it & provider serves it · "
              "DEAD = daemon lists it but provider rejects it · "
              "NEW = provider serves it, daemon doesn't list it yet\n"))


def die(msg: str) -> None:
    print(red("error: ") + msg, file=sys.stderr)
    sys.exit(1)


def main() -> None:
    ap = argparse.ArgumentParser(description="Audit STT model availability per provider.")
    ap.add_argument("--host", default="riva",
                    help="host whose running daemon env holds the API keys (default: riva)")
    ap.add_argument("--local", action="store_true",
                    help="use API keys from this shell's environment instead of SSH")
    ap.add_argument("--provider", choices=PROVIDERS, action="append",
                    help="limit to one or more providers (repeatable)")
    ap.add_argument("--color", choices=("auto", "always", "never"), default="auto")
    ap.add_argument("--no-color", action="store_const", const="never", dest="color")
    args = ap.parse_args()

    C.enabled = color_enabled(args.color)

    daemon_models = parse_daemon_models()
    keys = load_keys_local() if args.local else load_keys_from_host(args.host)

    found = sorted(keys.keys())
    src = "this shell's env" if args.local else f"{args.host}'s daemon env"
    if found:
        print(dim(f"keys found in {src}: {', '.join(found)} (values not shown)"))
    else:
        die(f"no provider API keys found in {src}")

    wanted = args.provider or list(PROVIDERS)
    checkers = {
        "groq": check_groq,
        "deepgram": check_deepgram,
        "fireworks": check_fireworks,
        "assemblyai": check_assemblyai,
    }

    reports: list[ProviderReport] = []
    for prov in wanted:
        listed = daemon_models.get(prov, [])
        if prov not in keys:
            rep = ProviderReport(prov, "skipped")
            rep.caveat = f"no {KEY_ENV[prov]} available — skipped"
            for m in listed:
                rep.rows.append(Row(m, "unknown", "not checked (no key)"))
            reports.append(rep)
            continue
        reports.append(checkers[prov](keys[prov], listed))

    render(reports)

    # Exit non-zero if any daemon-listed model is dead, so the script is CI-friendly.
    if any(r.status == "dead" for rep in reports for r in rep.rows):
        sys.exit(2)


if __name__ == "__main__":
    main()
