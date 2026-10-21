#!/usr/bin/env python3
"""Genuine offline booru filename/extension extraction and hook ownership.

Only pagination transport/authentication and download/filesystem IO are stubbed.
Real items(), text.nameext_from_url(), Job.update_kwdict(), MetadataPP and
source default formatters run on original post dictionaries. Raw extraction_events
precede job processing; events record Post hooks and actual PathFormat
set_filename before Prepare/File hooks, including default extension mapping. Run Python -B.
"""
import copy
import hashlib
import inspect
import json
import logging
from pathlib import Path
from types import SimpleNamespace

import gallery_dl
from gallery_dl import extractor, formatter, job, text, path
from gallery_dl.extractor import booru, common, danbooru, e621, gelbooru, gelbooru_v02
from gallery_dl.postprocessor import metadata as metadata_pp, common as pp_common
from booru_search_generate import FAMILIES, input_post, normalize

URL_CASES = [
    ("plain", "picture.jpg"), ("uppercase_jpeg", "Picture.JPEG"),
    ("dotfile", ".hidden"), ("only_dot", "."), ("trailing_dot", "picture."),
    ("multiple_dots", "picture.part.PNG"), ("no_extension", "picture"),
    ("extension_16", "picture.abcdefghijklmnop"),
    ("extension_17", "picture.abcdefghijklmnopq"),
    ("unicode_extension_16", "picture." + "Α" * 16),
    ("unicode_lower_expands", "picture." + "İ" * 16),
    ("encoded_dot", "picture%2EJPEG"), ("encoded_plus", "a%2Bb+c.jpg"),
    ("encoded_slash", "picture.j%2Fpg"), ("encoded_backslash", "picture.j%5Cpg"),
    ("invalid_utf8", "picture.%FF"), ("invalid_percent", "picture.%+1"),
    ("percent_once", "picture.%252E"),
    ("query", "picture.PNG?download=wrong.gif"),
    ("fragment", "picture.jpg#frag"),
    ("query_then_fragment", "picture.JPG?download=1#wrong.gif"),
    ("fragment_then_query", "picture.jpg#frag?download=1"),
    ("trailing_slash", "folder/"),
]


def source_case(family, name, tail, fields=None, hooks=None):
    obj = extractor.find(FAMILIES[family] + "tags=Original")
    obj.kwdict = {}
    obj.ugoira = False
    obj.external = False
    obj.includes = False
    obj.login = lambda: None
    post = input_post(family)
    host = {"danbooru": "cdn.donmai.us", "e621": "static1.e621.net",
            "gelbooru": "img3.gelbooru.com", "safebooru": "safebooru.org"}[family]
    raw_url = f"https://{host}/images/1/" + tail
    if family == "e621":
        post["file"]["url"] = raw_url
        post["file"].update(fields or {})
    else:
        post["file_url"] = raw_url
        post.update(fields or {})
    original = copy.deepcopy(post)
    calls = []
    def pagination(*args, **kwargs):
        calls.append({"args": copy.deepcopy(args), "kwargs": copy.deepcopy(kwargs)})
        return iter([post])
    obj._pagination = pagination
    state = SimpleNamespace(extractor=obj, metadata_http=None, kwdict={}, kwdict_eval=False,
                            get_logger=logging.getLogger, hooks={})
    def register(callbacks, options):
        for event, callback in callbacks.items():
            state.hooks.setdefault(event, []).append(callback)
    state.register_hooks = register
    processors = [metadata_pp.MetadataPP(state, option) for option in hooks or ()]
    events, extraction_events, references = [], [], []
    pathfmt = path.PathFormat(obj)
    error = None
    try:
        for kind, url, data in obj.items():
            extraction_events.append({"kind": "post" if kind == common.Message.Directory else "media",
                                      "url": url, "metadata": normalize(copy.deepcopy(data))})
            job.Job.update_kwdict(state, data)
            event = "post" if kind == common.Message.Directory else "file"
            if event == "post":
                pathfmt.set_directory(data)
            else:
                # Actual DownloadJob.handle_url checkpoint: default extension
                # map mutates this SAME source dictionary before Prepare/File.
                pathfmt.set_filename(data)
                for callback in state.hooks.get("prepare", ()):
                    callback(pathfmt)
            for callback in state.hooks.get(event, ()):
                callback(pathfmt)
            references.append(data)
            events.append({"kind": "post" if event == "post" else "media", "url": url,
                           "metadata": normalize(copy.deepcopy(data)),
                           "default_filename": formatter.parse(obj.filename_fmt).format_map(data),
                           "archive_key": formatter.parse(obj.archive_fmt).format_map(data)})
    except Exception as exc:
        error = type(exc).__name__ + ": " + str(exc)
    return {"name": family + "_" + name, "family": family, "url": obj.url,
            "input_post": original, "hook_options": hooks or [], "calls": calls,
            "filename_format": obj.filename_fmt, "archive_format": obj.archive_fmt,
            "calls_phase": "Source posts() arguments to stubbed _pagination, before cursor/limit mutation; not wire requests.",
            "extraction_events": extraction_events, "events": events, "error": error,
            "shared_reference": len(references) == 2 and references[0] is references[1]}


def main():
    helper_cases = []
    for name, tail in URL_CASES:
        url = "https://cdn.example.invalid/images/1/" + tail
        # Existing values are overwritten by this helper, even empty results.
        data = {"filename": "API_NAME", "extension": "API_EXT", "extra": True}
        helper_cases.append({"name": name, "url": url, "input_fields": copy.deepcopy(data),
                             "expected": text.nameext_from_url(url, data)})
    cases = []
    for family in FAMILIES:
        cases.extend(source_case(family, name, tail) for name, tail in URL_CASES)
        cases.append(source_case(family, "api_extension_mismatch", "picture.JPEG", {"ext": "PNG"} if family == "e621" else {"file_ext": "png"}))
        cases.append(source_case(family, "existing_filename_extension", "picture.JPEG", {"md5": "ABCDEF0123456789ABCDEF0123456789"} if family == "e621" else {"filename": "API_NAME", "extension": "API_EXT"}))
        cases.append(source_case(family, "existing_filename_only", "picture.JPEG", {"md5": "RAW_MD5"} if family == "e621" else {"filename": "API_NAME"}))
        for extension in ("jpeg", "jpe", "jfif", "jif", "jfi", "html"):
            for raw in (extension, extension.upper()):
                fields = {"ext": raw} if family == "e621" else {"filename": "API_NAME", "extension": raw}
                cases.append(source_case(family, "file_extension_map_" + raw, "picture." + raw, fields))
        for event in ("post", "file"):
            hooks = [{"name": "metadata", "mode": "modify", "event": event,
                      "fields": {"filename": "HOOK_NAME", "extension": "HOOK_EXT"}}]
            cases.append(source_case(family, "ownership_" + event, "picture.JPEG", hooks=hooks))
        for event in ("post", "file"):
            cases.append(source_case(family, "ownership_alias_" + event, "picture.PNG", hooks=[
                {"name": "metadata", "mode": "modify", "event": event,
                 "fields": {"extension": "jpeg"}},
            ]))
        cases.append(source_case(family, "ownership_post_then_file", "picture.JPEG", hooks=[
            {"name": "metadata", "mode": "modify", "event": "post", "fields": {"filename": "POST_NAME", "extension": "POST_EXT"}},
            {"name": "metadata", "mode": "modify", "event": "file", "fields": {"filename": "FILE_NAME", "extension": "FILE_EXT"}},
        ]))
    prepare_cases = []
    prepare_values = [("missing", {})]
    prepare_values.extend((name, {"extension": value}) for name, value in [
        ("null", None), ("false", False), ("true", True),
        ("integer", 7), ("zero", 0), ("negative", -3), ("float", 1.5),
        ("array_empty", []), ("array", ["jpeg"]), ("object_empty", {}),
        ("object", {"name": "jpeg"}), ("empty", ""), ("unknown", "custom"),
    ])
    for alias in ("jpeg", "jpe", "jfif", "jif", "jfi", "html"):
        prepare_values.extend(((alias, {"extension": alias}),
                               (alias.upper(), {"extension": alias.upper()})))
    for name, fields in prepare_values:
        obj = extractor.find(FAMILIES["danbooru"] + "tags=Original")
        pathfmt = path.PathFormat(obj)
        data = {"category": "danbooru", "search_tags": "Original", "id": 42,
                "filename": "picture", **copy.deepcopy(fields)}
        original = copy.deepcopy(data)
        error = None
        try:
            pathfmt.set_directory(data)
            pathfmt.set_filename(data)
        except Exception as exc:
            error = {"class": type(exc).__name__, "message": str(exc)}
        prepare_cases.append({"name": name, "input": original,
                              "output": copy.deepcopy(data), "error": error})
    root = Path(inspect.getfile(gallery_dl)).parent
    modules = (booru, common, danbooru, e621, gelbooru, gelbooru_v02, job, formatter, text, path, metadata_pp, pp_common)
    output = {"version": gallery_dl.version.__version__, "generator": __doc__,
              "source_sha256": {str(Path(inspect.getfile(module)).relative_to(root)):
                                hashlib.sha256(Path(inspect.getfile(module)).read_bytes()).hexdigest()
                                for module in modules},
              "input_schema_sha256": hashlib.sha256(Path(__file__).with_name("booru_search_generate.py").read_bytes()).hexdigest(),
              "helper_cases": helper_cases, "prepare_cases": prepare_cases, "cases": cases}
    Path(__file__).with_name("booru_filename.json").write_text(json.dumps(output, ensure_ascii=False, indent=2) + "\n")


if __name__ == "__main__":
    main()
