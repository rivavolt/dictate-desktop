#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Benchmark dictate-desktop's STT providers/models on a sample of real recordings.

A development aid — re-run it whenever the provider/model set changes — that answers
two questions for every model the daemon lists (read live from ../src/config.rs, so
this never drifts from the code):

  latency   median and p90 wall-clock per clip
  accuracy  word error rate against the AssemblyAI/universal transcript as reference,
            on a normalized comparison (lowercase, strip punctuation, collapse spaces)

It reuses the daemon's actual file-transcription request shapes (see
src/{assemblyai,groq,fireworks,deepgram}.rs), lang=auto, no custom vocabulary, fillers
kept — i.e. what the daemon sends for a plain dictation with default settings.

Keys: pulled from the running daemon's environment on the workstation (riva by
default) over SSH and injected into this process's env. Values are NEVER printed,
logged, or written anywhere — only the presence of each NAME is shown. (--local uses
the keys already in this shell's env instead.)

Sample: a duration-stratified sample of recordings from the host's audio dir
(riva:~/.local/state/dictate-desktop/audio). The clip list comes from `find -printf`
(NOT ls — it's aliased to an icon-lister that corrupts paths); the chosen clips are
copied locally and cached under ~/.cache/dictate-bench so re-runs are cheap.

Usage:
  scripts/benchmark.py                       # pull keys from riva, sample ~12 clips, all models
  scripts/benchmark.py --sample-size 24      # larger sample
  scripts/benchmark.py --provider groq       # restrict to one provider (repeatable)
  scripts/benchmark.py --model nova-2        # restrict to one model id (repeatable)
  scripts/benchmark.py --host watts          # pull keys + clips from another host
  scripts/benchmark.py --local               # use keys from this shell's env
  scripts/benchmark.py --json                # emit machine-readable results instead of the table
"""

from __future__ import annotations

import argparse
import concurrent.futures
import io
import json
import os
import random
import re
import statistics
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
import uuid
from dataclasses import dataclass, field
from pathlib import Path

PROVIDERS = ("assemblyai", "deepgram", "groq", "fireworks")
KEY_ENV = {
    "assemblyai": "ASSEMBLYAI_API_KEY",
    "deepgram": "DEEPGRAM_API_KEY",
    "groq": "GROQ_API_KEY",
    "fireworks": "FIREWORKS_API_KEY",
}

# The reference model every other model's WER is measured against. AssemblyAI's
# Universal batch model is the daemon's accuracy leader, so its transcript is the
# ground-truth stand-in (there are no hand-labels for the user's own recordings).
REFERENCE_PROVIDER = "assemblyai"
REFERENCE_MODEL = "universal"
REFERENCE_LABEL = f"{REFERENCE_PROVIDER}/{REFERENCE_MODEL}"

REPO_ROOT = Path(__file__).resolve().parent.parent
CONFIG_RS = REPO_ROOT / "src" / "config.rs"

REMOTE_AUDIO_DIR = "~/.local/state/dictate-desktop/audio"
CACHE_DIR = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "dictate-bench"

# Per-provider concurrency. Groq's and Fireworks' free/dev tiers throttle hard, so they
# get a small gate; AssemblyAI (async jobs) and Deepgram tolerate more in flight.
CONCURRENCY = {"assemblyai": 6, "deepgram": 6, "groq": 2, "fireworks": 2}

# Lang=auto, matching the daemon's default. AssemblyAI restricts auto-detection to these
# expected languages (see src/assemblyai.rs transcribe_file); the others send no language.
EXPECTED_LANGUAGES = ["en", "fr"]

POLL_INTERVAL_S = 0.5
POLL_MAX_TRIES = 240  # ~120 s ceiling, matching the daemon's 120 * 500 ms


# ---------------------------------------------------------------------------
# Colour
# ---------------------------------------------------------------------------

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


def color_enabled(flag: str) -> bool:
    if flag == "always":
        return True
    if flag == "never":
        return False
    return sys.stdout.isatty()


def die(msg: str) -> None:
    print(red("error: ") + msg, file=sys.stderr)
    sys.exit(1)


def log(*a) -> None:
    print(*a, file=sys.stderr)


# ---------------------------------------------------------------------------
# Daemon's current model list — parsed from src/config.rs (single source of truth).
# (Same parser as scripts/check-models.py, so the two never disagree.)
# ---------------------------------------------------------------------------

def parse_daemon_models() -> dict[str, list[str]]:
    if not CONFIG_RS.is_file():
        die(f"cannot find {CONFIG_RS} — run this from inside the repo")
    text = CONFIG_RS.read_text()
    m = re.search(r"pub fn provider_models\([^)]*\)\s*->\s*[^{]*\{(.*?)\n\}", text, re.S)
    if not m:
        die("could not locate provider_models() in src/config.rs")
    body = m.group(1)
    models: dict[str, list[str]] = {}
    for prov, arr in re.findall(r'"([a-z]+)"\s*=>\s*&\[([^\]]*)\]', body):
        ids = re.findall(r'"([^"]+)"', arr)
        if ids:
            models[prov] = ids
    if not models:
        die("parsed src/config.rs but found no provider model lists")
    return models


# ---------------------------------------------------------------------------
# Key extraction — names only ever surface, never values. (Mirrors check-models.py.)
# ---------------------------------------------------------------------------

def load_keys_from_host(host: str) -> dict[str, str]:
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
# Sample selection + caching.
# ---------------------------------------------------------------------------

@dataclass
class Clip:
    name: str            # basename, e.g. 20260307-183057.flac
    size: int            # bytes (from find -printf, used to stratify)
    local: Path          # cached local path
    duration_s: float = 0.0


def list_remote_clips(host: str) -> list[tuple[str, int]]:
    """Return [(basename, size_bytes)] for every recording, via `find -printf` (never ls,
    which is aliased to an icon-lister that corrupts paths)."""
    remote = (
        f"find {REMOTE_AUDIO_DIR} -maxdepth 1 -type f "
        r"\( -name '*.flac' -o -name '*.wav' -o -name '*.ogg' -o -name '*.mp3' \) "
        r"-printf '%s\t%f\n'"
    )
    out = subprocess.run(["ssh", host, remote], capture_output=True, text=True,
                         timeout=60, check=False)
    if out.returncode != 0:
        die(f"ssh {host} failed listing {REMOTE_AUDIO_DIR}: "
            f"{out.stderr.strip() or 'unknown error'}")
    clips: list[tuple[str, int]] = []
    for line in out.stdout.splitlines():
        if "\t" not in line:
            continue
        size_s, _, name = line.partition("\t")
        try:
            clips.append((name.strip(), int(size_s)))
        except ValueError:
            continue
    return clips


def stratified_pick(clips: list[tuple[str, int]], n: int, seed: int) -> list[tuple[str, int]]:
    """Pick n clips spread across the duration range. File size at a fixed FLAC bitrate is a
    faithful proxy for duration, so we sort by size, cut into n equal-count buckets, and draw
    one clip from each — giving short, medium and long clips rather than n similar ones."""
    if not clips:
        return []
    if len(clips) <= n:
        return list(clips)
    rng = random.Random(seed)
    ordered = sorted(clips, key=lambda c: c[1])
    picked: list[tuple[str, int]] = []
    for i in range(n):
        lo = i * len(ordered) // n
        hi = max(lo + 1, (i + 1) * len(ordered) // n)
        picked.append(rng.choice(ordered[lo:hi]))
    return picked


def fetch_clips(host: str, chosen: list[tuple[str, int]]) -> list[Clip]:
    """Copy chosen clips into the local cache (skipping ones already cached at the right
    size), then resolve each clip's real duration. Returns ready-to-use Clip records."""
    host_cache = CACHE_DIR / host
    host_cache.mkdir(parents=True, exist_ok=True)

    to_fetch = []
    for name, size in chosen:
        local = host_cache / name
        if not (local.is_file() and local.stat().st_size == size):
            to_fetch.append(name)

    if to_fetch:
        log(dim(f"fetching {len(to_fetch)} clip(s) from {host} into {host_cache} "
                f"({len(chosen) - len(to_fetch)} already cached)…"))
        # Stream the chosen files as one tar over a single SSH connection — independent of
        # the number of files and free of the ~/brace-expansion quoting pitfalls scp hits.
        # NUL-delimited names go in over stdin so odd filenames stay intact; `tar -T -`
        # reads the list, `-C` roots it at the audio dir.
        names_nul = "".join(n + "\0" for n in to_fetch).encode()
        remote = f"tar -C {REMOTE_AUDIO_DIR} --null -T - -cf - 2>/dev/null"
        cp = subprocess.run(
            ["ssh", host, remote],
            input=names_nul, capture_output=True, timeout=300, check=False)
        if cp.returncode != 0 or not cp.stdout:
            err = cp.stderr.decode("utf-8", "replace").strip() if cp.stderr else ""
            die(f"fetching clips from {host} failed: {err or 'empty tar stream'}")
        import tarfile
        with tarfile.open(fileobj=io.BytesIO(cp.stdout), mode="r|") as tf:
            tf.extractall(host_cache, filter="data")
    else:
        log(dim(f"all {len(chosen)} clip(s) already cached in {host_cache}"))

    out: list[Clip] = []
    for name, size in chosen:
        local = host_cache / name
        if not local.is_file():
            log(yellow(f"! missing after fetch, skipping: {name}"))
            continue
        out.append(Clip(name=name, size=size, local=local, duration_s=audio_duration(local)))
    return out


def audio_duration(path: Path) -> float:
    """Best-effort clip duration in seconds, parsed straight from the file header (no deps).
    FLAC carries it in STREAMINFO; WAV in the fmt/data chunks. Returns 0.0 if unknown — the
    benchmark still runs, only the per-clip duration column is blank."""
    try:
        suf = path.suffix.lower()
        if suf == ".flac":
            return _flac_duration(path)
        if suf == ".wav":
            import wave
            with wave.open(str(path), "rb") as w:
                return w.getnframes() / float(w.getframerate() or 1)
    except Exception:  # noqa: BLE001
        pass
    return 0.0


def _flac_duration(path: Path) -> float:
    with open(path, "rb") as fh:
        if fh.read(4) != b"fLaC":
            return 0.0
        # Walk metadata blocks; STREAMINFO (type 0) holds sample rate + total samples.
        while True:
            header = fh.read(4)
            if len(header) < 4:
                return 0.0
            last = header[0] & 0x80
            btype = header[0] & 0x7F
            blen = int.from_bytes(header[1:4], "big")
            block = fh.read(blen)
            if btype == 0 and len(block) >= 18:
                # bits 80..99 sample_rate (20 bits), bits 108..143 total_samples (36 bits).
                sr = (block[10] << 12) | (block[11] << 4) | (block[12] >> 4)
                total = ((block[13] & 0x0F) << 32) | (block[14] << 24) | \
                        (block[15] << 16) | (block[16] << 8) | block[17]
                return total / sr if sr else 0.0
            if last:
                return 0.0


# ---------------------------------------------------------------------------
# HTTP — stdlib only, returns (status, body_text). Never raises on HTTP errors.
# (Same robustness as check-models.py: a normal User-Agent so Groq's edge doesn't 403
# the python-urllib default, and retry/backoff on transient throttling.)
# ---------------------------------------------------------------------------

USER_AGENT = "dictate-desktop-benchmark/1.0"


def http(method: str, url: str, headers: dict[str, str],
         data: bytes | None = None, timeout: int = 180) -> tuple[int, str]:
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
               data: bytes | None = None, timeout: int = 180,
               retries: int = 4) -> tuple[int, str]:
    st, body = http(method, url, headers, data, timeout)
    attempt = 0
    while st in (403, 429, 500, 502, 503, 504) and attempt < retries:
        time.sleep(min(8.0, (2 ** attempt)) + random.uniform(0, 0.5))
        attempt += 1
        st, body = http(method, url, headers, data, timeout)
    return st, body


def multipart(fields: dict[str, str], filename: str, filedata: bytes,
              mime: str) -> tuple[bytes, str]:
    boundary = "----dictateBench" + uuid.uuid4().hex
    buf = io.BytesIO()
    for k, v in fields.items():
        buf.write(f"--{boundary}\r\n".encode())
        buf.write(f'Content-Disposition: form-data; name="{k}"\r\n\r\n'.encode())
        buf.write(f"{v}\r\n".encode())
    buf.write(f"--{boundary}\r\n".encode())
    buf.write(
        f'Content-Disposition: form-data; name="file"; filename="{filename}"\r\n'.encode())
    buf.write(f"Content-Type: {mime}\r\n\r\n".encode())
    buf.write(filedata)
    buf.write(f"\r\n--{boundary}--\r\n".encode())
    return buf.getvalue(), f"multipart/form-data; boundary={boundary}"


def audio_mime(path: Path) -> str:
    return {".flac": "audio/flac", ".ogg": "audio/ogg", ".oga": "audio/ogg",
            ".mp3": "audio/mpeg"}.get(path.suffix.lower(), "audio/wav")


def extract_msg(body: str) -> str:
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
# Per-provider transcription — faithful to the daemon's transcribe_file paths.
# Each returns the transcript text or raises RuntimeError(reason).
# ---------------------------------------------------------------------------

def t_assemblyai(clip: Clip, model: str, key: str) -> str:
    # src/assemblyai.rs transcribe_file: octet-stream upload, then submit with
    # language_detection + expected_languages (fallback auto), no speech_model field,
    # disfluencies=true (fillers kept), then poll /v2/transcript/{id}.
    data = clip.local.read_bytes()
    st, body = http_retry(
        "POST", "https://api.assemblyai.com/v2/upload",
        {"Authorization": key, "Content-Type": "application/octet-stream"}, data)
    if st != 200:
        raise RuntimeError(f"upload HTTP {st}: {extract_msg(body)}")
    upload_url = json.loads(body).get("upload_url")
    if not upload_url:
        raise RuntimeError("no upload_url in response")

    submit_body = json.dumps({
        "audio_url": upload_url,
        "language_detection": True,
        "language_detection_options": {
            "expected_languages": EXPECTED_LANGUAGES,
            "fallback_language": "auto",
        },
        "disfluencies": True,
    }).encode()
    st, body = http_retry(
        "POST", "https://api.assemblyai.com/v2/transcript",
        {"Authorization": key, "Content-Type": "application/json"}, submit_body)
    if st != 200:
        raise RuntimeError(f"submit HTTP {st}: {extract_msg(body)}")
    sj = json.loads(body)
    if sj.get("error"):
        raise RuntimeError(sj["error"])
    tid = sj["id"]

    poll_url = f"https://api.assemblyai.com/v2/transcript/{tid}"
    for _ in range(POLL_MAX_TRIES):
        time.sleep(POLL_INTERVAL_S)
        st, body = http("GET", poll_url, {"Authorization": key})
        if st != 200:
            continue
        pj = json.loads(body)
        status = pj.get("status")
        if status == "completed":
            return pj.get("text") or ""
        if status == "error":
            raise RuntimeError(pj.get("error", "unknown job error"))
    raise RuntimeError("poll timeout (>120s)")


def t_groq(clip: Clip, model: str, key: str) -> str:
    # src/groq.rs transcribe_file: OpenAI-compat multipart, model + response_format=json,
    # no language param at lang=auto.
    data, ctype = multipart({"model": model, "response_format": "json"},
                            clip.name, clip.local.read_bytes(), audio_mime(clip.local))
    st, body = http_retry(
        "POST", "https://api.groq.com/openai/v1/audio/transcriptions",
        {"Authorization": f"Bearer {key}", "Content-Type": ctype}, data)
    if st != 200:
        raise RuntimeError(f"HTTP {st}: {extract_msg(body)}")
    return json.loads(body).get("text") or ""


def t_fireworks(clip: Clip, model: str, key: str) -> str:
    # src/fireworks.rs transcribe_file: same multipart shape, audio-prod host.
    data, ctype = multipart({"model": model, "response_format": "json"},
                            clip.name, clip.local.read_bytes(), audio_mime(clip.local))
    st, body = http_retry(
        "POST", "https://audio-prod.api.fireworks.ai/v1/audio/transcriptions",
        {"Authorization": f"Bearer {key}", "Content-Type": ctype}, data)
    if st != 200:
        raise RuntimeError(f"HTTP {st}: {extract_msg(body)}")
    return json.loads(body).get("text") or ""


def t_deepgram(clip: Clip, model: str, key: str) -> str:
    # src/deepgram.rs transcribe_file: raw-body POST, language=multi at lang=auto,
    # smart_format=true, filler_words=true (fillers kept). nova-* accept multi; the hosted
    # Whisper models reject it (the daemon's auto path would 400 the same way) — surfaced as
    # the row's failure reason.
    api_lang = "multi"
    url = (f"https://api.deepgram.com/v1/listen?model={model}"
           f"&language={api_lang}&smart_format=true&filler_words=true")
    st, body = http_retry(
        "POST", url,
        {"Authorization": f"Token {key}", "Content-Type": audio_mime(clip.local)},
        clip.local.read_bytes())
    if st != 200:
        raise RuntimeError(f"HTTP {st}: {extract_msg(body)}")
    j = json.loads(body)
    if j.get("err_msg") or j.get("error"):
        raise RuntimeError(j.get("err_msg") or j.get("error"))
    try:
        return j["results"]["channels"][0]["alternatives"][0]["transcript"] or ""
    except (KeyError, IndexError):
        return ""


TRANSCRIBE = {
    "assemblyai": t_assemblyai,
    "groq": t_groq,
    "fireworks": t_fireworks,
    "deepgram": t_deepgram,
}


# ---------------------------------------------------------------------------
# Normalization + WER.
# ---------------------------------------------------------------------------

def normalize(text: str) -> list[str]:
    """Lowercase, strip punctuation, collapse whitespace → token list for WER."""
    text = text.lower()
    text = re.sub(r"[^\w\s]", " ", text, flags=re.UNICODE)  # drop punctuation
    return text.split()


def wer(reference: list[str], hypothesis: list[str]) -> float:
    """Word error rate = Levenshtein edit distance (word level) / reference length.
    A reference of zero words yields 0.0 when the hypothesis is also empty, else 1.0."""
    r, h = reference, hypothesis
    if not r:
        return 0.0 if not h else 1.0
    # Two-row DP edit distance — O(len(r)*len(h)) time, O(len(h)) space.
    prev = list(range(len(h) + 1))
    for i, rw in enumerate(r, 1):
        cur = [i] + [0] * len(h)
        for j, hw in enumerate(h, 1):
            cost = 0 if rw == hw else 1
            cur[j] = min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + cost)
        prev = cur
    return prev[len(h)] / len(r)


# ---------------------------------------------------------------------------
# Benchmark run.
# ---------------------------------------------------------------------------

@dataclass
class Task:
    provider: str
    model: str
    label: str
    clip: Clip


@dataclass
class TaskResult:
    label: str
    provider: str
    clip: str
    latency_s: float | None
    text: str | None
    error: str | None


@dataclass
class ModelStats:
    label: str
    provider: str
    latencies: list[float] = field(default_factory=list)
    wers: list[float] = field(default_factory=list)
    failures: list[tuple[str, str]] = field(default_factory=list)  # (clip, reason)
    is_reference: bool = False


def run_benchmark(tasks: list[Task], keys: dict[str, str],
                  show_progress: bool) -> list[TaskResult]:
    """Run every (model, clip) transcription. Each provider is gated by its own semaphore so
    the throttled ones (Groq/Fireworks) don't overwhelm their tier while the tolerant ones run
    wide. A single thread pool drives them all concurrently."""
    sems = {p: threading.Semaphore(CONCURRENCY[p]) for p in CONCURRENCY}
    results: list[TaskResult] = []
    done = 0
    total = len(tasks)
    lock = threading.Lock()

    def one(task: Task) -> TaskResult:
        nonlocal done
        fn = TRANSCRIBE[task.provider]
        with sems[task.provider]:
            t0 = time.perf_counter()
            try:
                text = fn(task.clip, task.model, keys[task.provider])
                res = TaskResult(task.label, task.provider, task.clip.name,
                                 round(time.perf_counter() - t0, 3), text, None)
            except Exception as e:  # noqa: BLE001
                res = TaskResult(task.label, task.provider, task.clip.name,
                                 None, None, str(e)[:200])
        if show_progress:
            with lock:
                done += 1
                status = green("OK ") if res.error is None else red("ERR")
                lat = f"{res.latency_s:6.2f}s" if res.latency_s is not None else "   -  "
                log(f"  [{done:>3}/{total}] {status} {task.label:<34} "
                    f"{task.clip.name:<24} {lat}"
                    + (dim("  " + res.error) if res.error else ""))
        return res

    max_workers = sum(CONCURRENCY[t.provider] for t in {t.provider: t for t in tasks}.values()) \
        if tasks else 1
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, max_workers)) as ex:
        for res in ex.map(one, tasks):
            results.append(res)
    return results


def aggregate(results: list[TaskResult],
              order: list[tuple[str, str]]) -> tuple[dict[str, str], list[ModelStats]]:
    """Turn raw per-task results into per-model stats, computing each non-reference model's
    WER against the reference model's transcript on the same clip. `order` is the [(label,
    provider)] sequence to keep the output stable. Returns (reference_text_by_clip, stats)."""
    by_label_clip: dict[tuple[str, str], TaskResult] = {}
    for r in results:
        by_label_clip[(r.label, r.clip)] = r

    reference_text: dict[str, str] = {}
    for (label, clip), r in by_label_clip.items():
        if label == REFERENCE_LABEL and r.text is not None:
            reference_text[clip] = r.text

    stats: list[ModelStats] = []
    for label, provider in order:
        ms = ModelStats(label=label, provider=provider,
                        is_reference=(label == REFERENCE_LABEL))
        clip_results = [r for (lbl, _clip), r in by_label_clip.items() if lbl == label]
        for r in sorted(clip_results, key=lambda x: x.clip):
            if r.error is not None:
                ms.failures.append((r.clip, r.error))
                continue
            if r.latency_s is not None:
                ms.latencies.append(r.latency_s)
            # WER only where we have a reference transcript for that clip and this isn't the
            # reference itself.
            if not ms.is_reference and r.clip in reference_text and r.text is not None:
                ref_tokens = normalize(reference_text[r.clip])
                hyp_tokens = normalize(r.text)
                ms.wers.append(wer(ref_tokens, hyp_tokens))
        stats.append(ms)
    return reference_text, stats


# ---------------------------------------------------------------------------
# Rendering.
# ---------------------------------------------------------------------------

def fmt_pct(x: float | None) -> str:
    return f"{x * 100:5.1f}%" if x is not None else "   -  "


def fmt_s(x: float | None) -> str:
    return f"{x:6.2f}s" if x is not None else "   -  "


def med_p90(xs: list[float]) -> tuple[float | None, float | None]:
    if not xs:
        return None, None
    s = sorted(xs)
    median = statistics.median(s)
    # p90 by nearest-rank.
    idx = max(0, min(len(s) - 1, int(round(0.9 * (len(s) - 1)))))
    return median, s[idx]


def render(stats: list[ModelStats], clips: list[Clip], host: str, sample_size: int,
           seed: int) -> None:
    print(bold("\nSTT benchmark — dictate-desktop"))
    durs = [c.duration_s for c in clips if c.duration_s > 0]
    dur_note = ""
    if durs:
        dur_note = (f", duration {min(durs):.1f}–{max(durs):.1f}s "
                    f"(total {sum(durs):.0f}s)")
    print(dim(f"{len(clips)} clip(s) from {host}:{REMOTE_AUDIO_DIR}{dur_note}; "
              f"sample seed {seed}"))
    print(dim(f"WER measured against {REFERENCE_LABEL} (reference), normalized "
              f"(lowercase, no punctuation, collapsed whitespace)\n"))

    header = (f"  {'model':<34} {'WER med':>8} {'WER mean':>9} "
              f"{'lat med':>8} {'lat p90':>8} {'fails':>6}")
    print(bold(header))
    print(dim("  " + "-" * (len(header) - 2)))

    # Reference first, then by ascending median WER (best accuracy on top), failures last.
    def sort_key(ms: ModelStats):
        if ms.is_reference:
            return (0, 0.0)
        med, _ = med_p90(ms.wers) if ms.wers else (None, None)
        if med is None:
            return (2, 0.0)  # nothing succeeded / no WER → bottom
        return (1, med)

    for ms in sorted(stats, key=sort_key):
        n_clips = len(clips)
        n_fail = len(ms.failures)
        lat_med, lat_p90 = med_p90(ms.latencies)

        if ms.is_reference:
            wer_med_s = dim("   ref ")
            wer_mean_s = dim("    ref ")
        else:
            wer_med, _ = med_p90(ms.wers) if ms.wers else (None, None)
            wer_mean = statistics.fmean(ms.wers) if ms.wers else None
            wer_med_s = fmt_pct(wer_med)
            wer_mean_s = fmt_pct(wer_mean)
            # Colour accuracy: green <10% WER, yellow <25%, red otherwise.
            if wer_med is not None:
                col = green if wer_med < 0.10 else (yellow if wer_med < 0.25 else red)
                wer_med_s = col(wer_med_s)

        fails_s = (red(f"{n_fail}/{n_clips}") if n_fail else green(f"0/{n_clips}"))
        name = bold(ms.label) if ms.is_reference else ms.label
        pad = " " * max(0, 34 - len(ms.label))
        print(f"  {name}{pad} {wer_med_s:>8} {wer_mean_s:>9} "
              f"{fmt_s(lat_med):>8} {fmt_s(lat_p90):>8} {fails_s:>6}")

    # Failure detail — group identical reasons so short-clip / entitlement errors stand out.
    any_fail = any(ms.failures for ms in stats)
    if any_fail:
        print(bold("\nfailures"))
        for ms in stats:
            if not ms.failures:
                continue
            reasons: dict[str, list[str]] = {}
            for clip, reason in ms.failures:
                reasons.setdefault(reason, []).append(clip)
            print(f"  {yellow(ms.label)} ({len(ms.failures)}):")
            for reason, clip_list in sorted(reasons.items(), key=lambda kv: -len(kv[1])):
                shown = ", ".join(clip_list[:3]) + (" …" if len(clip_list) > 3 else "")
                print(f"    {dim('×' + str(len(clip_list)))} {reason}")
                print(dim(f"       {shown}"))
    print(dim("\nWER med/mean = error rate vs the reference transcript across clips · "
              "lat med/p90 = per-clip wall-clock · fails = clips that errored "
              "(short-clip rejects, missing entitlement, decommissioned model)\n"))


# ---------------------------------------------------------------------------
# Main.
# ---------------------------------------------------------------------------

def main() -> None:
    ap = argparse.ArgumentParser(
        description="Benchmark dictate-desktop's STT providers/models on real recordings.")
    ap.add_argument("--host", default="riva",
                    help="host whose daemon env holds the keys and whose audio dir is sampled "
                         "(default: riva)")
    ap.add_argument("--local", action="store_true",
                    help="use API keys from this shell's environment instead of SSH "
                         "(clips are still pulled from --host)")
    ap.add_argument("--provider", choices=PROVIDERS, action="append",
                    help="limit to one or more providers (repeatable)")
    ap.add_argument("--model", action="append",
                    help="limit to one or more model ids, e.g. nova-2 (repeatable)")
    ap.add_argument("--sample-size", type=int, default=12,
                    help="number of recordings to benchmark (default: 12)")
    ap.add_argument("--seed", type=int, default=1,
                    help="sampling seed, for a reproducible clip set (default: 1)")
    ap.add_argument("--json", action="store_true",
                    help="emit machine-readable JSON results instead of the table")
    ap.add_argument("--color", choices=("auto", "always", "never"), default="auto")
    ap.add_argument("--no-color", action="store_const", const="never", dest="color")
    args = ap.parse_args()

    C.enabled = color_enabled(args.color) and not args.json

    daemon_models = parse_daemon_models()

    # Build the (provider, model) work set the daemon would actually offer, filtered by flags.
    wanted_providers = args.provider or list(PROVIDERS)
    model_filter = set(args.model) if args.model else None
    work: list[tuple[str, str]] = []  # (provider, model)
    for prov in PROVIDERS:
        if prov not in wanted_providers:
            continue
        for model in daemon_models.get(prov, []):
            if model_filter and model not in model_filter:
                continue
            work.append((prov, model))

    if model_filter:
        listed = {m for _p, m in work}
        missing = model_filter - listed
        if missing:
            log(yellow(f"! ignoring unknown model id(s) (not in the daemon's list): "
                       f"{', '.join(sorted(missing))}"))

    if not work:
        die("no provider/model combinations selected — check --provider/--model")

    # The reference model must be present to compute WER. If the user filtered it out (e.g.
    # --provider groq) but other models remain, add it back so there's something to score
    # against; if they explicitly asked only for AAI, that's fine (latency-only run).
    have_reference = any(p == REFERENCE_PROVIDER and m == REFERENCE_MODEL for p, m in work)
    only_reference = all(p == REFERENCE_PROVIDER and m == REFERENCE_MODEL for p, m in work)
    if not have_reference and not only_reference:
        work.insert(0, (REFERENCE_PROVIDER, REFERENCE_MODEL))
        if not args.json:
            log(dim(f"(adding {REFERENCE_LABEL} as the WER reference)"))

    # Keys.
    keys = load_keys_local() if args.local else load_keys_from_host(args.host)
    found = sorted(keys.keys())
    src = "this shell's env" if args.local else f"{args.host}'s daemon env"
    if not args.json:
        if found:
            log(dim(f"keys found in {src}: {', '.join(found)} (values not shown)"))
        else:
            die(f"no provider API keys found in {src}")

    # Drop work whose provider has no key (and warn).
    runnable: list[tuple[str, str]] = []
    skipped_no_key: list[str] = []
    for prov, model in work:
        if prov in keys:
            runnable.append((prov, model))
        else:
            skipped_no_key.append(f"{prov}/{model}")
    if skipped_no_key and not args.json:
        log(yellow(f"! no {', '.join(sorted({KEY_ENV[p.split('/')[0]] for p in skipped_no_key}))}"
                   f" — skipping: {', '.join(skipped_no_key)}"))
    if not runnable:
        die("no runnable models (no keys for the selected providers)")

    # Sample + cache clips.
    all_clips = list_remote_clips(args.host)
    if not all_clips:
        die(f"no recordings found in {args.host}:{REMOTE_AUDIO_DIR}")
    chosen = stratified_pick(all_clips, args.sample_size, args.seed)
    if not args.json:
        log(dim(f"{len(all_clips)} recording(s) on {args.host}; "
                f"benchmarking a stratified sample of {len(chosen)}"))
    clips = fetch_clips(args.host, chosen)
    if not clips:
        die("no clips available locally after fetch")

    # Build and run the task list.
    order = runnable  # stable [(provider, model)] -> labels
    labels = [(f"{p}/{m}", p) for p, m in order]
    tasks = [Task(provider=p, model=m, label=f"{p}/{m}", clip=c)
             for (p, m) in order for c in clips]

    if not args.json:
        log(dim(f"running {len(tasks)} transcription(s) "
                f"({len(order)} model(s) × {len(clips)} clip(s))…\n"))
    results = run_benchmark(tasks, keys, show_progress=not args.json)

    _, stats = aggregate(results, labels)

    if args.json:
        out = {
            "host": args.host,
            "sample_size": len(clips),
            "seed": args.seed,
            "reference": REFERENCE_LABEL,
            "clips": [{"name": c.name, "duration_s": round(c.duration_s, 3),
                       "size": c.size} for c in clips],
            "models": [],
        }
        for ms in stats:
            lat_med, lat_p90 = med_p90(ms.latencies)
            wer_med, _ = med_p90(ms.wers) if ms.wers else (None, None)
            out["models"].append({
                "model": ms.label,
                "provider": ms.provider,
                "is_reference": ms.is_reference,
                "n_clips": len(clips),
                "n_ok": len(ms.latencies),
                "n_failures": len(ms.failures),
                "wer_median": round(wer_med, 4) if wer_med is not None else None,
                "wer_mean": round(statistics.fmean(ms.wers), 4) if ms.wers else None,
                "latency_median_s": round(lat_med, 3) if lat_med is not None else None,
                "latency_p90_s": round(lat_p90, 3) if lat_p90 is not None else None,
                "failures": [{"clip": c, "reason": r} for c, r in ms.failures],
            })
        print(json.dumps(out, indent=2, ensure_ascii=False))
    else:
        render(stats, clips, args.host, len(clips), args.seed)


if __name__ == "__main__":
    main()
