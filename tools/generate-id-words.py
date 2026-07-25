#!/usr/bin/env -S uv run
# /// script
# requires-python = ">=3.12,<3.14"
# dependencies = [
#   "tiktoken==0.11.0",
# ]
# ///

from __future__ import annotations

import hashlib
import io
import math
import tarfile
import urllib.request
from pathlib import Path

import tiktoken


SOURCE_URL = (
    "https://github.com/allenap/rust-petname/archive/refs/tags/v3.1.0.tar.gz"
)
SOURCE_SHA256 = "ac72beba2e8e5272ab58609de84f1f50f7474d978fddff78d1724218ea354516"
SOURCE_PREFIX = "rust-petname-3.1.0/words/medium/"
OUTPUT = (
    Path(__file__).resolve().parents[1]
    / "crates/atra-id/src/words.txt"
)
EXPECTED_WORD_COUNT = 330
ENCODINGS = ("cl100k_base", "o200k_base")


def main() -> None:
    archive = urllib.request.urlopen(SOURCE_URL).read()
    digest = hashlib.sha256(archive).hexdigest()
    if digest != SOURCE_SHA256:
        raise RuntimeError(f"source digest changed: {digest}")

    words = set()
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as source:
        for name in ("adjectives.txt", "adverbs.txt", "nouns.txt"):
            member = source.extractfile(SOURCE_PREFIX + name)
            if member is None:
                raise RuntimeError(f"source archive does not contain {name}")
            words.update(member.read().decode().splitlines())

    encodings = [tiktoken.get_encoding(name) for name in ENCODINGS]
    eligible = [
        word
        for word in words
        if 3 <= len(word) <= 10
        and word.isascii()
        and word.isalpha()
        and word.islower()
        and all(
            len(encoding.encode(word)) == 1
            and len(encoding.encode(" " + word)) == 1
            for encoding in encodings
        )
    ]
    if len(eligible) != EXPECTED_WORD_COUNT:
        raise RuntimeError(
            f"{len(eligible)} words passed filters; expected {EXPECTED_WORD_COUNT}"
        )

    eligible.sort()
    OUTPUT.write_text("".join(f"{word}\n" for word in eligible))
    print(
        f"wrote {len(eligible)} words ({math.log2(len(eligible)):.2f} bits each) "
        f"to {OUTPUT}"
    )


if __name__ == "__main__":
    main()
