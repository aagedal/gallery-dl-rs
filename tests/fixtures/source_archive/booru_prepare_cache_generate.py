#!/usr/bin/env python3
"""Actual Danbooru DownloadJob Prepare HashPP cached PathFormat lifecycle, offline.

Only pagination/API input and media transport are synthetic. Real source items,
DownloadJob.run, PathFormat methods, HashPP, archive and publication execute.
Wrappers record state without changing it. Preview paths for preseed setup use a
separate real extractor+PathFormat, never copied source default format strings.
"""
import copy
import hashlib
import inspect
import json
import logging
import re
from pathlib import Path
import sqlite3
from tempfile import TemporaryDirectory
from unittest.mock import patch

import gallery_dl
from gallery_dl import config, extractor, job, path
from gallery_dl.extractor import common, danbooru
from gallery_dl.postprocessor import hash as hash_pp
from booru_search_generate import input_post, normalize

URL = "https://danbooru.donmai.us/posts?tags=cache"
KEYS = ("id", "category", "subcategory", "filename", "extension", "search_tags",
        "previous", "first", "second", "third")


class GroupedDanbooruFixture(danbooru.DanbooruTagExtractor):
    """Synthetic multi-file emitter using genuine Danbooru per-file preparation."""
    def items(self):
        shared = None
        for kind, message_url, data in super().items():
            if kind == common.Message.Directory:
                if shared is None:
                    shared = data
                    yield kind, message_url, shared
                else:
                    shared.clear()
                    shared.update(data)
            else:
                yield kind, message_url, shared


def configure(base, overrides, hooks):
    config.clear()
    config.set(("output",), "mode", "null")
    for key, value in {"base-directory": str(base), "part": False,
                       "sleep": 0, "sleep-skip": 0, "async": False,
                       "metadata-path": False, "metadata-url": False,
                       "metadata-extractor": False, "postprocessors": hooks,
                       **overrides}.items():
        config.set(("extractor", "danbooru"), key, copy.deepcopy(value))
    for name in ("DOWNLOAD", "FILE", "POST", "CHILD"):
        setattr(job.FLAGS, name, None)


def make_extractor(inputs, calls=None, grouped=False):
    obj = extractor.find(URL)
    if grouped:
        obj = GroupedDanbooruFixture(re.match(obj.pattern, URL))
    obj._get_auth_info = lambda: (None, None)
    obj.cookies_store = lambda: None
    obj.ugoira = obj.external = obj.includes = False
    obj.kwdict = {}
    def pagination(*args, **kwargs):
        if calls is not None:
            calls.append({"method": "_pagination", "args": normalize(args),
                          "kwargs": normalize(kwargs)})
        yield from copy.deepcopy(inputs)
    obj._pagination = pagination
    return obj


def source_paths(inputs):
    obj = make_extractor(inputs)
    preview_job = job.DownloadJob(obj)
    preview_job._init()
    actual = path.PathFormat(obj)
    result = {}
    for kind, _, data in obj.items():
        preview_job.update_kwdict(data)
        if kind == common.Message.Directory:
            actual.set_directory(data)
        else:
            actual.set_filename(data)
            actual.build_path()
            result[data["id"]] = actual.realpath
    return result


def run_case(name, *, directory_changes=False, preseed=False, archived=False,
             failed=False, rebuild=False, isolation=False, archive_rebuild=False,
             skip_rebuild=False, three_callbacks=False, grouped=False):
    inputs = []
    for ident in (42, 41, 40):
        row = dict(copy.deepcopy(input_post("danbooru")), id=ident)
        row["file_url"] = f"https://cdn.donmai.us/original/ab/cd/picture{ident}.jpg"
        inputs.append(row)
    overrides = {"directory": ["posts", "{id}"]} if directory_changes else {}
    hooks = [{"name": "hash", "event": "prepare", "filter": "id != 42",
              "hashes": "md5:previous"}]
    if rebuild:
        hooks = [{"name": "hash", "event": "prepare", "filter": "id == 41",
                  "hashes": "md5:first", "filename": True},
                 {"name": "hash", "event": "prepare", "filter": "id == 41",
                  "hashes": "md5:second"}]
    modify = lambda event, fields: {"name": "metadata", "mode": "modify", "event": event,
                                    "filter": "id == 41", "fields": fields}
    if archive_rebuild:
        archived = True
        overrides["filename"] = {"filename == None": "{filename!i}"}
        hooks = [{"name": "hash", "event": "prepare", "filter": "id == 41",
                  "hashes": "md5:first", "filename": True},
                 modify("prepare", {"filename": "{missing}"})]
    if skip_rebuild:
        preseed = True
        hooks += [modify("skip", {"filename": "SKIP_CHANGED"}),
                  {"name": "hash", "event": "skip", "filter": "id == 41",
                   "hashes": "md5:second", "filename": True}]
    if three_callbacks:
        preseed = True
        hooks = [{"name": "hash", "event": "prepare", "filter": "id == 41",
                  "hashes": "md5:first", "filename": True},
                 modify("prepare", {"filename": "SECOND_BUILT"}),
                 {"name": "hash", "event": "prepare", "filter": "id == 41",
                  "hashes": "md5:second", "filename": True},
                 {"name": "hash", "event": "prepare", "filter": "id == 41",
                  "hashes": "md5:third"}]
    with TemporaryDirectory(prefix="booru-prepare-cache-") as temporary:
        base = Path(temporary).resolve()
        configure(base, overrides, hooks)
        preview = source_paths(inputs)
        def relative(value):
            value = str(value)
            return value.replace(str(base) + "/", "").replace(str(base), "$BASE")
        seeds = []
        if preseed:
            target = Path(preview[41])
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(b"OLD-B")
            seeds.append({"path": relative(target), "data_hex": b"OLD-B".hex()})
        before = []
        archive_path = base / "archive.sqlite3"
        if archived:
            before = ["danboorut_cache_41"]
            db = sqlite3.connect(archive_path)
            db.execute("CREATE TABLE archive(entry TEXT PRIMARY KEY) WITHOUT ROWID")
            db.executemany("INSERT INTO archive VALUES (?)", [(key,) for key in before])
            db.commit()
            db.close()
        jobs = []
        for ordinal in range(2 if isolation else 1):
            current_inputs = inputs if ordinal == 0 else [inputs[1]]
            current_hooks = hooks if ordinal == 0 else [
                {"name": "hash", "event": "prepare", "hashes": "md5:previous"}]
            configure(base, overrides, current_hooks)
            if archived:
                config.set(("extractor", "danbooru"), "archive", str(archive_path))
            calls, trace, extraction_events = [], [], []
            obj = make_extractor(current_inputs, calls, grouped=grouped)
            actual = job.DownloadJob(obj)
            def metadata(data):
                return {key: normalize(copy.deepcopy(data[key])) for key in KEYS if key in data}
            def paths(actual_path):
                return {key: relative(getattr(actual_path, key)) for key in (
                    "directory", "filename", "path", "realpath", "temppath", "extension")}
            def record(event, actual_path, **extra):
                item = {"event": event, "id": actual_path.kwdict.get("id"),
                        "metadata": metadata(actual_path.kwdict), "paths": paths(actual_path)}
                item.update(extra)
                trace.append(item)
                return item
            real_initialize = actual.initialize
            def initialize(data=None):
                real_initialize(data)
                for event in ("post", "prepare", "prepare-after", "file", "after", "skip",
                              "error", "post-after", "finalize-error", "finalize"):
                    actual.register_hooks({event: lambda p, event=event: record(event, p)})
            actual.initialize = initialize
            real_items = obj.items
            def items():
                for kind, message_url, data in real_items():
                    extraction_events.append({"kind": "post" if kind == common.Message.Directory else "media",
                                              "url": message_url, "metadata": metadata(data)})
                    yield kind, message_url, data
            obj.items = items
            def download(download_url):
                p = actual.pathfmt
                record("download", p, url=download_url)
                if failed and p.kwdict["id"] == 41:
                    return False
                if not p.temppath:
                    p.fix_extension()
                target = Path(p.temppath)
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(f"BODY-{p.kwdict['id']}".encode())
                return True
            actual.download = download
            original_build = path.PathFormat.build_path
            original_open = hash_pp.HashPP._open
            def build(p):
                record("build.before", p)
                try:
                    result = original_build(p)
                except Exception as exc:
                    record("build.error", p, error=type(exc).__name__)
                    raise
                record("build.after", p)
                return result
            def opened(processor, p):
                item = record("hash.open", p, hash_keys=[key for key, _ in processor.hashes])
                try:
                    fp = original_open(processor, p)
                except Exception as exc:
                    item["error"] = type(exc).__name__
                    raise
                item["selected"] = relative(fp.name)
                return fp
            with patch.object(path.PathFormat, "build_path", build), patch.object(hash_pp.HashPP, "_open", opened):
                status = actual.run()
            after = []
            if archive_path.exists():
                db = sqlite3.connect(archive_path)
                after = [row[0] for row in db.execute("SELECT entry FROM archive ORDER BY entry")]
                db.close()
            jobs.append({"ordinal": ordinal, "input_posts": copy.deepcopy(current_inputs),
                         "hook_options": current_hooks, "calls": calls,
                         "extraction_events": extraction_events, "trace": trace,
                         "status": status, "archive_entries_after": after,
                         "published": [{"path": relative(file), "data_hex": file.read_bytes().hex()}
                                       for file in sorted(base.rglob("*.jpg"))]})
        result = {"name": name, "family": "danbooru", "url": URL,
                  "extractor": type(obj).__name__, "category": "danbooru", "subcategory": "tag",
                  "filename_format": obj.filename_fmt, "directory_format": list(obj.directory_fmt),
                  "archive_format": obj.archive_fmt, "overrides": overrides,
                  "preseed": seeds, "archive_entries_before": before,
                  "failed_ids": [41] if failed else [],
                  "download_body_hex_by_id": {str(row["id"]): f"BODY-{row['id']}".encode().hex() for row in inputs},
                  "preview_paths": {str(key): relative(value) for key, value in preview.items()},
                  "jobs": jobs}
        if grouped:
            result["custom_emitter"] = {
                "class": "GroupedDanbooruFixture",
                "description": "Runs genuine Danbooru.items preparation but emits only the first Directory; copies each later prepared dictionary into the same shared dictionary before its Url.",
                "groups": [{"post_index": 0, "file_indices": list(range(len(inputs)))}],
                "file_keyword_updates": "all prepared per-file fields; dictionary shared across Directory/Urls",
            }
    config.clear()
    return result


def main():
    logging.disable(logging.CRITICAL)
    cases = [run_case("success"), run_case("success_directory_changes", directory_changes=True),
             run_case("existing_B", preseed=True), run_case("archived_missing_B", archived=True),
             run_case("failed_missing_B", failed=True),
             run_case("failed_missing_B_directory_changes", failed=True, directory_changes=True),
             run_case("prepare_rebuild_changes_next_candidate", preseed=True, rebuild=True),
             run_case("separate_job_isolation", isolation=True),
             run_case("archive_hit_retains_prepare_built_path_after_invalid_filename", archive_rebuild=True),
             run_case("skip_hash_rebuild_changes_next_realpath", skip_rebuild=True),
             run_case("three_prepare_hashes_keep_temporary_path_after_second_rebuild", three_callbacks=True),
             run_case("custom_grouped_same_post", grouped=True)]
    root = Path(inspect.getfile(gallery_dl)).parent
    modules = (job, path, common, danbooru, hash_pp)
    output = {"version": gallery_dl.version.__version__, "generator": __doc__,
              "source_sha256": {str(Path(inspect.getfile(module)).relative_to(root)):
                                hashlib.sha256(Path(inspect.getfile(module)).read_bytes()).hexdigest()
                                for module in modules},
              "input_schema_sha256": hashlib.sha256(Path(__file__).with_name("booru_search_generate.py").read_bytes()).hexdigest(),
              "metadata_keys": list(KEYS),
              "scope": "Real DanbooruTagExtractor.items and DownloadJob.run, three one-file posts. Separate fresh job shares artifacts only. One explicitly labeled GroupedDanbooruFixture case changes the emitter grouping only, retaining genuine per-file Danbooru preparation and actual DownloadJob/PathFormat/HashPP lifecycle.",
              "transport": "Only media download replaced by bounded BODY-id writes, or False for selected failure after real path build; no network. Real SQLite archive and actual HashPP file reads.",
              "cases": cases}
    Path(__file__).with_name("booru_prepare_cache.json").write_text(json.dumps(output, ensure_ascii=False, indent=2) + "\n")
    for case in cases:
        print(case["name"], [(j["status"], [(t["id"], t.get("selected"), t.get("error"))
                                             for t in j["trace"] if t["event"] == "hash.open"]) for j in case["jobs"]])


if __name__ == "__main__":
    main()
