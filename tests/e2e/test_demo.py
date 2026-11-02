import runpy
import shutil
import subprocess
from pathlib import Path

import pytest


@pytest.fixture(scope="module")
def demo():
    return runpy.run_path(str(Path(__file__).resolve().parents[2] / "scripts/demo"))


def test_replay_rows_keep_blank_highlights_and_inherited_styles(demo):
    style = "\x1b[1;38;2;12;34;56;48;5;236m"
    lines = demo["replay_lines"](
        style + " " * 120 + "\ntext   \n\x1b[22;39;49mplain\nnext\n"
    )
    assert len(lines) == demo["ROWS"]
    assert lines[0] == style + " " * 120
    assert lines[1] == style + "text   "
    assert lines[2] == style + "\x1b[22;39;49mplain"
    assert lines[3] == "next"
    assert lines[4:] == [""] * (demo["ROWS"] - 4)


def test_replay_coalesces_frames_and_diffs_rows(demo):
    replay = demo["make_replay"](
        [(0, "review\nunchanged"), (0.2, "review\nunchanged"), (0.5, "fix\nunchanged")]
    )
    assert replay["cols"] == 120
    assert replay["rows"] == 32
    assert len(replay["frames"]) == 2
    assert replay["frames"][0]["ms"] == 500
    assert replay["frames"][1]["ms"] == 120
    assert replay["frames"][1]["l"] == ["fix"] + [None] * 31
    with pytest.raises(RuntimeError, match="no frames"):
        demo["make_replay"]([])
    for text in ("demo/\x1b[2mscripted", "Loading sessions..."):
        with pytest.raises(RuntimeError, match="model labels or a cold-start"):
            demo["make_replay"]([(0, text)])


@pytest.mark.skipif(
    not shutil.which("ffmpeg") or not shutil.which("ffprobe"),
    reason="GIF timing check requires ffmpeg and ffprobe",
)
def test_encoded_gif_does_not_repeat_final_hold(demo, tmp_path):
    for n, color in enumerate(("black", "white")):
        subprocess.run(
            [
                "ffmpeg",
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                f"color=c={color}:s=16x16",
                "-frames:v",
                "1",
                "-threads",
                "1",
                str(tmp_path / f"{n:04d}.png"),
            ],
            check=True,
            timeout=10,
        )
    output = tmp_path / "demo.gif"
    demo["encode"]({"frames": [{"ms": 500}, {"ms": 2500}]}, tmp_path, output)
    duration = subprocess.check_output(
        [
            "ffprobe",
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            str(output),
        ],
        text=True,
        timeout=10,
    )
    assert float(duration) == pytest.approx(3, abs=0.09)
