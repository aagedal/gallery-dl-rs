#!/usr/bin/env python3
"""Genuine Gelbooru favorite count/order/JSON traversal and job oracle.

Real FavoriteExtractor metadata/posts/_pagination/_pagination_reverse/items,
request_json/_api_request, DownloadJob, PathFormat and MetadataPP run. Only
request response bytes and downloaded bytes are controlled. Page response maps
are keyed by requested pid and do not implement order selection. Default skip
is preserved; typed datetimes retain NullDatetime identity rather than year1.
"""
import copy
import collections
import datetime
import hashlib
import inspect
import json
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace
from urllib.parse import parse_qsl, urlsplit
import sqlite3

import gallery_dl
from gallery_dl import config, extractor, formatter, job, path, dt, util, text
from gallery_dl.extractor import common, booru, gelbooru, gelbooru_v02
from gallery_dl.postprocessor import metadata as metadata_pp, common as pp_common
from booru_search_generate import input_post

HOSTS = {"gelbooru": "gelbooru.com"}
STAMP = 1704164645


def typed(value):
    if isinstance(value, datetime.datetime):
        return {"$datetime": value.isoformat(), "$class": type(value).__name__,
                "$str": str(value), "$truthy": bool(value)}
    if isinstance(value, dict):
        return {key: typed(item) for key, item in value.items()}
    if isinstance(value, (tuple, list)):
        return [typed(item) for item in value]
    return value

def run_case(name, probe, pages=None, posts=None, options=None, hook_options=None, consume_limit=None,
             query="page=favorites&s=view&id=7"):
    family = "gelbooru"
    responses = []
    pages = {} if pages is None else pages
    posts = {} if posts is None else posts
    url = f"https://{HOSTS[family]}/index.php?{query}"
    config.clear()
    with TemporaryDirectory(prefix="booru-defaults-") as temp:
        base = Path(temp)
        obj = extractor.find(url)
        category = obj.category
        opts = {"base-directory": temp, "async": False,
                "sleep": 0, "sleep-skip": 0, **(options or {})}
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
        calls = []
        response_index = 0
        def transport(request_url, params=None, **kwargs):
            nonlocal response_index
            wire = dict(parse_qsl(urlsplit(request_url).query, keep_blank_values=True))
            wire.update({key: value for key, value in (params or {}).items() if value is not None})
            calls.append({"method": "json", "url": request_url, "params": copy.deepcopy(params), "wire_params": wire})
            if len(calls) > 500:
                raise RuntimeError("OracleRequestBoundExceeded")
            if params.get("s") == "favorite":
                body = probe if params.get("limit") == "2" else pages.get(str(params.get("pid")), {"favorite": []})
            else:
                pid = params["id"]
                body = posts.get(str(pid), {"post": [dict(input_post(family), id=pid)]})
            body = copy.deepcopy(body)
            responses.append({"kind": "json", "body": body, "status": 200})
            response_index += 1
            return SimpleNamespace(text=json.dumps(body, ensure_ascii=False))
        # Only byte transport is replaced; common.request_json and Gelbooru's
        # real _api_request select, wrap and validate source response shapes.
        obj.request = transport
        real_metadata = obj.metadata
        source_metadata = None
        def metadata():
            nonlocal source_metadata
            result = real_metadata()
            source_metadata = copy.deepcopy(result)
            return result
        obj.metadata = metadata
        current = job.DownloadJob(obj)
        current.out = SimpleNamespace(success=lambda value: None, skip=lambda value: None)
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
            return {key: typed(copy.deepcopy(value)) for key, value in data.items()
                    if key not in {"_path", "_extr"}}
        def checkpoint(event, pathfmt):
            trace.append({"event": event, "metadata": snapshot(pathfmt.kwdict),
                          "directory": relative(pathfmt.directory), "filename": pathfmt.filename,
                          "path": relative(pathfmt.path), "realpath": relative(pathfmt.realpath),
                          "temppath": relative(pathfmt.temppath), "extension": pathfmt.extension,
                          "archive_key": formatter.parse(obj.archive_fmt).format_map(pathfmt.kwdict)})
        real_initialize = current.initialize
        def initialize(data=None):
            real_initialize(data)
            if not current.hooks:
                current.hooks = collections.defaultdict(list)
            # Source initialize installed configured real MetadataPP callbacks.
            # Add checkpoint observers AFTER mutations; methods/order untouched.
            for event in ("post", "prepare", "file", "after", "post-after", "skip"):
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
        stopped_by_consumer = False
        source_iterator = iter(obj.items())
        def messages():
            nonlocal stopped_by_consumer
            media_count = 0
            if consume_limit == 0:
                stopped_by_consumer = True
                return
            for kind, message_url, data in source_iterator:
                extraction.append({"kind": "post" if kind == common.Message.Directory else "media",
                                   "url": message_url, "metadata": snapshot(data)})
                yield kind, message_url, data
                if kind == common.Message.Url:
                    media_count += 1
                    if consume_limit is not None and media_count >= consume_limit:
                        stopped_by_consumer = True
                        return
        error = None
        try:
            current.dispatch(messages())
        except Exception as exc:
            error = {"class": type(exc).__name__, "message": str(exc)}
            current.status = 1
        finally:
            source_iterator.close()
            current.handle_finalize()
        published = [{"path": str(file.relative_to(base)), "data_hex": file.read_bytes().hex()}
                     for file in sorted(base.rglob("*")) if file.is_file()]
        result = {"name": family + "_" + name, "family": family, "category": category, "url": url,
                  "extractor": type(obj).__name__, "subcategory": obj.subcategory,
                  "captured_id": obj.groups[-1], "source_metadata": source_metadata,
                  "per_page": obj.per_page,
                  "directory_format": list(obj.directory_fmt), "filename_format": obj.filename_fmt,
                  "archive_format": obj.archive_fmt, "options": options or {},
                  "hook_options": hook_options or [], "probe_response": probe, "favorite_pages": pages, "post_responses": posts,
                  "responses": responses, "download_body": "source-default-path-fixture",
                  "consume_limit": consume_limit, "stopped_by_consumer": stopped_by_consumer,
                  "calls": calls, "extraction_events": extraction, "trace": trace,
                  "published": published, "error": error,
                  "request_count": response_index}
    config.clear()
    return result




def entry(entry_id, post_id, added=STAMP):
    return {"id": entry_id, "favorite": post_id, "added": added}


def probe(count=2, entries=None):
    return {"@attributes": {"count": count}, "favorite": [entry(2, 42), entry(1, 41)] if entries is None else entries}


def run(name, probe_value=None, pages=None, posts=None, options=None, hooks=None, limit=None, query="page=favorites&s=view&id=7"):
    case = run_case(name, probe() if probe_value is None else probe_value,
                    {"0": {"favorite": [entry(2, 42), entry(1, 41)]}} if pages is None else pages,
                    posts, options, hooks, limit, query)
    archives, artifacts = [], []
    for artifact in case["published"]:
        if artifact["path"] == "metadata.sqlite":
            con = sqlite3.connect(":memory:")
            con.deserialize(bytes.fromhex(artifact["data_hex"]))
            archives = [row[0] for row in con.execute("SELECT entry FROM archive ORDER BY entry")]
            con.close()
        else:
            artifacts.append(artifact)
    case["archive_sql"] = archives
    case["published"] = artifacts
    return case


def main():
    def modify(event, fields):
        return {"name": "metadata", "mode": "modify", "event": event, "fields": fields}
    cases = [run("descending_default")]
    for lexical in ("0", "0007", "7suffix"):
        cases.append(run("owner_" + lexical, query="page=favorites&s=view&id=" + lexical))
    cases.append(run("forward_short_pages_continue_until_empty", probe(3), {
        "0": {"favorite": [entry(3, 43)]}, "1": {"favorite": [entry(2, 42)]}, "2": {"favorite": [entry(1, 41)]}}))
    asc = probe(201, [entry(1, 41), entry(2, 42)])
    reverse_pages = {"2": {"favorite": [entry(201, 44)]},
                     "1": {"favorite": [entry(101, 42), entry(102, 43)]},
                     "0": {"favorite": [entry(1, 40), entry(2, 41)]}}
    cases.append(run("ascending_reverse_pages", asc, reverse_pages))
    cases.append(run("zero_count_reverse_pid_minus_one", probe(0, [entry(1, 41), entry(2, 42)]), {}))
    for name, entries in (("numeric_compare", [entry(10, 42), entry(2, 41)]),
                           ("lexical_compare", [entry("10", 42), entry("2", 41)])):
        cases.append(run(name, probe(2, entries), {"0": {"favorite": entries}}))
    order_values = [None, False, True, 0, 1, [], ["a"], ["asc"], [{}], {}, {"0": "a"},
                    "asc", "reverse", "random", "descending", "Reverse"]
    for index, value in enumerate(order_values):
        cases.append(run("order_option_" + str(index), options={"order-posts": value}))
    cases.append(run("empty_probe_fallback_descending", probe(0, []), {}))
    cases.append(run("single_probe_fallback_descending", probe(1, [entry(1, 42)]), {"0": {"favorite": [entry(1, 42)]}}))
    for name, value in (("missing_favorite", {"@attributes": {"count": 2}}),
                         ("missing_count", {"@attributes": {}, "favorite": []}),
                         ("null_favorite", {"@attributes": {"count": 2}, "favorite": None}),
                         ("string_count_forward_ignored", probe("2")),
                         ("string_count_reverse_error", probe("2", [entry(1, 41), entry(2, 42)]))):
        cases.append(run(name, value))
    cases.append(run("single_object_favorite_page", probe(1), {"0": {"favorite": entry(2, 42)}}))
    cases.append(run("reverse_single_object_page_error", probe(1, [entry(1, 41), entry(2, 42)]), {"0": {"favorite": entry(2, 42)}}))
    cases.append(run("missing_favorite_page_ends", pages={"0": {}}))
    cases.append(run("duplicate_posts_default_skip", probe(3), {"0": {"favorite": [entry(3, 42), entry(2, 42), entry(1, 41)]}}))
    cases.append(run("zero_post_results_continue", posts={"42": {"post": []}}))
    cases.append(run("multiple_post_results_per_entry", posts={"42": {"post": [dict(input_post("gelbooru"), id=42), dict(input_post("gelbooru"), id=43)]}}))
    cases.append(run("late_invalid_post_body", posts={"41": {"post": None}}))
    cases.append(run("cancel_before_probe", limit=0))
    cases.append(run("cancel_first_post_before_next_entry", limit=1))
    cases.append(run("cancel_page_boundary_before_next_request", probe(1), {"0": {"favorite": [entry(1, 42)]}}, limit=1))
    cases.append(run("configured_auth", options={"api-key": "SYNTHETIC_KEY", "user-id": "12345678"}))
    added_values = [None, False, True, 1704164645.9, "1704164645.9", "１７０４１６４６４５", "1_704_164_645", "bad", {}, 10**100]
    for index, value in enumerate(added_values):
        cases.append(run("added_type_" + str(index), probe(1), {"0": {"favorite": [entry(1, 42, value)]}}))
    missing_added = entry(1, 42)
    del missing_added["added"]
    cases.append(run("missing_added_fatal_after_post_fetch", probe(1), {"0": {"favorite": [missing_added]}}))
    cases.append(run("missing_added_empty_post_no_error", probe(1), {"0": {"favorite": [missing_added]}}, posts={"42": {"post": []}}))
    for event in ("post", "prepare", "file", "after", "post-after"):
        cases.append(run("scalar_hooks_" + event, hooks=[modify(event, {"favorite_id": event.upper(), "date_favorited": event.upper()})]))
    cases.append(run("delete_scalar_file", hooks=[{"name": "metadata", "mode": "delete", "event": "file", "fields": ["favorite_id", "date_favorited"]}]))
    cases.append(run("user_num_post", hooks=[modify("post", {"num": "{id!i:A+57/}"})]))
    cases.append(run("directory_override", options={"directory": ["custom", "{favorite_id}"]}))
    cases.append(run("filename_override", options={"filename": "chosen_{id}.{extension}"}))
    cases.append(run("both_overrides", options={"directory": [], "filename": "chosen_{filename}.{extension}"}))
    cases.append(run("conditional_override", options={"directory": {"id == 42": ["match"], "": ["other"]}, "filename": {"id == 42": "match_{id}.{extension}", "": "other_{id}.{extension}"}}))
    archive_observer = {"name": "metadata", "mode": "jsonl", "event": "file", "filename": "files.jsonl",
                        "base-directory": True, "directory": [], "archive": [":basedirectory", "metadata.sqlite"],
                        "include": ["favorite_id", "id", "category", "filename", "extension", "tags", "date_favorited"]}
    cases.append(run("actual_metadata_archive", hooks=[archive_observer]))
    cases.append(run("actual_mutated_metadata_archive", hooks=[modify("file", {"favorite_id": "HOOK"}), archive_observer]))
    for name, added in (("valid_date_formatting", STAMP), ("invalid_date_formatting", "bad")):
        hooks = [{"name": "metadata", "mode": "custom", "event": "file", "filename": "date.txt", "format": "{date_favorited}|{date_favorited:%Y-%m-%d}"},
                 {"name": "metadata", "mode": "custom", "event": "file", "filename": "truthy.txt", "format": "truthy", "filter": "bool(date_favorited)"},
                 {"name": "metadata", "mode": "custom", "event": "file", "filename": "false.txt", "format": "false", "filter": "not bool(date_favorited)"}]
        cases.append(run(name, probe(1), {"0": {"favorite": [entry(1, 42, added)]}}, hooks=hooks))
    cases.append(run("scalar_skip_duplicate", probe(3), {"0": {"favorite": [entry(3, 42), entry(2, 42), entry(1, 41)]}},
                     hooks=[modify("skip", {"favorite_id": "SKIP", "date_favorited": "SKIP"})]))
    cases.append(run("missing_attributes_probe", {"favorite": []}))
    cases.append(run("null_count_reverse_error", probe(None, [entry(1, 41), entry(2, 42)])))
    cases.append(run("float_count_reverse_pages", probe(201.5, [entry(1, 41), entry(2, 42)]), {
        "2.0": {"favorite": [entry(201, 44)]}, "1.0": {"favorite": [entry(101, 43)]},
        "0.0": {"favorite": [entry(1, 42)]}}))
    cases.append(run("empty_string_probe_fallback_descending", probe(2, "")))
    cases.append(run("nonempty_string_probe_error", probe(2, "bad")))
    timestamp_cases = []
    for value in added_values:
        date = dt.parse_ts(value)
        data = {"date_favorited": date}
        timestamp_cases.append({"input": value, "date": typed(date),
                                "plain": formatter.parse("{date_favorited}").format_map(data),
                                "year": formatter.parse("{date_favorited:%Y}").format_map(data),
                                "truthy": util.compile_filter("bool(date_favorited)")(data),
                                "json": json.dumps(data, default=util.json_default)})
    routes = []
    for url in ("https://gelbooru.com/index.php?page=favorites&s=view&id=7", "https://gelbooru.com/?page=favorites&s=view&id=0007",
                "https://gelbooru.com/index.php?page=favorites&s=view&id=7suffix", "https://gelbooru.com/index.php?s=view&page=favorites&id=7",
                "https://gelbooru.com/index.php?page=favorites&s=view&id=0", "https://gelbooru.com/index.php?page=favorites&s=view&id=-7"):
        obj = extractor.find(url)
        routes.append({"url": url, "extractor": type(obj).__name__ if obj else None,
                       "captured_id": obj.groups[-1] if obj else None,
                       "archive_format": obj.archive_fmt if obj else None})
    expected_errors = {"order_option_2", "order_option_4", "order_option_8", "order_option_10",
                       "missing_favorite", "missing_count", "null_favorite", "string_count_reverse_error",
                       "reverse_single_object_page_error", "late_invalid_post_body", "missing_added_fatal_after_post_fetch",
                       "missing_attributes_probe", "null_count_reverse_error", "nonempty_string_probe_error"}
    assert {case["name"].removeprefix("gelbooru_") for case in cases if case["error"]} == expected_errors
    root = Path(inspect.getfile(gallery_dl)).parent
    modules = (common, booru, gelbooru, gelbooru_v02, config, formatter, job, path, dt, text, metadata_pp, pp_common)
    output = {"version": gallery_dl.version.__version__, "generator": __doc__,
              "source_sha256": {str(Path(inspect.getfile(module)).relative_to(root)):
                                hashlib.sha256(Path(inspect.getfile(module)).read_bytes()).hexdigest()
                                for module in modules},
              "input_schema_sha256": hashlib.sha256(Path(__file__).with_name("booru_search_generate.py").read_bytes()).hexdigest(),
              "source_skip_default": True, "excluded_internal_artifacts": ["metadata.sqlite"],
              "timestamp_cases": timestamp_cases, "route_cases": routes, "cases": cases}
    Path(__file__).with_name("booru_gel_favorite.json").write_text(json.dumps(output, ensure_ascii=False, indent=2) + "\n")


if __name__ == "__main__":
    main()
