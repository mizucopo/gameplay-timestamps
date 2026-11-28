#!/usr/bin/env python3
"""Emit Japanese SRT from a local recording and local mlx-whisper weights."""

import argparse
from contextlib import redirect_stdout
from heapq import heappop, heappush
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


SAMPLE_RATE = 16_000
CHUNK_SAMPLES = 450 * SAMPLE_RATE
CONTEXT_SAMPLES = 2 * SAMPLE_RATE


def windows(stream):
    """Yield the central sample range plus up to two seconds of audio on either side."""
    chunk_bytes = 2 * CHUNK_SAMPLES
    context_bytes = 2 * CONTEXT_SAMPLES
    size = chunk_bytes + context_bytes
    left = b""
    upcoming = stream.read(size)
    start = 0
    while upcoming:
        if len(upcoming) % 2:
            raise ValueError("FFmpeg の PCM が途中で終了しました")
        middle = min(chunk_bytes, len(upcoming))
        end = start + middle // 2
        yield start, end, start - len(left) // 2, left + upcoming
        left = upcoming[max(0, middle - context_bytes):middle]
        ahead = upcoming[middle:]
        if not ahead:
            break
        upcoming = ahead + stream.read(size - len(ahead))
        start = end


def segments(stream, model):
    import mlx.core as mx
    import mlx_whisper
    import numpy as np

    if not mx.metal.is_available():
        raise RuntimeError("MLX が利用できる Apple Silicon Mac が必要です")
    mx.set_cache_limit(1024**3)
    pending = []
    for start, end, offset, pcm in windows(stream):
        audio = np.frombuffer(pcm, dtype="<i2").astype(np.float32) / 32768.0
        result = mlx_whisper.transcribe(
            audio, path_or_hf_repo=str(model), language="ja", task="transcribe",
            temperature=0.0, condition_on_previous_text=False,
            word_timestamps=False, verbose=None,
        )
        left_ms = round(offset / SAMPLE_RATE * 1000)
        right_ms = round((offset + len(audio)) / SAMPLE_RATE * 1000)
        for item in result["segments"]:
            first = max(left_ms, round((offset / SAMPLE_RATE + item["start"]) * 1000))
            last = min(right_ms, round((offset / SAMPLE_RATE + item["end"]) * 1000))
            text = item["text"].strip()
            midpoint = (first + last) / 2
            belongs = start / SAMPLE_RATE * 1000 <= midpoint < end / SAMPLE_RATE * 1000
            if first < last and text and belongs:
                heappush(pending, (first, last, text))
        del result, audio
        mx.clear_cache()

        # The next chunk may still contribute starts in this chunk's trailing overlap.
        earliest_next = (end - CONTEXT_SAMPLES) * 1000 / SAMPLE_RATE
        while pending and pending[0][0] < earliest_next:
            yield heappop(pending)
    while pending:
        yield heappop(pending)


def clock(ms):
    seconds, ms = divmod(ms, 1000)
    minutes, seconds = divmod(seconds, 60)
    hours, minutes = divmod(minutes, 60)
    return f"{hours:02d}:{minutes:02d}:{seconds:02d},{ms:03d}"


def merge_overlaps(cues):
    previous = None
    for cue in cues:
        if previous and cue[2] == previous[2] and cue[0] < previous[1]:
            previous = (previous[0], max(previous[1], cue[1]), previous[2])
            continue
        if previous:
            yield previous
        previous = cue
    if previous:
        yield previous


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("video", type=Path)
    video = parser.parse_args().video.expanduser().resolve(strict=True)
    setting = os.environ.get("GAMEPLAY_WHISPER_MODEL")
    if not setting:
        parser.error("GAMEPLAY_WHISPER_MODEL にローカルの MLX モデルディレクトリを指定してください")
    model = Path(setting).expanduser().resolve(strict=True)
    weights = ("weights.safetensors", "weights.npz")
    if not (model / "config.json").is_file() or not any((model / name).is_file() for name in weights):
        parser.error("モデルの config.json と MLX 重みファイルを確認してください")
    os.environ.update(HF_HUB_OFFLINE="1", TRANSFORMERS_OFFLINE="1", HF_HUB_DISABLE_TELEMETRY="1")
    command = [
        "ffmpeg", "-nostdin", "-v", "error", "-xerror", "-protocol_whitelist", "file,pipe",
        "-i", str(video), "-map", "0:a:0", "-af", "aresample=async=1:first_pts=0",
        "-ac", "1", "-ar", str(SAMPLE_RATE), "-c:a", "pcm_s16le", "-f", "s16le", "pipe:1",
    ]
    # Publish only a complete SRT. Temporary storage scales with the text, not the recording.
    with tempfile.TemporaryFile() as subtitles:
        with subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE) as decoder:
            try:
                with decoder.stdout, redirect_stdout(sys.stderr):
                    cues = merge_overlaps(segments(decoder.stdout, model))
                    for number, (first, last, text) in enumerate(cues, 1):
                        entry = f"{number}\n{clock(first)} --> {clock(last)}\n{text}\n\n"
                        subtitles.write(entry.encode("utf-8"))
            except BaseException:
                decoder.kill()
                raise
            if decoder.wait() != 0:
                raise RuntimeError("FFmpeg で音声を最後まで読み取れませんでした")
        subtitles.seek(0)
        shutil.copyfileobj(subtitles, sys.stdout.buffer)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ImportError, RuntimeError, ValueError) as error:
        sys.exit(str(error))
