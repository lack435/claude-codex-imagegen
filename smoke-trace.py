"""Decode the payloads of Codex rollout traces written in schema version 3, for smoke.ps1.

Usage: py -3.14 -I smoke-trace.py <trace root> <out dir>

From codex-cli 0.171.0, a trace folder's events name their payloads by id ({raw_payload_id, kind,
shard, parts?}) instead of by file, and the payloads live in zstd-compressed shards,
payloads\\<shard, 6 digits>.zst. Windows PowerShell 5.1 cannot decompress zstd, so this helper
writes, for every trace folder under <trace root> whose manifest.json says schema_version 3, each
payload that smoke.ps1 reads -- an inference_started event's request_payload and a
tool_call_started event's invocation_payload -- as plain JSON to
<out dir>\\<trace folder name>\\<raw payload id, ':' as '_'>.json. Folders in an older schema are
left alone; smoke.ps1 reads their payload files directly.

The layout, as read from real traces (codex-cli 0.171.0):
- A shard decompresses to records, back to back: a little-endian u16 id length, a little-endian u64
  data length, the id (UTF-8, such as "raw_payload:6"), then the data.
- A payload reference without parts names a record whose data is the JSON document itself.
- A reference with N parts names a record whose data is a JSON array of N+1 strings: the document
  is string 0, part 1's data, string 1, ... part N's data, string N. Each part is a reference of
  its own (to a large value such as an image's base64, stored once and shared).

It fails closed: a record layout that does not consume a shard exactly, a missing record or shard,
a split payload whose segment count does not match its parts, or a reference carrying a field
other than raw_payload_id, kind, shard and parts (a truncation marker, say) is an error, and the
payload is not written. Each error is printed as "problem: ..." and the exit code is 1, so the
smoke check that needed the payload fails rather than reading part of it. Exit code 0 means every
payload named was written.

Standard library only: compression.zstd needs Python 3.14 or later.
"""

import json
import struct
import sys
from pathlib import Path

try:
    from compression import zstd
except ImportError:
    print("problem: Python 3.14 or later is needed (compression.zstd)")
    sys.exit(3)

REFERENCE_KEYS = {"raw_payload_id", "kind", "shard", "parts"}
# The events and the payload each names that smoke.ps1 reads.
WANTED = {"inference_started": "request_payload", "tool_call_started": "invocation_payload"}


class TraceError(Exception):
    pass


def decompress(raw):
    """The shard's content, and whether its zstd stream was complete. A writer stopped mid-shard
    (a server killed on purpose, as smoke step (c) does) leaves a stream without its end; what was
    flushed before still decompresses."""
    out = []
    complete = True
    while raw:
        decompressor = zstd.ZstdDecompressor()
        out.append(decompressor.decompress(raw))
        if not decompressor.eof:
            complete = False
            break
        raw = decompressor.unused_data
    return b"".join(out), complete


def read_shard(path):
    """The records in one shard, by id. A record cut off by an incomplete stream is left out, so
    asking for it is an error; every record returned is whole, since its length is recorded."""
    data, complete = decompress(path.read_bytes())
    records = {}
    pos = 0
    while pos < len(data):
        header_end = pos + 10
        if header_end <= len(data):
            (id_len,) = struct.unpack_from("<H", data, pos)
            (data_len,) = struct.unpack_from("<Q", data, pos + 2)
            end = header_end + id_len + data_len
        if header_end > len(data) or end > len(data):
            if complete:
                raise TraceError(f"{path.name}: the record at byte {pos} runs past the shard's end")
            break  # the cut-off tail of an incomplete stream
        record_id = data[header_end : header_end + id_len].decode("utf-8")
        if record_id in records:
            raise TraceError(f"{path.name}: record {record_id} appears twice")
        records[record_id] = data[header_end + id_len : end]
        pos = end
    return records, complete


class Folder:
    def __init__(self, path):
        self.path = path
        self.shards = {}

    def record(self, shard, record_id):
        if not isinstance(shard, int):
            raise TraceError(f"{record_id}: shard {shard!r} is not a number")
        if shard not in self.shards:
            shard_path = self.path / "payloads" / f"{shard:06d}.zst"
            if not shard_path.is_file():
                raise TraceError(f"{record_id}: shard file {shard_path.name} is missing")
            self.shards[shard] = read_shard(shard_path)
        records, complete = self.shards[shard]
        if record_id not in records:
            why = "" if complete else " (the shard's stream ends early: its writer was stopped)"
            raise TraceError(f"{record_id} is not in shard {shard}{why}")
        return records[record_id]

    def resolve(self, ref):
        """The payload a reference names, as text, with its parts spliced in."""
        if not isinstance(ref, dict):
            raise TraceError(f"a payload reference is not an object: {ref!r:.200}")
        extra = set(ref) - REFERENCE_KEYS
        record_id = ref.get("raw_payload_id")
        if extra:
            raise TraceError(f"{record_id}: unrecognised reference field(s) {sorted(extra)}")
        if not isinstance(record_id, str):
            raise TraceError("a payload reference has no raw_payload_id")
        text = self.record(ref.get("shard"), record_id).decode("utf-8")
        parts = ref.get("parts") or []
        if not parts:
            return text
        segments = json.loads(text)
        if not (isinstance(segments, list) and all(isinstance(s, str) for s in segments)):
            raise TraceError(f"{record_id}: has parts, but its data is not a list of strings")
        if len(segments) != len(parts) + 1:
            raise TraceError(
                f"{record_id}: {len(segments)} segment(s) for {len(parts)} part(s)"
            )
        pieces = [segments[0]]
        for part, segment in zip(parts, segments[1:]):
            pieces.append(self.resolve(part))
            pieces.append(segment)
        return "".join(pieces)


def schema_version(folder):
    manifest = folder / "manifest.json"
    if not manifest.is_file():
        return None
    try:
        return json.loads(manifest.read_text(encoding="utf-8")).get("schema_version")
    except (OSError, ValueError):
        return None


def decode_folder(folder, out_root):
    """Write the wanted payloads of one schema-3 folder. Returns (written, problems)."""
    reader = Folder(folder)
    out = out_root / folder.name
    out.mkdir(parents=True, exist_ok=True)
    written = 0
    problems = []
    log = folder / "trace.jsonl"
    with log.open(encoding="utf-8") as lines:
        for number, line in enumerate(lines, 1):
            if not any(f'"{kind}"' in line for kind in WANTED):
                continue
            try:
                event = json.loads(line)
            except ValueError as e:
                problems.append(f"{folder.name}, line {number}: not a JSON event ({e})")
                continue
            payload = event.get("payload") or {}
            field = WANTED.get(payload.get("type"))
            if field is None or field not in payload:
                continue
            ref = payload[field]
            record_id = ref.get("raw_payload_id") if isinstance(ref, dict) else None
            try:
                text = reader.resolve(ref)
                # Must parse as JSON: a document cut short is not handed on.
                json.loads(text)
            except (TraceError, ValueError, OSError, zstd.ZstdError, UnicodeDecodeError) as e:
                problems.append(f"{folder.name}, line {number} ({record_id}): {e}")
                continue
            name = str(record_id).replace(":", "_") + ".json"
            (out / name).write_text(text, encoding="utf-8")
            written += 1
    return written, problems


def main():
    if len(sys.argv) != 3:
        print("usage: smoke-trace.py <trace root> <out dir>")
        return 2
    root, out_root = Path(sys.argv[1]), Path(sys.argv[2])
    folders = sorted({log.parent for log in root.rglob("trace.jsonl")})
    problems = []
    for folder in folders:
        version = schema_version(folder)
        if version != 3:
            print(f"{folder.name}: schema {version}, left as it is")
            continue
        written, found = decode_folder(folder, out_root)
        problems += found
        print(f"{folder.name}: schema 3, {written} payload(s) decoded")
    for problem in problems:
        print(f"problem: {problem}")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
