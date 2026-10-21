#!/usr/bin/env python3
"""Offline Gelbooru/Safebooru pool transport/traversal/path/ownership oracle.

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
from urllib.parse import parse_qsl, urlsplit
import xml.etree.ElementTree as ET

import gallery_dl
from gallery_dl import config, extractor, formatter, job, path
from gallery_dl.extractor import booru, common, danbooru, e621, gelbooru, gelbooru_v02
from gallery_dl.postprocessor import metadata as metadata_pp, common as pp_common, hash as hash_pp
from booru_search_generate import input_post, normalize

HOSTS = {"gelbooru": "gelbooru.com", "safebooru": "safebooru.org"}




def run_case(family, name, responses, options=None, hook_options=None, consume_limit=None,
             query="page=pool&s=show&id=7", input_posts=None):
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
        request_kind = "html"
        def transport(request_url, params=None, **kwargs):
            nonlocal response_index
            kind = request_kind
            wire = dict(parse_qsl(urlsplit(request_url).query, keep_blank_values=True))
            wire.update({key: value for key, value in (params or {}).items() if value is not None})
            calls.append({"method": kind, "url": request_url, "params": copy.deepcopy(params),
                          "wire_params": wire})
            if response_index >= len(responses):
                raise RuntimeError("UnexpectedRequestAfterScript")
            response = responses[response_index]
            response_index += 1
            if response["kind"] != kind:
                raise RuntimeError("UnexpectedTransportKind:" + kind)
            body = copy.deepcopy(response["body"])
            return SimpleNamespace(text=json.dumps(body) if kind == "json" else body)
        # Only request() transport is stubbed. Real source request_json and
        # request_xml parse controlled response text, including malformed XML.
        real_request_json, real_request_xml = obj.request_json, obj.request_xml
        def request_json(*args, **kwargs):
            nonlocal request_kind
            request_kind = "json"
            try:
                return real_request_json(*args, **kwargs)
            finally:
                request_kind = "html"
        def request_xml(*args, **kwargs):
            nonlocal request_kind
            request_kind = "xml"
            try:
                return real_request_xml(*args, **kwargs)
            finally:
                request_kind = "html"
        obj.request = transport
        obj.request_json, obj.request_xml = request_json, request_xml
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
                  "hook_options": hook_options or [], "input_posts": input_posts or [],
                  "responses": responses, "download_body": "source-default-path-fixture",
                  "consume_limit": consume_limit, "stopped_by_consumer": stopped_by_consumer,
                  "calls": calls, "extraction_events": extraction, "trace": trace,
                  "published": published, "error": error,
                  "unconsumed_response_count": len(responses) - response_index}
    config.clear()
    return result




def html(family, ids, title="My &amp; Pool", include_title=True):
    heading = "<h3>Now Viewing: " if family == "gelbooru" else "<h4>Pool: "
    end = "</h3>" if family == "gelbooru" else "</h4>"
    return (heading + title + end if include_title else "missing title") + "".join(
        f'<a class="thumb" id="p{pid}">' for pid in ids)


def api_response(family, rows):
    if family == "gelbooru":
        return {"kind": "json", "body": {"post": copy.deepcopy(rows)}, "status": 200}
    root = ET.Element("posts")
    for row in rows:
        ET.SubElement(root, "post", {key: str(value) for key, value in row.items()})
    return {"kind": "xml", "body": ET.tostring(root, encoding="unicode"), "status": 200}


def standard(family, name, ids=None, title="My &amp; Pool", options=None, hook_options=None,
             consume_limit=None, query="page=pool&s=show&id=7", overrides=None):
    ids = [42, 41] if ids is None else ids
    page = html(family, ids, title)
    responses = [{"kind": "html", "body": page, "status": 200}]
    if family == "gelbooru":
        responses.append({"kind": "html", "body": page, "status": 200})
    posts = []
    for pid in ids:
        rows = [dict(copy.deepcopy(input_post(family)), id=pid)]
        if overrides and pid in overrides:
            rows = copy.deepcopy(overrides[pid])
        posts.extend(rows)
        responses.append(api_response(family, rows))
    return run_case(family, name, responses, options, hook_options, consume_limit, query, posts)


def main():
    def modify(event, fields):
        return {"name": "metadata", "mode": "modify", "event": event, "fields": fields}
    cases = []
    for family in HOSTS:
        cases.append(standard(family, "ordered_two_members"))
        for lexical in ("0", "0007", "7suffix"):
            cases.append(standard(family, "lexical_" + lexical, ids=[42], query="page=pool&s=show&id=" + lexical))
        cases.append(standard(family, "entity_title", ids=[42], title="A_B&amp;B &NotEqualTilde; &#x80;"))
        for name, body in (("missing_title", html(family, [], include_title=False)),
                           ("empty_title", html(family, [], title=""))):
            cases.append(run_case(family, name, [{"kind": "html", "body": body, "status": 200}]))
        cases.append(standard(family, "whitespace_title", ids=[42], title="  "))
        cases.append(standard(family, "empty_pool", ids=[]))
        cases.append(standard(family, "duplicate_members", ids=[42, 42, 41]))
        cases.append(standard(family, "zero_result_then_later", overrides={42: []}))
        row = input_post(family)
        cases.append(standard(family, "multiple_results_one_member", ids=[42], overrides={42: [dict(row, id=42), dict(row, id=43)]}))
        cases.append(standard(family, "consumer_zero", consume_limit=0))
        cases.append(standard(family, "consumer_first", consume_limit=1))
        cases.append(standard(family, "configured_auth", ids=[42], options={"api-key": "SYNTHETIC_KEY", "user-id": "12345678"}))
        for event in ("post", "file", "post-after"):
            cases.append(standard(family, "scalar_" + event, hook_options=[modify(event, {"pool": "HOOK", "pool_name": event.upper()})]))
        cases.append(standard(family, "scalar_all_hooks", hook_options=[modify(event, {"pool": event.upper(), "pool_name": event.upper()}) for event in ("post", "file", "post-after")]))
        cases.append(standard(family, "scalar_delete_file", hook_options=[{"name": "metadata", "mode": "delete", "event": "file", "fields": ["pool", "pool_name"]}]))
        # A controlled later malformed response is parsed by the real source API
        # helper after one successfully published file; no later member fetch.
        normal = standard(family, "temporary", ids=[42, 41])
        responses = copy.deepcopy(normal["responses"])
        responses[-1] = ({"kind": "json", "body": {"post": None}, "status": 200} if family == "gelbooru" else
                         {"kind": "xml", "body": "<posts><post", "status": 200})
        cases.append(run_case(family, "late_malformed_api", responses, input_posts=normal["input_posts"]))
    # Gelbooru metadata reads HTML separately, then posts rereads pid=0 and
    # increments the offset by its real45-item page size only on resumed poll.
    first_ids = list(range(100, 55, -1))
    first_page = html("gelbooru", first_ids)
    responses = [{"kind": "html", "body": first_page, "status": 200},
                 {"kind": "html", "body": first_page, "status": 200}]
    posts = []
    for pid in first_ids:
        row = dict(input_post("gelbooru"), id=pid)
        posts.append(row)
        responses.append(api_response("gelbooru", [row]))
    responses.append({"kind": "html", "body": html("gelbooru", [55]), "status": 200})
    row = dict(input_post("gelbooru"), id=55)
    posts.append(row)
    responses.append(api_response("gelbooru", [row]))
    cases.append(run_case("gelbooru", "45_boundary_offset45", responses, input_posts=posts))
    cases.append(run_case("gelbooru", "consumer45_before_next_html", responses, consume_limit=45, input_posts=posts))
    for name, body in (("error_root", "<error>Bad request</error>"),
                       ("auth_error_root", "<error>Missing authentication credentials</error>"),
                       ("empty_xml_root", "<posts />")):
        responses = [{"kind": "html", "body": html("safebooru", [42]), "status": 200},
                     {"kind": "xml", "body": body, "status": 200}]
        cases.append(run_case("safebooru", name, responses, options={"api-key": "SYNTHETIC_KEY", "user-id": "12345678"}))
    # Source Gel JSON accepts a single post object as well as an array; a
    # missing post key means an empty result, without ending HTML traversal.
    row = input_post("gelbooru")
    for name, body in (("single_object_json", {"post": row}), ("missing_post_json", {})):
        responses = [{"kind": "html", "body": html("gelbooru", [42]), "status": 200},
                     {"kind": "html", "body": html("gelbooru", [42]), "status": 200},
                     {"kind": "json", "body": body, "status": 200}]
        cases.append(run_case("gelbooru", name, responses, input_posts=[row]))
    for family in HOSTS:
        body = '<a class="thumb" id="p999">' + html(family, [42]) + '<a class="other" id="p43">' + "<a class='thumb' id='p44'>"
        ids = [999, 42, 43] if family == "gelbooru" else [42]
        rows = [dict(input_post(family), id=pid) for pid in ids]
        responses = [{"kind": "html", "body": body, "status": 200}]
        if family == "gelbooru":
            responses.append({"kind": "html", "body": body, "status": 200})
        responses.extend(api_response(family, [row]) for row in rows)
        cases.append(run_case(family, "exact_html_markers_and_title_position", responses, input_posts=rows))
    # Both sites inherit GelbooruV02._prepare. Preserve URL-before-prepare
    # ordering, and let real XML decoding establish attribute string types.
    for family in HOSTS:
        malformed = []
        row = input_post(family)
        del row["tags"]
        malformed.append(("missing_tags", row))
        row = input_post(family)
        del row["created_at"]
        malformed.append(("missing_date", row))
        if family == "gelbooru":
            for name, value in (("null", None), ("number", 42), ("array", []), ("object", {})):
                malformed.append(("nonstring_tags_" + name, dict(input_post(family), tags=value)))
        else:
            # XML attributes are text even if this serializer's input is a
            # number. Do not fabricate impossible non-string decoded XML tags.
            cases.append(standard(family, "numeric_tags_are_xml_text", ids=[42],
                                  overrides={42: [dict(input_post(family), tags=42)]}))
        for name, row in malformed:
            case = standard(family, "prepare_" + name, ids=[42], overrides={42: [row]})
            case["expected_prepare_error"] = True
            cases.append(case)
        for field in ("tags", "created_at"):
            row = dict(input_post(family), id=41)
            del row[field]
            case = standard(family, "valid_first_then_missing_" + field, ids=[42, 41, 40],
                            overrides={41: [row]})
            case["expected_prepare_error"] = True
            cases.append(case)
        row = input_post(family)
        for field in ("file_url", "tags", "created_at"):
            del row[field]
        cases.append(standard(family, "missing_file_url_skips_before_validation", ids=[42, 41],
                              overrides={42: [row]}))
    cases.append(standard("gelbooru", "outer_tag_whitespace", ids=[42],
                          overrides={42: [dict(input_post("gelbooru"), tags="  \t\u001ccat  dog\u001f \n ")]}))
    for extension in ("mp4", "webm"):
        row = dict(input_post("gelbooru"),
                   file_url="https://img3.gelbooru.com/images/1/original." + extension,
                   preview_url="https://img4.gelbooru.com/thumbnails/preview.jpg")
        cases.append(standard("gelbooru", "video_rewrite_" + extension, ids=[42], overrides={42: [row]}))
    row = dict(input_post("gelbooru"), file_url="https://img3.gelbooru.com/images/1/original.mp4?token=1")
    cases.append(standard("gelbooru", "video_query_suffix_no_rewrite", ids=[42], overrides={42: [row]}))
    for field in ("preview_url", "md5"):
        row = dict(input_post("gelbooru"), file_url="https://img3.gelbooru.com/images/1/original.mp4", preview_url="https://img4.gelbooru.com/thumbnails/preview.jpg")
        del row[field]
        cases.append(standard("gelbooru", "video_missing_" + field + "_skips", ids=[42, 41], overrides={42: [row]}))
    row = dict(input_post("safebooru"), file_url="/images/99/root-relative.jpg")
    del row["directory"]
    del row["image"]
    cases.append(standard("safebooru", "root_relative_file_url_without_image_fields", ids=[42], overrides={42: [row]}))
    routes = []
    for family, host in HOSTS.items():
        for query in ("page=pool&s=show&id=0", "page=pool&s=show&id=0007", "page=pool&s=show&id=7suffix",
                      "s=show&page=pool&id=7", "page=pool&id=7&s=show", "page=pool&s=show&id=-7",
                      "page=pool&s=show&id=", "page=pool&s=show&id=7&id=8"):
            url = f"https://{host}/index.php?{query}"
            obj = extractor.find(url)
            routes.append({"url": url, "extractor": type(obj).__name__ if obj else None,
                           "captured_id": obj.groups[-1] if obj else None,
                           "archive_format": obj.archive_fmt if obj else None})
    expected_errors = ("missing_title", "empty_title", "late_malformed_api", "error_root", "auth_error_root")
    assert all(case["error"] is None or case["name"].endswith(expected_errors) or case.get("expected_prepare_error") for case in cases)
    assert all(case["error"] is not None for case in cases if case["name"].endswith(expected_errors) or case.get("expected_prepare_error"))
    root = Path(inspect.getfile(gallery_dl)).parent
    modules = (common, booru, gelbooru, gelbooru_v02, config, formatter, job, path, metadata_pp, pp_common)
    output = {"version": gallery_dl.version.__version__, "generator": __doc__,
              "source_sha256": {str(Path(inspect.getfile(module)).relative_to(root)):
                                hashlib.sha256(Path(inspect.getfile(module)).read_bytes()).hexdigest()
                                for module in modules},
              "input_schema_sha256": hashlib.sha256(Path(__file__).with_name("booru_search_generate.py").read_bytes()).hexdigest(),
              "source_skip_default": True, "route_cases": routes, "cases": cases}
    Path(__file__).with_name("booru_dapi_pool.json").write_text(json.dumps(output, ensure_ascii=False, indent=2) + "\n")


if __name__ == "__main__":
    main()
