#!/usr/bin/env python3
"""Offline source default paths using actual extractor/DownloadJob/PathFormat.

Real source metadata(), posts(), items(), dispatch(), initialize(), URL and
Directory handling, MetadataPP, PathFormat and publication run. Pagination is
replaced by a controlled iterator of full API posts; request_json supplies only
single-post/collection responses. Download bytes are written inside a temporary
base directory. No network, fake path object or copied format strings are used.
"""
import copy
import collections
import hashlib
import inspect
import json
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace

import gallery_dl
from gallery_dl import config, extractor, formatter, job, path
from gallery_dl.extractor import booru, common, danbooru, e621, gelbooru, gelbooru_v02
from gallery_dl.postprocessor import metadata as metadata_pp, common as pp_common, hash as hash_pp
from booru_search_generate import input_post, normalize

ROOTS = [
    ("danbooru_post", "danbooru", "https://danbooru.donmai.us/posts/42"),
    ("danbooru_tag", "danbooru", "https://danbooru.donmai.us/posts?tags=Cat+Dog"),
    ("danbooru_pool", "danbooru", "https://danbooru.donmai.us/pools/7"),
    ("danbooru_favgroup", "danbooru", "https://danbooru.donmai.us/favorite_groups/7"),
    ("e621_post", "e621", "https://e621.net/posts/42"),
    ("e621_tag", "e621", "https://e621.net/posts?tags=Cat+Dog"),
    ("e621_pool", "e621", "https://e621.net/pools/7"),
    ("e926_post", "e621", "https://e926.net/posts/42"),
    ("e926_tag", "e621", "https://e926.net/posts?tags=Cat+Dog"),
    ("e926_pool", "e621", "https://e926.net/pools/7"),
    ("gelbooru_post", "gelbooru", "https://gelbooru.com/index.php?page=post&s=view&id=42"),
    ("gelbooru_tag", "gelbooru", "https://gelbooru.com/index.php?page=post&s=list&tags=Cat+Dog"),
    ("safebooru_post", "safebooru", "https://safebooru.org/index.php?page=post&s=view&id=42"),
    ("safebooru_tag", "safebooru", "https://safebooru.org/index.php?page=post&s=list&tags=Cat+Dog"),
]


def run_case(root, name, overrides=None, hook_options=None, count=1, input_overrides=None, remove_fields=(), collection_name=None):
    label, family, url = root
    config.clear()
    with TemporaryDirectory(prefix="booru-defaults-") as temp:
        base = Path(temp)
        obj = extractor.find(url)
        category = obj.category
        opts = {"base-directory": temp, "skip": False, "async": False,
                "sleep": 0, "sleep-skip": 0, **(overrides or {})}
        if hook_options:
            opts["postprocessors"] = copy.deepcopy(hook_options)
        for key, value in opts.items():
            config.set(("extractor", category), key, value)
        obj._get_auth_info = lambda: (None, None)
        obj.login = lambda: None
        obj.cookies_store = lambda: None
        obj.ugoira = False
        obj.external = False
        obj.includes = False
        obj.kwdict = {}
        inputs = [dict(copy.deepcopy(input_post(family)), id=42 - i) for i in range(count)]
        def overlay(target, changes):
            for key, value in changes.items():
                if isinstance(value, dict) and isinstance(target.get(key), dict):
                    overlay(target[key], value)
                else:
                    target[key] = copy.deepcopy(value)
        for row in inputs:
            overlay(row, input_overrides or {})
            for field in remove_fields:
                row.pop(field, None)
        collection = {"id": 99, "name": "my_collection_name" if collection_name is None else collection_name, "post_ids": [row["id"] for row in inputs],
                      "description": "raw_<b>description</b>"}
        calls = []
        def pagination(*args, **kwargs):
            calls.append({"method": "_pagination", "args": copy.deepcopy(args), "kwargs": copy.deepcopy(kwargs)})
            return iter(copy.deepcopy(inputs))
        def request_json(request_url, params=None):
            calls.append({"method": "request_json", "url": request_url, "params": copy.deepcopy(params)})
            if "/pools/" in request_url or "/favorite_groups/" in request_url:
                return copy.deepcopy(collection)
            result = copy.deepcopy(inputs[0])
            return {"post": result} if family == "e621" else result
        obj._pagination = pagination
        obj.request_json = request_json
        current = job.DownloadJob(obj)
        current.out = SimpleNamespace(success=lambda value: None)
        current._init()
        trace, extraction = [], []
        def relative(value):
            if not value:
                return ""
            value = str(value)
            return value.replace(temp + "/", "").replace(temp, "$BASE")
        def snapshot(data):
            # Actual Job injects live private proxies, not serializable metadata.
            # Leave them in the real dict; omit only from recorded snapshots.
            return {key: normalize(copy.deepcopy(value)) for key, value in data.items()
                    if key not in {"_path", "_extr"}}
        def checkpoint(event, pathfmt):
            trace.append({"event": event, "metadata": snapshot(pathfmt.kwdict),
                          "directory": relative(pathfmt.directory), "filename": pathfmt.filename,
                          "path": relative(pathfmt.path), "realpath": relative(pathfmt.realpath),
                          "temppath": relative(pathfmt.temppath), "extension": pathfmt.extension})
        real_initialize = current.initialize
        def initialize(data=None):
            real_initialize(data)
            if not current.hooks:
                current.hooks = collections.defaultdict(list)
            # Source initialize installed configured real MetadataPP callbacks.
            # Add checkpoint observers AFTER mutations; methods/order untouched.
            for event in ("post", "prepare", "file", "after", "post-after"):
                current.register_hooks({event: lambda p, event=event: checkpoint(event, p)})
        current.initialize = initialize
        def download(download_url):
            pathfmt = current.pathfmt
            if not pathfmt.temppath:
                pathfmt.fix_extension()
            target = Path(pathfmt.temppath)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(b"source-default-path-fixture")
            return True
        current.download = download
        def messages():
            for kind, message_url, data in obj.items():
                extraction.append({"kind": "post" if kind == common.Message.Directory else "media",
                                   "url": message_url, "metadata": snapshot(data)})
                yield kind, message_url, data
        error = None
        try:
            current.dispatch(messages())
        except Exception as exc:
            error = {"class": type(exc).__name__, "message": str(exc)}
            current.status = 1
        finally:
            current.handle_finalize()
        published = [{"path": str(file.relative_to(base)), "data_hex": file.read_bytes().hex()}
                     for file in sorted(base.rglob("*")) if file.is_file()]
        result = {"name": label + "_" + name, "family": family, "url": url,
                  "extractor": type(obj).__name__, "category": category, "subcategory": obj.subcategory,
                  "directory_format": list(obj.directory_fmt), "filename_format": obj.filename_fmt,
                  "archive_format": obj.archive_fmt, "overrides": overrides or {},
                  "hook_options": hook_options or [], "input_posts": inputs,
                  "collection_response": collection if obj.subcategory in ("pool", "favgroup") else None,
                  "calls": calls, "extraction_events": extraction, "trace": trace,
                  "published": published, "error": error}
    config.clear()
    return result


def main():
    def modify(event, fields):
        return {"name": "metadata", "mode": "modify", "event": event, "fields": fields}
    cases = []
    for root in ROOTS:
        cases.append(run_case(root, "default"))
        cases.append(run_case(root, "directory_override", {"directory": ["custom", "{category}", "{id}"]}))
        cases.append(run_case(root, "filename_override", {"filename": "custom_{id}.{extension}"}))
        cases.append(run_case(root, "both_overrides", {"directory": ["both", "{id}"], "filename": "chosen_{filename}.{extension}"}))
        cases.append(run_case(root, "conditional_overrides", {
            "directory": {"id == 42": ["matched", "{category}"], "": ["fallback"]},
            "filename": {"id == 42": "match_{id}.{extension}", "": "fallback.{extension}"}}))
        cases.append(run_case(root, "conditional_fallback", {
            "directory": {"id == 999": ["matched", "{category}"], "": ["fallback"]},
            "filename": {"id == 999": "match_{id}.{extension}", "": "fallback.{extension}"}}))
        cases.append(run_case(root, "conditional_unmatched_defaults", {
            "directory": {"id == 999": ["matched"]},
            "filename": {"id == 999": "match.{extension}"}}))
        cases.append(run_case(root, "empty_directory", {"directory": []}))
        cases.append(run_case(root, "post_mutation", hook_options=[modify("post", {"id": "800", "filename": "POST", "extension": "jpeg"})]))
        cases.append(run_case(root, "prepare_mutation", hook_options=[modify("prepare", {"id": "900", "filename": "PREPARE", "extension": "png"})]))
        cases.append(run_case(root, "file_mutation", hook_options=[modify("file", {"id": "1000", "filename": "FILE", "extension": "gif"})]))
    multi = next(root for root in ROOTS if root[0] == "danbooru_tag")
    cases.append(run_case(multi, "multi_post_directory_checkpoint", count=2, hook_options=[
        modify("post", {"search_tags": "POST_TAG", "filename": "POST"}),
        modify("prepare", {"search_tags": "PREPARE_TAG", "filename": "PREPARE"}),
        modify("file", {"search_tags": "FILE_TAG", "filename": "FILE"}),
        modify("post-after", {"search_tags": "AFTER_TAG", "filename": "AFTER"}),
    ]))
    # Representative path-cleaning and source keyword-default cases; avoid
    # multiplying every edge over all fourteen route classes.
    for name, query in (("empty_search_tags", ""), ("whitespace_search_tags", "+%20+")):
        cases.append(run_case(("danbooru_tag", "danbooru", "https://danbooru.donmai.us/posts?tags=" + query), name))
    for label in ("danbooru_pool", "danbooru_favgroup", "e621_pool"):
        root_case = next(root for root in ROOTS if root[0] == label)
        cases.append(run_case(root_case, "collection_name_cleaning", collection_name="  猫 café_/ spaced \tcontrol\nline\x01  "))
    for label in ("gelbooru_post", "safebooru_post"):
        root_case = next(root for root in ROOTS if root[0] == label)
        cases.append(run_case(root_case, "missing_md5", remove_fields=("md5",)))
        cases.append(run_case(root_case, "null_md5", input_overrides={"md5": None}))
    cases.append(run_case(next(root for root in ROOTS if root[0] == "e621_post"), "null_md5", input_overrides={"file": {"md5": None}}))
    # Metadata extension-format temporarily modifies extension and rebuilds the
    # sidecar basename with the retained extractor/configured formatter. File
    # mutations do not move the cached download directory.
    cases.append(run_case(multi, "metadata_extension_format_after_file_mutation", hook_options=[
        modify("file", {"search_tags": "FILE_DIR", "filename": "FILE_NAME", "extension": "gif"}),
        {"name": "metadata", "mode": "custom", "event": "file", "format": "sidecar", "extension-format": "{extension}.note"},
    ]))
    cases.append(run_case(multi, "metadata_extension_format_with_filename_override", overrides={"filename": "chosen_{filename}.{extension}"}, hook_options=[
        modify("file", {"filename": "FILE_NAME", "extension": "gif"}),
        {"name": "metadata", "mode": "custom", "event": "file", "format": "sidecar", "extension-format": "{extension}.note"},
    ]))
    for rebuild in (False, True):
        cases.append(run_case(multi, "hash_rebuild_" + str(rebuild).lower(), hook_options=[
            modify("file", {"search_tags": "FILE_DIR", "filename": "FILE_NAME", "extension": "gif"}),
            {"name": "hash", "event": "file", "mode": "md5", "filename": rebuild},
        ]))
    gel_post = next(root for root in ROOTS if root[0] == "gelbooru_post")
    for rebuild in (False, True):
        cases.append(run_case(gel_post, "hash_digest_rebuild_" + str(rebuild).lower(),
                              hook_options=[{"name": "hash", "event": "file", "mode": "md5", "filename": rebuild}]))
    for event in ("file", "post-after"):
        cases.append(run_case(multi, "prepare_extension_cache_metadata_" + event,
                              hook_options=[
                                  modify("prepare", {"extension": "gif"}),
                                  {"name": "metadata", "mode": "custom", "event": event,
                                   "format": "sidecar", "extension-format": "{extension}.note"},
                              ]))
    cases.append(run_case(next(root for root in ROOTS if root[0] == "danbooru_pool"),
                          "override_filename_ignores_invalid_default_num",
                          overrides={"filename": "override.{extension}"},
                          hook_options=[modify("post", {"num": "{pool}"})]))
    assert all(case["error"] is None for case in cases), [(case["name"], case["error"]) for case in cases if case["error"]]
    assert all(case["published"] for case in cases)
    root = Path(inspect.getfile(gallery_dl)).parent
    modules = (booru, common, danbooru, e621, gelbooru, gelbooru_v02, job, path, formatter, metadata_pp, pp_common, hash_pp)
    output = {"version": gallery_dl.version.__version__, "generator": __doc__,
              "source_sha256": {str(Path(inspect.getfile(module)).relative_to(root)):
                                hashlib.sha256(Path(inspect.getfile(module)).read_bytes()).hexdigest()
                                for module in modules},
              "input_schema_sha256": hashlib.sha256(Path(__file__).with_name("booru_search_generate.py").read_bytes()).hexdigest(),
              "omitted_private_snapshot_keys": ["_path", "_extr"],
              "checkpoint_observers": "Registered after configured callbacks; observers do not build or modify paths. Actual source handle_url builds target after Prepare and before File. Published files are collected after actual finalize.",
              "cases": cases}
    Path(__file__).with_name("booru_defaults.json").write_text(json.dumps(output, ensure_ascii=False, indent=2) + "\n")


if __name__ == "__main__":
    main()
