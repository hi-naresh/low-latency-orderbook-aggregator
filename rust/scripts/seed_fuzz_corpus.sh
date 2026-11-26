#!/usr/bin/env sh
# Seeds the fuzz corpora from the recorded fixtures (the corpus itself is git-ignored:
# it grows to thousands of files while fuzzing).
# Usage (from rust/): scripts/seed_fuzz_corpus.sh
set -eu
cd "$(dirname "$0")/../crates/ob-parse"
mkdir -p fuzz/corpus/binance fuzz/corpus/bybit fuzz/corpus/decimal
for venue in binance:binance_depth20 bybit:bybit_orderbook50; do
  i=0
  while IFS= read -r line; do
    i=$((i + 1))
    printf '%s' "$line" > "fuzz/corpus/${venue%%:*}/frame_$i.json"
  done < "tests/fixtures/${venue#*:}.jsonl"
done
for s in 58631.90 6.774 0 0.001 105123.40; do
  printf '\001%s' "$s" > "fuzz/corpus/decimal/d1_$s"   # first byte = decimals
  printf '\003%s' "$s" > "fuzz/corpus/decimal/d3_$s"
done
echo "seeded: $(ls fuzz/corpus/binance | wc -l) binance, $(ls fuzz/corpus/bybit | wc -l) bybit, $(ls fuzz/corpus/decimal | wc -l) decimal"

# WebSocket codec: real exchange messages wrapped in server frames, behind the 10-byte
# control prefix the target expects (max size, UTF-8 flag, 8 chunk-cut bytes).
cd ../ob-net
mkdir -p fuzz/corpus/ws
python3 - <<'PY'
import pathlib, struct
fixtures = pathlib.Path("../ob-parse/tests/fixtures")
out = pathlib.Path("fuzz/corpus/ws")
def frame(payload, op=0x1, fin=True):
    b0 = (0x80 if fin else 0) | op
    n = len(payload)
    if n < 126: head = bytes([b0, n])
    elif n < 65536: head = bytes([b0, 126]) + struct.pack(">H", n)
    else: head = bytes([b0, 127]) + struct.pack(">Q", n)
    return head + payload
prefix = bytes([255, 1, 3, 9, 27, 81, 120, 200, 240, 250])
i = 0
for f in ["binance_depth20.jsonl", "bybit_orderbook50.jsonl"]:
    for line in (fixtures / f).read_bytes().splitlines()[:5]:
        i += 1
        (out / f"msg_{i}").write_bytes(prefix + frame(line))
# fragmented text with a ping in between, and a close
(out / "fragmented").write_bytes(prefix + frame(b"Hel", 0x1, False) + frame(b"hb", 0x9) + frame(b"lo", 0x0))
(out / "close").write_bytes(prefix + frame(b"\x03\xe8bye", 0x8))
print(f"seeded: {i + 2} ws")
PY
