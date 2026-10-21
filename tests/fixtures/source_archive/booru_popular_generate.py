#!/usr/bin/env python3
"""Genuine offline Danbooru/E621/E926 Popular source job oracle.

Real metadata/posts/_pagination/items, request_json, DownloadJob, PathFormat,
MetadataPP and file publication run. Only transport bytes/download bytes are
controlled. gallery_dl.dt.date is a deterministic real datetime.date subclass;
metadata normalization algorithms are never replaced. Source default skip is
preserved. Public snapshots retain source string date/scale types.
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
from urllib.parse import parse_qsl, urlsplit, urlencode
import sqlite3

import gallery_dl
from gallery_dl import config, extractor, formatter, job, path, dt, util, text
from gallery_dl.extractor import common, booru, danbooru, e621
from gallery_dl.postprocessor import metadata as metadata_pp, common as pp_common
from booru_search_generate import input_post

TODAY = "2026-10-21"
class FrozenDate(datetime.date):
    @classmethod
    def today(cls):
        return cls(2026, 10, 21)


def typed(value):
    if isinstance(value, datetime.datetime):
        return {"$datetime": value.isoformat(), "$class": type(value).__name__, "$str": str(value), "$truthy": bool(value)}
    if isinstance(value, dict):
        return {key: typed(item) for key, item in value.items()}
    if isinstance(value, (tuple, list)):
        return [typed(item) for item in value]
    return value

def run_case(family, name, pages=None, options=None, hook_options=None, consume_limit=None,
             query=None, route=None):
    category = family
    api_family = "e621" if family == "e926" else family
    host = {"danbooru": "danbooru.donmai.us", "e621": "e621.net", "e926": "e926.net"}[family]
    route = ("/popular" if family == "danbooru" else "/explore/posts/popular") if route is None else route
    url = "https://" + host + route + (("?" + query) if query is not None else "")
    pages = [[42], []] if pages is None else pages
    responses = []
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
            index = response_index
            if index >= len(pages):
                raise RuntimeError("UnexpectedRequestAfterScript")
            source_page = pages[index]
            if isinstance(source_page, dict):
                body = source_page.get("$raw_response", source_page)
            else:
                rows = [dict(input_post(api_family), id=pid, scale="API_SCALE", date="API_DATE") for pid in source_page]
                body = rows if api_family == "danbooru" else {"posts": rows}
            body = copy.deepcopy(body)
            responses.append({"kind": "json", "body": body, "status": 200})
            response_index += 1
            return SimpleNamespace(text=json.dumps(body, ensure_ascii=False))
        # Only byte transport is replaced; actual common.request_json parses
        # the controlled response text for the inherited real _pagination.
        obj.request = transport
        real_metadata = obj.metadata
        source_metadata = None
        source_params_initial = None
        def metadata():
            nonlocal source_metadata, source_params_initial
            result = real_metadata()
            source_metadata = copy.deepcopy(result)
            source_params_initial = copy.deepcopy(obj.params)
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
                  "captured_query": obj.groups[-1], "source_metadata": source_metadata,
                  "source_params_initial": source_params_initial,
                  "per_page": obj.per_page, "threshold": obj.threshold,
                  "directory_format": list(obj.directory_fmt), "filename_format": obj.filename_fmt,
                  "archive_format": obj.archive_fmt, "options": options or {},
                  "hook_options": hook_options or [], "input_pages": pages,
                  "responses": responses, "download_body": "source-default-path-fixture",
                  "consume_limit": consume_limit, "stopped_by_consumer": stopped_by_consumer,
                  "calls": calls, "extraction_events": extraction, "trace": trace,
                  "published": published, "error": error,
                  "request_count": response_index, "unconsumed_page_count": len(pages) - response_index}
    config.clear()
    return result





def run(family, name, **kwargs):
    case = run_case(family, name, **kwargs)
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


def build():
    def modify(event, fields):
        return {"name": "metadata", "mode": "modify", "event": event, "fields": fields}
    queries = [
        ("defaults", None), ("empty_query", ""), ("bare_pairs", "date&scale"),
        ("blank_values", "date=&scale="), ("first_blank_wins", "date=&date=1999-01-01&scale=&scale=week"),
        ("decoded_keys", "%64ate=2026-10-21&date=1999-01-01&%73cale=week&scale=day"),
        ("week_calendar", "scale=week&date=2026-10-21"),
        ("week_basic", "scale=week&date=20261021"),
        ("week_calendar_week", "scale=week&date=2026-W43-3"),
        ("week_compact_week", "scale=week&date=2026W433"),
        ("week_without_weekday", "scale=week&date=2026-W43"),
        ("week_compact_without_weekday", "scale=week&date=2026W43"),
        ("month", "scale=month&date=2026-10-21"),
        ("month_unicode_character_slice", urlencode({"scale": "month", "date": "猫犬鳥魚"})),
        ("month_short_date", "scale=month&date=ab"),
        ("uppercase_unknown_scale", "scale=Week&date=20261021"),
        ("unicode_scale", urlencode({"scale": "猫犬", "date": "raw"})),
        ("invalid_week", "scale=week&date=not-a-date"),
        ("original_params_reserved", "date=2026-10-21&scale=week&page=77&limit=1&other=A%2BB"),
    ]
    cases = []
    for family in ("danbooru", "e621", "e926"):
        for name, query in queries:
            cases.append(run(family, name, query=query))
        cases.append(run(family, "numeric_pages", pages=[[42], [41], []], options={"threshold": 1}))
        cases.append(run(family, "short_page_stops", pages=[[42], [41]], options={"threshold": 2}))
        cases.append(run(family, "cancel_zero", pages=[[42], [41], []], options={"threshold": 1}, consume_limit=0))
        cases.append(run(family, "cancel_first", pages=[[42], [41], []], options={"threshold": 1}, consume_limit=1))
        cases.append(run(family, "empty_first_page", pages=[[], [42]]))
        cases.append(run(family, "late_invalid_page", pages=[[42], {"posts": None}], options={"threshold": 1}))
        cases.append(run(family, "duplicate_rows_default_skip", pages=[[42, 42, 41]]))
        for event in ("post", "prepare", "file", "after", "post-after"):
            cases.append(run(family, "scalar_" + event, pages=[[42, 41]],
                             hook_options=[modify(event, {"date": event.upper(), "scale": event.upper()})]))
        cases.append(run(family, "scalar_skip", pages=[[42, 42, 41]],
                         hook_options=[modify("skip", {"date": "SKIP", "scale": "SKIP"})]))
        cases.append(run(family, "delete_scalar_file", pages=[[42, 41]], hook_options=[
            {"name": "metadata", "mode": "delete", "event": "file", "fields": ["date", "scale"]}]))
        cases.append(run(family, "user_num_post", pages=[[42, 41]], hook_options=[modify("post", {"num": "{id!i:A+57/}"})]))
        cases.append(run(family, "id_delete_numeric_cursor", pages=[[42], [41], []], options={"threshold": 1}, hook_options=[
            {"name": "metadata", "mode": "delete", "event": "file", "fields": ["id"]}]))
        cases.append(run(family, "path_overrides", options={"directory": ["custom", "{scale}", "{date}"], "filename": "chosen_{id}.{extension}"}))
        observer = {"name": "metadata", "mode": "jsonl", "event": "file", "filename": "files.jsonl",
                    "base-directory": True, "directory": [], "archive": [":basedirectory", "metadata.sqlite"],
                    "include": ["category", "id", "date", "scale", "filename", "extension"]}
        cases.append(run(family, "actual_metadata_archive", hook_options=[observer]))
        cases.append(run(family, "blank_scale_actual_archive", query="scale=", hook_options=[observer]))
        cases.append(run(family, "mutated_actual_archive", hook_options=[modify("file", {"date": "HOOK", "scale": "EDIT"}), observer]))
        cases.append(run(family, "string_date_formatting", hook_options=[
            {"name": "metadata", "mode": "custom", "event": "file", "filename": "string.txt", "format": "{date!u}|{scale!u}"},
            {"name": "metadata", "mode": "custom", "event": "file", "filename": "match.txt", "format": "matched", "filter": "date == '2026-10-21'"},
        ]))
    cases.append(run("danbooru", "explore_route_alias", route="/explore/posts/popular"))
    for family in ("danbooru", "e621", "e926"):
        api_family = "e621" if family == "e926" else family
        row = dict(input_post(api_family), id=42, scale="API_SCALE", date="API_DATE")
        cases.append(run(family, "wrapped_posts_valid", pages=[{"posts": [row]}]))
        cases.append(run(family, "missing_posts_key", pages=[{}]))
        cases.append(run(family, "legacy_post_wrapper_error", pages=[{"post": row}]))
        cases.append(run(family, "empty_posts_object", pages=[{"posts": {}}]))
        cases.append(run(family, "empty_posts_string", pages=[{"posts": ""}]))
        body = [row, None] if family == "danbooru" else {"posts": [row, None]}
        cases.append(run(family, "valid_then_nonobject_row", pages=[{"$raw_response": body}]))
    routes = []
    for family, host in (("danbooru", "danbooru.donmai.us"), ("e621", "e621.net"), ("e926", "e926.net")):
        for route in ("/popular", "/explore/posts/popular", "/explore/posts/popularjunk?scale=week&date=2026-10-21", "/explore/posts/popular/ignored?scale=month"):
            url = "https://" + host + route
            obj = extractor.find(url)
            routes.append({"url": url, "extractor": type(obj).__name__ if obj else None,
                           "category": obj.category if obj else None,
                           "captured_query": obj.groups[-1] if obj else None,
                           "archive_format": obj.archive_fmt if obj else None})
    format_cases = []
    data = {"date": "2026-10-21", "scale": "day", "id": 42}
    for template in ("{date}", "{date!u}", "{date:%Y}", "{date!D:%Y}", "{scale[0]}"):
        error = None
        try:
            value = formatter.parse(template).format_map(data)
        except Exception as exc:
            value = None
            error = {"class": type(exc).__name__, "message": str(exc)}
        format_cases.append({"template": template, "context": data, "value": value, "error": error})
    assert all(case["error"] is None or case["name"].endswith(("invalid_week", "late_invalid_page", "missing_posts_key", "legacy_post_wrapper_error", "valid_then_nonobject_row")) for case in cases)
    assert all(case["error"] is not None for case in cases if case["name"].endswith(("invalid_week", "late_invalid_page", "missing_posts_key", "legacy_post_wrapper_error", "valid_then_nonobject_row")))
    root = Path(inspect.getfile(gallery_dl)).parent
    modules = (common, booru, danbooru, e621, config, formatter, job, path, dt, text, metadata_pp, pp_common)
    return {"version": gallery_dl.version.__version__, "generator": __doc__, "frozen_today": TODAY,
            "source_sha256": {str(Path(inspect.getfile(module)).relative_to(root)):
                              hashlib.sha256(Path(inspect.getfile(module)).read_bytes()).hexdigest()
                              for module in modules},
            "input_schema_sha256": hashlib.sha256(Path(__file__).with_name("booru_search_generate.py").read_bytes()).hexdigest(),
            "source_skip_default": True, "excluded_internal_artifacts": ["metadata.sqlite"],
            "format_cases": format_cases, "route_cases": routes, "cases": cases}


def main():
    original_date = dt.date
    try:
        dt.date = FrozenDate
        output = build()
    finally:
        dt.date = original_date
        config.clear()
    Path(__file__).with_name("booru_popular.json").write_text(json.dumps(output, ensure_ascii=False, indent=2) + "\n")


if __name__ == "__main__":
    main()
