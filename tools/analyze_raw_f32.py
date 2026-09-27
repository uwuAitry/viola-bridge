# SPDX-License-Identifier: GPL-3.0-or-later
"""Report per-channel RMS/peak of a headerless interleaved f32 render dump.

Usage:
    python analyze_raw_f32.py <file.f32> <channels> [sample_rate]

Reads `orender --output-backend file --output-file-format raw-f32` output and
prints one line per channel plus a silence verdict. Standard library only, so it
runs on a stock Python with no install step.
"""
import array
import math
import sys


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__.strip())
        return 2
    path = sys.argv[1]
    channels = int(sys.argv[2])
    rate = int(sys.argv[3]) if len(sys.argv) > 3 else 48_000
    if channels <= 0:
        print("channel count must be positive")
        return 2

    samples = array.array("f")
    with open(path, "rb") as handle:
        samples.frombytes(handle.read())
    if sys.byteorder == "big":  # raw-f32 is little-endian by definition
        samples.byteswap()

    total = len(samples)
    if total == 0:
        print("EMPTY: no samples read")
        return 1
    frames = total // channels
    print("file       : %s" % path)
    print(
        "samples    : %d (%d frames, %d ch, %.3fs @ %d Hz)"
        % (total, frames, channels, frames / float(rate), rate)
    )

    loudest = 0.0
    for ch in range(channels):
        acc = 0.0
        peak = 0.0
        count = 0
        for idx in range(ch, total, channels):
            value = samples[idx]
            if math.isfinite(value):
                acc += value * value
                peak = max(peak, abs(value))
                count += 1
        rms = math.sqrt(acc / count) if count else 0.0
        loudest = max(loudest, rms)
        dbfs = 20.0 * math.log10(rms) if rms > 0 else float("-inf")
        flag = "" if rms > 1e-6 else "   <- silent"
        print(
            "  ch %2d  rms=%.6f (%7.2f dBFS)  peak=%.6f%s"
            % (ch + 1, rms, dbfs, peak, flag)
        )

    print("verdict    : %s" % ("AUDIBLE" if loudest > 1e-6 else "SILENT"))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
