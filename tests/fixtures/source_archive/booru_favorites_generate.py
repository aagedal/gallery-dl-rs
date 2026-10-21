#!/usr/bin/env python3
"""Offline E621FavoriteExtractor query/traversal/path/ownership source oracle.

Real source metadata(), posts(), items(), dispatch(), initialize(), URL and
Directory handling, MetadataPP, PathFormat and publication run. Real inherited _pagination runs against controlled request_json responses. Download bytes are written inside a temporary
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

HOSTS = {"e621": "e621.net", "e926": "e926.net"}




def run_case(family, name, query="", ids=None, options=None, hook_options=None, consume_limit=None, bad_final=False, path_suffix=""):
    host = HOSTS[family]
    url = f"https://{host}/favorites{path_suffix}" + (("?" + query) if query is not None else "")
    config.clear()
    with TemporaryDirectory(prefix="booru-defaults-") as temp:
        base = Path(temp)
        obj = extractor.find(url)
        category = obj.category
        opts = {"base-directory": temp, "skip": False, "async": False,
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
        pages = []
        for page in ([[42], []] if ids is None else ids):
            rows = []
            for pid in page:
                row = copy.deepcopy(input_post("e621"))
                row.update(id=pid, user_id="API_OWNER")
                rows.append(row)
            pages.append(rows)
        responses = [{"posts": copy.deepcopy(rows)} for rows in pages]
        if bad_final:
            responses.append({"posts": None})
        calls = []
        response_index = 0
        def request_json(request_url, params=None):
            nonlocal response_index
            calls.append({"url": request_url, "params": copy.deepcopy(params),
                          "wire_params": {key: value for key, value in params.items() if value is not None}})
            if response_index >= len(responses):
                raise RuntimeError("UnexpectedRequestAfterScript")
            result = copy.deepcopy(responses[response_index])
            response_index += 1
            return result
        obj.request_json = request_json
        real_metadata = obj.metadata
        parsed_query_initial = source_metadata = None
        def metadata():
            nonlocal parsed_query_initial, source_metadata
            result = real_metadata()
            parsed_query_initial = copy.deepcopy(obj.query)
            source_metadata = copy.deepcopy(result)
            return result
        obj.metadata = metadata
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
                          "temppath": relative(pathfmt.temppath), "extension": pathfmt.extension,
                          "archive_key": formatter.parse(obj.archive_fmt).format_map(pathfmt.kwdict)})
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
        result = {"name": family + "_" + name, "family": "e621", "category": category, "url": url,
                  "extractor": type(obj).__name__, "subcategory": obj.subcategory,
                  "captured_query": obj.groups[-1], "parsed_query_initial": parsed_query_initial,
                  "source_metadata": source_metadata, "per_page": obj.per_page, "threshold": obj.threshold,
                  "directory_format": list(obj.directory_fmt), "filename_format": obj.filename_fmt,
                  "archive_format": obj.archive_fmt, "options": options or {},
                  "hook_options": hook_options or [], "input_pages": pages, "responses": responses,
                  "download_body": "source-default-path-fixture",
                  "response_status": 200,
                  "consume_limit": consume_limit, "stopped_by_consumer": stopped_by_consumer,
                  "calls": calls, "extraction_events": extraction, "trace": trace,
                  "published": published, "error": error,
                  "unconsumed_response_count": len(responses) - response_index}
    config.clear()
    return result



def main():
    def modify(event, fields):
        return {"name": "metadata", "mode": "modify", "event": event, "fields": fields}
    queries = [
        ("missing", None), ("empty_query", ""), ("owner", "user_id=007"),
        ("duplicates", "user_id=First&user_id=Last"),
        ("bare_then_empty", "user_id&user_id=&user_id=9"),
        ("decoded_key_firstwins", "%75ser_id=A%2BB+Cat&user_id=9"),
        ("invalid_utf8", "user_id=%FF"),
        ("invalid_utf8_key_collision", "%FF=First&%FE=Last&user_id=7"),
        ("zero_owner", "user_id=0"), ("percent_once", "user_id=%252B"),
        ("signed_percent", "user_id=%+1"),
        ("reserved_overrides", "user_id=7&page=91&limit=1&tags=cat+dog"),
        ("encoded_reserved", "%70age=91&%6cimit=1&user_id=7"),
        ("arbitrary_params", "user_id=Cat+Dog&foo=A%2BB&flag=&bare&foo=Last"),
    ]
    cases = []
    for family in HOSTS:
        for name, query in queries:
            cases.append(run_case(family, name, query))
        cases.append(run_case(family, "configured_multi_page", "user_id=007", [[42], [41], []], {"threshold": 1}))
        cases.append(run_case(family, "threshold_above_then_boundary", "user_id=7", [[44, 43, 42], [41, 40], []], {"threshold": 2}))
        cases.append(run_case(family, "consumer_zero", "user_id=7", [[42], [41], []], {"threshold": 1}, consume_limit=0))
        cases.append(run_case(family, "consumer_first", "user_id=7", [[42], [41], []], {"threshold": 1}, consume_limit=1))
        cases.append(run_case(family, "invalid_later_page", "user_id=7", [[42]], {"threshold": 1}, bad_final=True))
        for event in ("post", "file", "post-after"):
            cases.append(run_case(family, "owner_" + event, "user_id=007", [[42], [41], []], {"threshold": 1},
                                  [modify(event, {"user_id": event.upper()})]))
        cases.append(run_case(family, "owner_all_hooks", "user_id=007", [[42], [41], []], {"threshold": 1},
                              [modify(event, {"user_id": event.upper()}) for event in ("post", "file", "post-after")]))
        cases.append(run_case(family, "owner_delete_file", "user_id=007", [[42], [41], []], {"threshold": 1},
                              [{"name": "metadata", "mode": "delete", "event": "file", "fields": ["user_id"]}]))
        cases.append(run_case(family, "id_mutation_no_cursor", "user_id=007", [[42], [41], []], {"threshold": 1},
                              [modify("file", {"id": "HOOK"})]))
        cases.append(run_case(family, "id_delete_no_cursor", "user_id=007", [[42], [41], []], {"threshold": 1},
                              [{"name": "metadata", "mode": "delete", "event": "file", "fields": ["id"]}]))
        for suffix in ("junk", "/ignored"):
            cases.append(run_case(family, "prefix_" + suffix.replace("/", "_"), "user_id=7", path_suffix=suffix))
    routes = []
    for host in HOSTS.values():
        for suffix in ("", "?user_id=7", "junk?user_id=7", "/ignored?user_id=7", "?user_id=7#ignored", "?user_id&user_id=8"):
            url = f"https://{host}/favorites{suffix}"
            obj = extractor.find(url)
            routes.append({"url": url, "extractor": type(obj).__name__ if obj else None,
                           "category": obj.category if obj else None,
                           "captured_query": obj.groups[-1] if obj else None,
                           "archive_format": obj.archive_fmt if obj else None})
    assert all(case["error"] is None or case["name"].endswith("invalid_later_page") for case in cases)
    assert all(case["error"] is not None for case in cases if case["name"].endswith("invalid_later_page"))
    root = Path(inspect.getfile(gallery_dl)).parent
    modules = (common, danbooru, e621, config, formatter, job, path, metadata_pp, pp_common)
    output = {"version": gallery_dl.version.__version__, "generator": __doc__,
              "source_sha256": {str(Path(inspect.getfile(module)).relative_to(root)):
                                hashlib.sha256(Path(inspect.getfile(module)).read_bytes()).hexdigest()
                                for module in modules},
              "input_schema_sha256": hashlib.sha256(Path(__file__).with_name("booru_search_generate.py").read_bytes()).hexdigest(),
              "route_cases": routes, "cases": cases}
    Path(__file__).with_name("booru_favorites.json").write_text(json.dumps(output, ensure_ascii=False, indent=2) + "\n")


if __name__ == "__main__":
    main()
