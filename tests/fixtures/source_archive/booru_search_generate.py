#!/usr/bin/env python3
"""Offline source oracle for search constructor parsing and emitted booru keys.

Run with the authoritative gallery-dl environment. Real extractor.find,
metadata(), posts(), items(), Job.update_kwdict and formatter methods run.
Only pagination transport is replaced by one synthetic post. No session,
authentication, requests, or filesystem output hooks are initialized.
"""
import copy
import datetime
import hashlib
import inspect
import json
import logging
from pathlib import Path
from types import SimpleNamespace

import gallery_dl
from gallery_dl import extractor, formatter
from gallery_dl.job import Job

FAMILIES = {
    "danbooru": "https://danbooru.donmai.us/posts?",
    "e621": "https://e621.net/posts?",
    "gelbooru": "https://gelbooru.com/index.php?page=post&s=list&",
    "safebooru": "https://safebooru.org/index.php?page=post&s=list&",
}
QUERIES = [
    ("plain", "tags=Cat"),
    ("plus_spaces", "tags=Cat++Dog"),
    ("encoded_plus", "tags=Cat%2BDog"),
    ("mixed_plus", "tags=Cat++Dog%2Bfish"),
    ("percent_once", "tags=cat%252Bdog"),
    ("unicode", "tags=%E7%8C%AB+%CE%A3"),
    ("invalid_utf8", "tags=%FF"),
    ("invalid_percent", "tags=%+1%+A%01"),
    ("empty", "tags="),
    ("missing", "other=cat"),
    ("bare", "tags"),
    ("duplicates", "tags=first&tags=last"),
    ("duplicates_with_other", "tags=first&foo=x&tags=last"),
    ("last_empty", "tags=first&tags="),
    ("first_empty", "tags=&tags=last"),
    ("whitespace", "tags=++%20+"),
    ("surrounding_whitespace", "tags=++Cat++"),
    ("all_sentinel", "tags=all"),
    ("all_case", "tags=ALL"),
    ("literal_hash", "tags=cat%23dog"),
    ("fragment", "tags=cat#tags=ignored"),
    ("bare_then_value", "tags&tags=cat"),
    ("encoded_name", "%74ags=cat"),
    ("uppercase_name", "TAGS=cat"),
    ("embedded_name", "notags=cat"),
    ("order_id_ascending", "tags=cat+order%3Aid_asc"),
    ("id_filter", "tags=cat+id%3A42"),
]
MD5 = "0123456789abcdef0123456789abcdef"


def input_post(family):
    url = f"https://cdn.example.invalid/{MD5}.jpg"
    common = {"id": 42, "md5": MD5, "file_url": url,
              "created_at": "2024-01-02T03:04:05+00:00", "tags": "cat dog"}
    if family == "danbooru":
        common.update({"file_url": f"https://cdn.donmai.us/original/01/23/{MD5}.jpg",
                       "file_ext": "jpg", "image_width": 640, "image_height": 480,
                       "file_size": 123, "rating": "g"})
        for suffix in ("", "_artist", "_character", "_copyright", "_general", "_meta"):
            common["tag_string" + suffix] = "cat dog" if not suffix else ""
    elif family == "e621":
        common = {"id": 42, "created_at": "2024-01-02T03:04:05+00:00",
                  "file": {"url": f"https://static1.e621.net/data/01/23/{MD5}.jpg",
                           "md5": MD5, "ext": "jpg", "width": 640,
                           "height": 480, "size": 123},
                  "tags": {"general": ["cat", "dog"]}, "pools": [], "rating": "s"}
    else:
        host = "img3.gelbooru.com" if family == "gelbooru" else "safebooru.org"
        common.update({"file_url": f"https://{host}/images/1/{MD5}.jpg",
                       "created_at": "Tue Jan 02 03:04:05 +0000 2024",
                       "width": 640, "height": 480, "rating": "s"})
        if family == "safebooru":
            common.update({"directory": "1", "image": MD5 + ".jpg", "hash": MD5})
    return common


def normalize(value):
    if isinstance(value, datetime.datetime):
        return value.isoformat()
    if isinstance(value, dict):
        return {key: normalize(item) for key, item in value.items()}
    if isinstance(value, (tuple, list)):
        return [normalize(item) for item in value]
    return value


def run_case(family, name, url):
    obj = extractor.find(url)
    case = {"name": f"{family}_{name}", "family": family, "url": url}
    if obj is None:
        case.update(extractor=None, error="NoExtractor", events=[], calls=[])
        return case
    case.update(extractor=type(obj).__name__, category=obj.category,
                subcategory=obj.subcategory, captured_tags=obj.groups[-1],
                archive_format=obj.archive_fmt, filename_format=obj.filename_fmt)
    # Keep all source option gates disabled without creating login/session state.
    obj.kwdict = {}
    obj.ugoira = False
    obj.external = False
    obj.includes = False
    obj.login = lambda: None
    calls = []
    post = input_post(family)
    case["input_post"] = copy.deepcopy(post)

    if family in ("danbooru", "e621"):
        def pagination(endpoint, params, prefix=None):
            calls.append({"method": "_pagination", "endpoint": endpoint,
                          "params": copy.deepcopy(params), "prefix": prefix})
            return iter([copy.deepcopy(post)])
    else:
        def pagination(params):
            calls.append({"method": "_pagination", "params": copy.deepcopy(params)})
            return iter([copy.deepcopy(post)])
    obj._pagination = pagination
    events = []
    job = SimpleNamespace(extractor=obj, metadata_http=None, kwdict={}, kwdict_eval=False)
    try:
        for message in obj.items():
            kind = "post" if message[0] == 2 else "media" if message[0] == 3 else "other"
            data = copy.deepcopy(message[2])
            # Gelbooru and Safebooru attach search_tags to extractor.kwdict,
            # unlike Danbooru/e621 which merge metadata() into each post.
            # Use the real Job merge before formatting, exactly as dispatch does.
            Job.update_kwdict(job, data)
            event = {"kind": kind, "url": message[1], "metadata": normalize(data),
                     "archive_key": formatter.parse(obj.archive_fmt).format_map(data),
                     "default_filename": formatter.parse(obj.filename_fmt).format_map(data)}
            events.append(event)
        error = None
    except Exception as exc:
        error = type(exc).__name__
    case.update(calls=calls, events=events, error=error,
                final_extractor_keywords=normalize(obj.kwdict))
    return case


def ownership_case(family, root):
    from gallery_dl.postprocessor.metadata import MetadataPP
    obj = extractor.find(root + "tags=Original")
    obj.kwdict = {}
    obj.ugoira = False
    obj.external = False
    obj.includes = False
    obj.login = lambda: None
    post = input_post(family)
    obj._pagination = lambda *args, **kwargs: iter([copy.deepcopy(post)])
    job = SimpleNamespace(extractor=obj, metadata_http=None, kwdict={}, kwdict_eval=False,
                          get_logger=logging.getLogger,
                          register_hooks=lambda *args, **kwargs: None)
    modify = MetadataPP(job, {"mode": "modify", "event": "post",
                              "fields": {"search_tags": "HOOK"}})
    messages = iter(obj.items())
    directory = next(messages)
    assert directory[0] == 2
    data = directory[2]  # Keep the real reference shared with the suspended source.
    Job.update_kwdict(job, data)
    trace = [{"event": "post-before", "search_tags": data["search_tags"]}]
    modify.run(SimpleNamespace(kwdict=data))
    trace.append({"event": "post-after", "search_tags": data["search_tags"]})
    media = next(messages)
    assert media[0] == 3
    same_reference = media[2] is data
    Job.update_kwdict(job, media[2])
    trace.append({"event": "file", "search_tags": media[2]["search_tags"]})
    assert next(messages, None) is None
    return {"family": family, "url": obj.url, "input_post": post,
            "trace": trace, "shared_reference": same_reference,
            "file_archive_key": formatter.parse(obj.archive_fmt).format_map(media[2])}

def main():
    cases = [run_case(family, name, root + query)
             for family, root in FAMILIES.items() for name, query in QUERIES]
    # Native DAPI query parsing currently accepts reordered pairs, but the
    # installed source matchers require the literal page=post&s=list prefix.
    for family in ("gelbooru", "safebooru"):
        host = "gelbooru.com" if family == "gelbooru" else "safebooru.org"
        cases.append(run_case(family, "reordered_query",
                              f"https://{host}/index.php?s=list&page=post&tags=cat"))
    modules = [inspect.getmodule(type(extractor.find(root + "tags=cat")))
               for root in FAMILIES.values()]
    from gallery_dl.extractor import booru, common, danbooru, gelbooru_v02
    from gallery_dl import job, text
    modules += [booru, common, danbooru, gelbooru_v02, job, text, formatter]
    hashes = {Path(module.__file__).name:
              hashlib.sha256(Path(module.__file__).read_bytes()).hexdigest()
              for module in modules}
    from gallery_dl.postprocessor import metadata as pp_metadata, common as pp_common
    for module in (pp_metadata, pp_common):
        file = Path(module.__file__)
        hashes["postprocessor/" + file.name] = hashlib.sha256(file.read_bytes()).hexdigest()
    output = {
        "version": gallery_dl.version.__version__, "source_sha256": hashes,
        "generator": "Real extractor.find + tag metadata/posts/items + Job.update_kwdict + formatter. Pagination replaced with one synthetic API post, no HTTP/login/session. Captured pagination params are constructor arguments before pagination adds cursors/limits. No unsupported/error cases removed.",
        "cases": cases,
        "ownership_cases": [ownership_case(family, root) for family, root in FAMILIES.items()],
    }
    Path(__file__).with_name("booru_search.json").write_text(
        json.dumps(output, ensure_ascii=False, indent=2) + "\n")


if __name__ == "__main__":
    main()
