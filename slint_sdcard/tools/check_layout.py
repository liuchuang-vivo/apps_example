#!/usr/bin/env python3
"""Run the real Slint layout and pointer regression test without attached hardware."""

import argparse
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    subprocess.run([str(args.binary.resolve()), str(args.output.parent)], check=True)
    args.output.write_text("Slint layout and navigation checks passed\n", encoding="utf-8")


if __name__ == "__main__":
    main()
