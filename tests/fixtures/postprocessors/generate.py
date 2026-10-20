"""Regenerate with the installed, unmodified gallery-dl package; no network."""
import datetime
import hashlib
import json
import logging
import os
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace

import gallery_dl.version
from gallery_dl.postprocessor import hash as hash_module, mtime as mtime_module


class Job:
    def __init__(self):
        self.hooks = []

    def get_logger(self, name):
        return logging.getLogger(name)

    def register_hooks(self, hooks, options):
        self.hooks.extend(hooks)


class PathFormat:
    def __init__(self, directory, context):
        self.realpath = self.temppath = str(directory / "input")
        self.realdirectory = str(directory)
        self.kwdict = context.copy()
        self.rebuild = 0

    def build_path(self):
        self.rebuild += 1


hash_options = [
    ("default", {}),
    ("all_fixed_hashlib_algorithms", {"hashes": "md5,sha1,sha224,sha256,sha384,sha512,sha512_224,sha512_256,sha3_224,sha3_256,sha3_384,sha3_512,blake2b,blake2s"}),
    ("dictionary", {"mode": {"digest": "sha256", "md5_alias": "md5"}}),
    ("string_keys", {"hashes": "sha256:custom,md5:"}),
    ("pairs_duplicate_key", {"mode": [["digest", "md5"], ["digest", "sha1"]]}),
    ("mode_precedes_hashes", {"mode": "sha1", "hashes": "sha256"}),
    ("empty_mode_falls_back", {"mode": {}, "hashes": "sha256"}),
    ("empty_hash_dictionary", {"hashes": {}}),
    ("empty_hash_list_defaults", {"hashes": []}),
    ("false_hash_defaults", {"hashes": False}),
    ("zero_chunk_hashes_empty", {"chunk-size": 0}),
    ("negative_chunk_reads_all", {"chunk-size": -1}),
    ("small_chunk", {"chunk-size": 1}),
    ("boolean_chunk", {"chunk-size": True}),
    ("false_chunk", {"chunk-size": False}),
    ("filename_rebuild", {"filename": ["enabled"]}),
    ("filename_false", {"filename": {}}),
    ("openssl_aliases", {"hashes": "SHA256,sha-512,sha3-256"}),
    ("custom_events", {"event": "prepare,after"}),
]
mtime_options = [
    ("default_numeric", {}, {"date": 1704164646}),
    ("default_numeric_string", {}, {"date": "1704164646"}),
    ("missing_value", {}, {}),
    ("zero_value", {}, {"date": 0}),
    ("null_value", {}, {"date": None}),
    ("malformed_numeric", {}, {"date": "not-a-timestamp"}),
    ("float_truncation", {}, {"date": 1704164646.875}),
    ("negative_float", {}, {"date": -123.75}),
    ("datetime", {}, {"date": {"$datetime": "2024-01-02T03:04:06"}}),
    ("custom_key", {"key": "created"}, {"created": 1704164646}),
    ("template_value", {"value": "{created}"}, {"created": 1704164646}),
    ("directory_target", {"target": "directory"}, {"date": 1704164646}),
    ("directory_mode", {"mode": "dir"}, {"date": "1704164646"}),
    ("directory_zero_ignored", {"target": "dir"}, {"date": 0}),
    ("custom_events", {"event": "prepare,after"}, {"date": 1704164646}),
]
result = {"source": "gallery-dl", "version": gallery_dl.version.__version__,
          "generator": "generate.py", "modules": {}, "hash_cases": [], "mtime_cases": []}
for module in [hash_module, mtime_module]:
    path = Path(module.__file__)
    result["modules"][path.name] = {"sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
for name, options in hash_options:
    with TemporaryDirectory() as raw:
        directory = Path(raw)
        job = Job()
        pp = hash_module.HashPP(job, options)
        path = PathFormat(directory, {"unchanged": "preserved"})
        Path(path.realpath).write_bytes(b"abc")
        pp.run(path)
        result["hash_cases"].append({"name": name, "options": options, "input": "abc",
                                    "context": {"unchanged": "preserved"},
                                    "result": path.kwdict, "rebuild": bool(path.rebuild),
                                    "hooks": job.hooks})
for name, options, context in mtime_options:
    with TemporaryDirectory() as raw:
        directory = Path(raw)
        job = Job()
        pp = mtime_module.MtimePP(job, options)
        raw_context = {key: datetime.datetime.fromisoformat(value["$datetime"]) if isinstance(value, dict) and "$datetime" in value else value for key,value in context.items()}
        path = PathFormat(directory, raw_context)
        Path(path.realpath).write_bytes(b"abc")
        os.utime(directory, (1000000000, 1000000000))
        pp.run(path)
        result["mtime_cases"].append({"name": name, "options": options, "context": context,
                                     "result": {key: {"$datetime": value.isoformat()} if isinstance(value, datetime.datetime) else value for key,value in path.kwdict.items()}, "hooks": job.hooks,
                                     "directory_mtime": int(directory.stat().st_mtime)})
Path(__file__).with_name("source.json").write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
