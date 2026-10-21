#!/usr/bin/env python3
"""Run actual booru tag traversal against controlled, offline API pages.

Page sizes are never overridden; configured thresholds use real config and
_init. request_json is the only transport stub; the _pagination wrapper records its arguments and delegates to the real
bound method. Traversal API records contain only IDs. Separate mutation cases
run real items(), MetadataPP and DownloadJob directory/finalize callbacks on
complete synthetic API posts; only file IO callbacks are dispatched directly.
"""
import copy
import hashlib
import inspect
import json
import logging
from types import SimpleNamespace
from pathlib import Path
from urllib.parse import quote_plus

import gallery_dl
from gallery_dl import extractor, text, config, job, formatter
from gallery_dl.postprocessor import metadata as metadata_pp, common as pp_common
from booru_search_generate import input_post
from gallery_dl.extractor import common, danbooru, e621

UNSET = object()

FAMILIES = {
    "danbooru": "https://danbooru.donmai.us/posts?tags=",
    "e621": "https://e621.net/posts?tags=",
}
PREFIX_TAGS = [
    ("plain", "cat"),
    ("order_id", "cat order:id"),
    ("order_id_asc", "cat order:id_asc"),
    ("order_id_desc", "cat order:id_desc"),
    ("other_order", "cat order:score"),
    ("id_filter", "cat id:42"),
    ("md5_filter", "cat md5:0123456789abcdef0123456789abcdef"),
    ("ordfav_filter", "cat ordfav:7"),
    ("ordfavgroup_filter", "cat ordfavgroup:7"),
    ("ordpool_filter", "cat ordpool:7"),
    ("last_order_disables", "cat order:id order:score"),
    ("last_order_desc", "cat order:score order:id_desc"),
    ("last_order_asc", "cat order:id_desc order:id_asc"),
    ("id_break_before_order", "cat id:42 order:id"),
    ("id_break_after_order", "cat order:id id:42 order:id_desc"),
    ("md5_break_before_order", "md5:abc order:id"),
    ("ordfavgroup_break_before_order", "ordfavgroup:7 order:id"),
    ("ordpool_break_after_order", "order:id ordpool:7 order:id_desc"),
    ("unrelated_substring", "cat disorder:id"),
    ("python_control_separator", "cat\u001corder:id"),
    ("zero_width_space_is_not_separator", "cat\u200border:id"),
]


def instance(family, tags="cat", threshold_override=UNSET):
    config.clear()
    try:
        if threshold_override is not UNSET:
            config.set(("extractor", family), "threshold", threshold_override)
        obj = extractor.find(FAMILIES[family] + quote_plus(tags))
        assert obj is not None
        # Real _init reads real config and computes defaults/clamping. No
        # authentication, session or HTTP is initialized by this isolated call.
        obj._get_auth_info = lambda: (None, None)
        obj._init()
        metadata = obj.metadata()
        return obj, metadata
    finally:
        config.clear()


def run_case(family, name, tags, pages, consume_limit=None, threshold_override=UNSET):
    obj, metadata = instance(family, tags, threshold_override)
    pages = copy.deepcopy(pages)
    calls = []
    pagination_arguments = []
    next_page = 0

    def request_json(url, params):
        nonlocal next_page
        calls.append({
            "url": url,
            "params": copy.deepcopy(params),
            # requests omits None-valued query parameters; no prepared request
            # or network stack is needed for these simple scalar query values.
            "wire_params": {key: value for key, value in params.items()
                            if value is not None},
        })
        if next_page == len(pages):
            raise RuntimeError("UnexpectedRequestAfterScript")
        result = [{"id": value} for value in pages[next_page]]
        next_page += 1
        return {"posts": result} if family == "e621" else result

    real_pagination = obj._pagination

    def record_pagination(endpoint, params, prefix=None):
        pagination_arguments.append({"endpoint": endpoint,
                                     "params": copy.deepcopy(params), "prefix": prefix})
        return real_pagination(endpoint, params, prefix)

    obj.request_json = request_json
    obj._pagination = record_pagination
    emitted = []
    error = None
    stopped_by_consumer = False
    iterator = None
    try:
        iterator = iter(obj.posts())
        if consume_limit is None:
            emitted.extend(post["id"] for post in iterator)
        else:
            for _ in range(consume_limit):
                try:
                    emitted.append(next(iterator)["id"])
                except StopIteration:
                    break
            else:
                # No N+1 poll: closing here must not execute pagination's next
                # request or its code following the final requested yield.
                stopped_by_consumer = True
    except Exception as exc:
        error = type(exc).__name__ + ": " + str(exc)
    finally:
        if iterator is not None:
            iterator.close()
    return {
        "name": f"{family}_{name}", "family": family, "url": obj.url,
        "extractor": type(obj).__name__, "tags": tags,
        "metadata": metadata,
        "per_page": obj.per_page, "threshold": obj.threshold,
        "page_start": obj.page_start,
        "threshold_set": threshold_override is not UNSET,
        "threshold_override": None if threshold_override is UNSET else threshold_override,
        "input_pages": pages, "consume_limit": consume_limit,
        "pagination_arguments": pagination_arguments,
        "calls": calls, "emitted_ids": emitted,
        "unconsumed_page_count": len(pages) - next_page,
        "stopped_by_consumer": stopped_by_consumer, "error": error,
    }


def family_cases(family):
    obj, _ = instance(family)
    size, threshold = obj.per_page, obj.threshold
    # Controlled API ordering is input, not a second implementation of the
    # source's prefix or cursor rules. Dedicated asc cases below model its
    # first-page ascending / later-page descending API contract explicitly.
    full_desc = list(range(10_000, 10_000 - size, -1))
    full_asc = list(range(1, size + 1))
    cases = [run_case(family, "prefix_" + name, tags, [full_desc, []])
             for name, tags in PREFIX_TAGS]
    cases += [
        run_case(family, "default_full_page", "cat", [full_desc, []]),
        run_case(family, "threshold_minus_one_stops", "cat",
                 [full_desc[:threshold - 1], [99]]),
        run_case(family, "threshold_boundary_continues", "cat",
                 [full_desc[:threshold], [99]]),
        run_case(family, "threshold_plus_one_continues", "cat",
                 [full_desc[:threshold + 1], []]),
        run_case(family, "empty_first_page", "cat", [[], [99]]),
        # Source b-cursor uses the final raw row, not the lowest ID in the page.
        run_case(family, "descending_last_not_min", "cat",
                 [full_desc[:size // 2] + [1] + full_desc[size // 2 + 1:], []]),
        run_case(family, "ascending_first_page_unchanged_later_reversed", "cat order:id_asc",
                 [full_asc, list(range(2 * size, size, -1)), [2 * size + 2, 2 * size + 1]]),
        # Subsequent rows are reversed before both yielding and selecting the
        # next cursor; with this deliberately irregular page max != final.
        run_case(family, "ascending_cursor_uses_last_after_reverse", "cat order:id",
                 [full_asc, [size + 5] + list(range(10_000, 10_000 - size + 1, -1)), []]),
        run_case(family, "numeric_pages_absent_then_2_then_3", "cat order:score",
                 [full_desc, [value + 20_000 for value in full_desc], [99]]),
        run_case(family, "id_filter_numeric_pages", "id:42 order:id",
                 [full_desc, []]),
        # The source has no cursor progress/cycle guard: repeated full replies
        # are yielded again and trigger another identical b-cursor request.
        run_case(family, "repeated_page_is_not_a_source_stop", "cat",
                 [full_desc, full_desc, []]),
        run_case(family, "consumer_zero_no_request", "cat",
                 [full_desc, [99]], consume_limit=0),
        run_case(family, "consumer_first_yield_no_future_page", "cat",
                 [full_desc, [99]], consume_limit=1),
        run_case(family, "consumer_at_threshold_does_not_request_next", "cat",
                 [full_desc[:threshold], [99]], consume_limit=threshold),
        run_case(family, "consumer_at_full_page_does_not_request_next", "cat",
                 [full_desc, [99]], consume_limit=size),
        run_case(family, "consumer_past_short_page_exhausts_without_next", "cat",
                 [full_desc[:threshold - 1], [99]], consume_limit=threshold),
    ]
    cases += [
        run_case(family, "configured_zero_clamps_to_one", "cat", [[42], [41], []], threshold_override=0),
        run_case(family, "configured_negative_clamps_to_one", "cat", [[42], [41], []], threshold_override=-7),
        run_case(family, "configured_one", "cat", [[42], [41], []], threshold_override=1),
        run_case(family, "configured_five_short", "cat", [[5, 4, 3, 2], [99]], threshold_override=5),
        run_case(family, "configured_five_boundary", "cat", [[6, 5, 4, 3, 2], [99]], threshold_override=5),
        run_case(family, "configured_five_above", "cat", [[7, 6, 5, 4, 3, 2], []], threshold_override=5),
        run_case(family, "configured_above_page_size", "cat", [full_desc, [99]], threshold_override=size + 1),
        run_case(family, "configured_null_uses_default", "cat", [[42], [99]], threshold_override=None),
        run_case(family, "configured_string_uses_default", "cat", [[42], [99]], threshold_override="1"),
        run_case(family, "configured_float_uses_default", "cat", [[42], [99]], threshold_override=1.0),
        run_case(family, "configured_true_uses_default", "cat", [[42], [99]], threshold_override=True),
        run_case(family, "configured_false_uses_default", "cat", [[42], [99]], threshold_override=False),
        # Real threshold=1 keeps 101 one-row API pages alive, then requests
        # the terminal empty page. This cannot fit a native 100-page ceiling.
        run_case(family, "configured_one_beyond_100_pages", "cat",
                 [[value] for value in range(1000, 899, -1)] + [[]], threshold_override=1),
    ]
    return cases


def run_mutation_case(family, name, options, pages=None):
    """Real items/pagination and MetadataPP callbacks on the SAME source dict.

    Real DownloadJob.handle_directory/handle_finalize preserve post-after timing:
    next page request is made before handle_directory receives its next yield.
    File callbacks use that same path dictionary; only download IO is omitted.
    """
    pages = pages or [[42], []]
    obj, _ = instance(family, threshold_override=1)
    obj.kwdict = {}
    obj.login = lambda: None
    obj.cookies_store = lambda: None
    calls, trace, emitted = [], [], []
    next_page = 0

    def request_json(url, params):
        nonlocal next_page
        calls.append({"url": url, "params": copy.deepcopy(params),
                      "wire_params": {k: v for k, v in params.items() if v is not None}})
        if next_page == len(pages):
            raise RuntimeError("UnexpectedRequestAfterScript")
        rows = [dict(copy.deepcopy(input_post(family)), id=id_) for id_ in pages[next_page]]
        next_page += 1
        return {"posts": rows} if family == "e621" else rows

    class Path:
        def __init__(self, data):
            self.kwdict = data
        def set_directory(self, data):
            self.kwdict = data

    state = SimpleNamespace(extractor=obj, metadata_http=None, kwdict={},
                            kwdict_eval=False, get_logger=logging.getLogger,
                            hooks={}, pathfmt=None, archive=None, status=0)
    state.initialize = lambda data: setattr(state, "pathfmt", Path(data))

    def id_state(data):
        return {"present": "id" in data, "value": copy.deepcopy(data.get("id"))}

    def register(hooks, options):
        for event, callback in hooks.items():
            def traced(path, event=event, callback=callback):
                before = id_state(path.kwdict)
                callback(path)
                trace.append({"event": event, "before": before,
                              "after": id_state(path.kwdict), "requests": len(calls)})
            state.hooks.setdefault(event, []).append(traced)
    state.register_hooks = register
    processors = [metadata_pp.MetadataPP(state, option) for option in options]
    obj.request_json = request_json
    error = None
    try:
        for message in obj.items():
            kind, _, data = message
            job.Job.update_kwdict(state, data)
            if kind == common.Message.Directory:
                job.DownloadJob.handle_directory(state, data)
            elif kind == common.Message.Url:
                state.pathfmt.kwdict = data
                for callback in state.hooks.get("file", ()):
                    callback(state.pathfmt)
                emitted.append(id_state(data))
    except Exception as exc:
        error = type(exc).__name__ + ": " + str(exc)
        state.status = 1
    finally:
        job.DownloadJob.handle_finalize(state)
    assert processors  # retain genuine instances through finalization
    return {"name": family + "_" + name, "family": family, "url": obj.url,
            "threshold": obj.threshold, "hook_options": options,
            "input_pages": pages, "calls": calls, "trace": trace,
            "files": emitted, "error": error,
            "unconsumed_page_count": len(pages) - next_page}


def mutation_cases(family):
    def modify(event, value):
        return {"name": "metadata", "mode": "modify", "event": event, "fields": {"id": value}}
    return [
        run_mutation_case(family, "post_id", [modify("post", "800")]),
        run_mutation_case(family, "file_id", [modify("file", "900")]),
        run_mutation_case(family, "file_arithmetic", [modify("file", "{id:A+100/}")]),
        run_mutation_case(family, "file_leading_zero_string", [modify("file", "00077")]),
        run_mutation_case(family, "file_delete_id", [{"name": "metadata", "mode": "delete", "event": "file", "fields": ["id"]}]),
        run_mutation_case(family, "post_after_does_not_change_cursor", [modify("post-after", "999")], [[42], [41], []]),
        run_mutation_case(family, "file_then_post_after", [modify("file", "900"), modify("post-after", "999")]),
        run_mutation_case(family, "post_then_file", [modify("post", "800"), modify("file", "900")]),
    ]


def main():
    defaults = {}
    cases = []
    for family in FAMILIES:
        obj, _ = instance(family)
        defaults[family] = {"per_page": obj.per_page, "threshold": obj.threshold,
                            "page_start": obj.page_start}
        cases.extend(family_cases(family))
    assert defaults == {"danbooru": {"per_page": 200, "threshold": 180, "page_start": None},
                        "e621": {"per_page": 320, "threshold": 300, "page_start": None}}
    assert all(case["error"] is None for case in cases)
    modules = [common, danbooru, e621, text, config, job, formatter, metadata_pp, pp_common]
    output = {
        "version": gallery_dl.version.__version__,
        "source_sha256": {str(Path(inspect.getfile(module)).relative_to(Path(inspect.getfile(gallery_dl)).parent)):
                           hashlib.sha256(Path(inspect.getfile(module)).read_bytes()).hexdigest()
                           for module in modules},
        "generator": "Real Tag metadata/posts and inherited Danbooru._pagination; real _init threshold defaults. Only request_json transport and authentication discovery stubbed. IDs-only API records, E621 wrapper vs Danbooru array retained. Page sizes never overridden; configured threshold cases use real config.set+clear and real _init, and threshold_set distinguishes explicit null from absence. Consumer limit is iterator cancellation, not extractor max-posts. Raw params retain None; wire_params omit None as requests does.",
        "defaults": defaults, "cases": cases,
        "mutation_cases": [case for family in FAMILIES for case in mutation_cases(family)],
        "input_schema_sha256": hashlib.sha256(Path(__file__).with_name("booru_search_generate.py").read_bytes()).hexdigest(),
        "mutation_method": "Real items, MetadataPP and Job.update_kwdict on the original shared dictionaries. Actual DownloadJob.handle_directory and handle_finalize dispatch post/post-after; file callbacks omit download IO only. Cursor computation remains inside actual _pagination after generator resumes. Input post schema reused from booru_search_generate.input_post.",
    }
    Path(__file__).with_name("booru_pagination.json").write_text(
        json.dumps(output, ensure_ascii=False, indent=2) + "\n")


if __name__ == "__main__":
    main()
