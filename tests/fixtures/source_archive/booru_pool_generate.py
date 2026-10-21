#!/usr/bin/env python3
"""Offline real gallery-dl pool extraction/keyword ownership oracle.

Run /private/tmp/gdlvenv/bin/python -B this_file.py. Only authentication
lookup, transport and filesystem/download operations are stubbed. Actual
items, metadata, posts, pagination, formatter, MetadataPP and DownloadJob
Directory/finalize hook dispatch run on the original source dictionaries.
"""
import copy
import hashlib
import inspect
import json
import logging
from pathlib import Path
from types import SimpleNamespace

import gallery_dl
from gallery_dl import config, extractor, formatter, job
from gallery_dl.extractor import common, danbooru, e621
from gallery_dl.postprocessor import metadata as metadata_pp, common as pp_common
from booru_search_generate import input_post, normalize

HOSTS = {"danbooru": "danbooru.donmai.us", "e621": "e621.net"}
BASE_POOL = {"id": 99, "name": "my_pool_name", "description": "raw_<b>description</b>",
             "created_at": "2024-01-02T03:04:05+00:00", "unknown": {"source": True},
             "post_ids": [42, 41, 40]}


def run_case(family, name, pages, pool=None, options=None, hooks=None, post_extra=None, consume_limit=None):
    options = {"threshold": 1} if options is None else options
    pool = copy.deepcopy(BASE_POOL if pool is None else pool)
    config.clear()
    try:
        for key, value in options.items():
            config.set(("extractor", family), key, value)
        obj = extractor.find(f"https://{HOSTS[family]}/pools/7")
        obj._get_auth_info = lambda: (None, None)
        obj._init()
        obj.kwdict = {}
        obj.login = lambda: None
        obj.cookies_store = lambda: None
        calls, events, hook_trace, pool_refs, post_refs = [], [], [], [], []
        next_page = 0
        aliases = []

        def alias(value):
            # Stable identity labels rather than nonreproducible CPython IDs.
            for index, previous in enumerate(aliases):
                if previous is value:
                    return index
            aliases.append(value)
            return len(aliases) - 1

        def request_json(url, params=None):
            nonlocal next_page
            calls.append({"url": url, "params": copy.deepcopy(params),
                          "wire_params": None if params is None else
                          {key: value for key, value in params.items() if value is not None}})
            if "/pools/" in url:
                return copy.deepcopy(pool)
            if next_page >= len(pages):
                raise RuntimeError("UnexpectedRequestAfterScript")
            rows = [dict(copy.deepcopy(input_post(family)), **copy.deepcopy(post_extra or {}), id=pid) for pid in pages[next_page]]
            next_page += 1
            return {"posts": rows} if family == "e621" else rows

        class Pathfmt:
            def __init__(self, data):
                self.kwdict = data
            def set_directory(self, data):
                self.kwdict = data

        state = SimpleNamespace(extractor=obj, metadata_http=None, kwdict={},
                                kwdict_eval=False, get_logger=logging.getLogger,
                                hooks={}, pathfmt=None, archive=None, status=0)
        state.initialize = lambda data: setattr(state, "pathfmt", Pathfmt(data))

        def snapshot(data):
            return {"pool_present": "pool" in data, "pool": copy.deepcopy(data.get("pool")),
                    "id": copy.deepcopy(data.get("id")), "num": data.get("num")}

        def register(callbacks, option):
            for event, callback in callbacks.items():
                def traced(path, event=event, callback=callback):
                    before = snapshot(path.kwdict)
                    callback(path)
                    hook_trace.append({"event": event, "before": before,
                                       "after": snapshot(path.kwdict), "requests": len(calls)})
                state.hooks.setdefault(event, []).append(traced)
        state.register_hooks = register
        processors = [metadata_pp.MetadataPP(state, option) for option in hooks or ()]
        obj.request_json = request_json
        real_metadata = obj.metadata
        initial_metadata = None
        metadata_reference = None
        def record_metadata():
            nonlocal initial_metadata, metadata_reference
            data = real_metadata()
            initial_metadata = copy.deepcopy(data)
            metadata_reference = data
            return data
        obj.metadata = record_metadata
        error = None
        iterator = iter(obj.items())
        media_count = 0
        stopped_by_consumer = False
        try:
            for message in iterator:
                kind, url, data = message
                job.Job.update_kwdict(state, data)
                if kind == common.Message.Directory:
                    job.DownloadJob.handle_directory(state, data)
                    kind_name = "post"
                elif kind == common.Message.Url:
                    kind_name = "media"
                    state.pathfmt.kwdict = data
                    for callback in state.hooks.get("file", ()):
                        callback(state.pathfmt)
                else:
                    raise AssertionError("UnexpectedSourceMessage")
                post_refs.append(data)
                pool_refs.append(data.get("pool"))
                events.append({"kind": kind_name, "url": url, "metadata": normalize(copy.deepcopy(data)),
                               "pool_alias": alias(data.get("pool")), "post_alias": alias(data),
                               "default_directory": [formatter.parse(part).format_map(data) for part in obj.directory_fmt],
                               "default_filename": formatter.parse(obj.filename_fmt).format_map(data),
                               "archive": formatter.parse(obj.archive_fmt).format_map(data)})
                if kind_name == "media":
                    media_count += 1
                    if consume_limit is not None and media_count >= consume_limit:
                        stopped_by_consumer = True
                        break
        except Exception as exc:
            error = type(exc).__name__ + ": " + str(exc)
            state.status = 1
        finally:
            iterator.close()
            job.DownloadJob.handle_finalize(state)
        # These final views expose identity sharing which copied event snapshots
        # cannot: nested mutations affect earlier references; root replacement
        # detaches only that post. Repeated E621 members share the post itself.
        final_references = [{"pool": normalize(copy.deepcopy(pool_value)),
                             "post": snapshot(post_value)}
                            for pool_value, post_value in zip(pool_refs, post_refs)]
        return {"name": family + "_" + name, "family": family, "url": obj.url,
                "options": options, "hook_options": hooks or [], "pool_response": pool,
                "post_extra": post_extra or {}, "consume_limit": consume_limit,
                "stopped_by_consumer": stopped_by_consumer,
                "initial_metadata": initial_metadata, "final_metadata": metadata_reference,
                "input_pages": pages, "per_page": obj.per_page, "threshold": obj.threshold,
                "archive_format": obj.archive_fmt, "filename_format": obj.filename_fmt,
                "calls": calls, "events": events, "hook_trace": hook_trace,
                "final_references": final_references, "error": error,
                "unconsumed_page_count": len(pages) - next_page}
    finally:
        config.clear()


def family_cases(family):
    cases = [
        run_case(family, "default_threshold_short", [[41, 42]], options={}),
        run_case(family, "multi_page_response_id_differs", [[41], [42], []]),
        run_case(family, "unavailable_member_position_gap", [[42, 40], []]),
        run_case(family, "duplicate_members", [[42, 41], []], dict(BASE_POOL, post_ids=[42, 41, 42])),
        run_case(family, "duplicate_api_rows", [[42, 41, 42], []]),
        run_case(family, "empty_members", [[], []], dict(BASE_POOL, post_ids=[])),
        run_case(family, "missing_post_ids_empty_feed", [[]], {k: v for k, v in BASE_POOL.items() if k != "post_ids"}),
        run_case(family, "missing_post_ids_nonempty_feed", [[42], []], {k: v for k, v in BASE_POOL.items() if k != "post_ids"}),
    ]
    cases.append(run_case(family, "consumer_first_media", [[42], [41], []], consume_limit=1))
    if family == "danbooru":
        for order in ("desc", "asc_id"):
            # Reversing produces a valid42 BEFORE unknown999, yet num setup is
            # performed for the entire list before any Directory can be yielded.
            cases.append(run_case(family, "reverse_unknown_member_" + order, [[999, 42], []],
                                  dict(BASE_POOL, post_ids=[42, 41]),
                                  options={"threshold": 1, "order-posts": order}))
    for order in (None, "asc", "pool", "id", "desc_id", "desc", "desc_pool", "asc_id", "id_asc", "invalid"):
        cases.append(run_case(family, "order_" + str(order), [[41], [42], []],
                              options={"threshold": 1, "order-posts": order}))
    def modify(event, fields):
        return {"name": "metadata", "mode": "modify", "event": event, "fields": fields}
    cases += [
        run_case(family, "nested_pool_file_mutation", [[42, 41], []], hooks=[modify("file", {"pool[id]": "123", "pool[name]": "HOOK_under"})]),
        run_case(family, "root_pool_replacement", [[42, 41], []], hooks=[modify("file", {"pool": "REPLACED"})]),
        run_case(family, "root_pool_deletion", [[42, 41], []], hooks=[{"name": "metadata", "mode": "delete", "event": "file", "fields": ["pool"]}]),
        run_case(family, "nested_pool_post_after", [[42], [41], []], hooks=[modify("post-after", {"pool[id]": "123", "pool[name]": "AFTER_under"})]),
    ]
    return cases


def main():
    cases = [case for family in HOSTS for case in family_cases(family)]
    root = Path(inspect.getfile(gallery_dl)).parent
    modules = (common, danbooru, e621, config, formatter, job, metadata_pp, pp_common)
    output = {"version": gallery_dl.version.__version__,
              "source_sha256": {str(Path(inspect.getfile(module)).relative_to(root)):
                                hashlib.sha256(Path(inspect.getfile(module)).read_bytes()).hexdigest()
                                for module in modules},
              "input_schema_sha256": hashlib.sha256(Path(__file__).with_name("booru_search_generate.py").read_bytes()).hexdigest(),
              "generator": __doc__, "cases": cases}
    Path(__file__).with_name("booru_pool.json").write_text(json.dumps(output, ensure_ascii=False, indent=2) + "\n")


if __name__ == "__main__":
    main()
