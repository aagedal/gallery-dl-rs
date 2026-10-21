#!/usr/bin/env python3
"""Genuine offline Safebooru favorite extraction, paths, hooks and archives.

The reusable runner uses real source metadata/posts/items/_pagination_html,
request_xml, DownloadJob dispatch, PathFormat, MetadataPP and publication.
Only request() response text and downloaded bytes are controlled. Default skip
is retained. Real source MetadataPP archive connections produce captured SQL.
Run with the authoritative gallery-dl interpreter and -B.
"""
import copy
import hashlib
import inspect
import json
from pathlib import Path
import sqlite3

import gallery_dl
from gallery_dl import extractor, config, formatter, job, path, text
from gallery_dl.extractor import booru, common, gelbooru_v02
from gallery_dl.postprocessor import metadata as metadata_pp, common as pp_common
import booru_dapi_pool_generate as runner
from booru_search_generate import input_post

FAMILY = "safebooru"
EXAMPLE = gelbooru_v02.GelbooruV02FavoriteExtractor.example
QUERY = EXAMPLE.partition("?")[2].replace("12345", "7")


def html(ids):
    return "no title or pool metadata " + "".join(f'<a id="p{pid}">' for pid in ids)


def response(rows):
    return runner.api_response(FAMILY, rows)


def run(name, responses, query=QUERY, options=None, hooks=None, limit=None, rows=None):
    case = runner.run_case(FAMILY, name, responses, options=options, hook_options=hooks,
                           consume_limit=limit, query=query, input_posts=rows)
    archives = []
    artifacts = []
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


def standard(name, ids=None, overrides=None, query=QUERY, options=None, hooks=None, limit=None, page_html=None):
    ids = [42, 41] if ids is None else ids
    responses = [{"kind": "html", "body": html(ids) if page_html is None else page_html, "status": 200}]
    rows = []
    for pid in ids:
        row = dict(input_post(FAMILY), id=pid)
        current = copy.deepcopy(overrides[pid]) if overrides and pid in overrides else [row]
        rows.extend(current)
        responses.append(response(current))
    return run(name, responses, query, options, hooks, limit, rows)


def main():
    def modify(event, fields):
        return {"name": "metadata", "mode": "modify", "event": event, "fields": fields}
    cases = [standard("two_ordered_members")]
    for lexical in ("0", "0007", "7suffix"):
        cases.append(standard("owner_" + lexical, ids=[42], query=QUERY.replace("id=7", "id=" + lexical)))
    cases.append(standard("no_members", ids=[]))
    cases.append(standard("duplicate_members", ids=[42, 42, 41]))
    cases.append(standard("empty_api_then_next", overrides={42: []}))
    cases.append(standard("multiple_rows_one_member", ids=[42], overrides={42: [dict(input_post(FAMILY), id=42), dict(input_post(FAMILY), id=43)]}))
    cases.append(standard("cancel_zero", limit=0))
    cases.append(standard("cancel_first", limit=1))
    cases.append(standard("configured_auth", ids=[42], options={"api-key": "SYNTHETIC_KEY", "user-id": "12345678"}))
    # Actual Python regex accepts Unicode whitespace and Nd decimal digits;
    # return ASCII API identities independently of the lexical requested ID.
    marker_cases = [
        ("marker_exact", 'id="p999"<a class="any" id="p42suffix\' ><a id=\'p998\'>', [42]),
        ("marker_unicode_whitespace", '<a\u001cid="p42"><a\u00a0id="p43">', [42, 43]),
        ("marker_unicode_decimal", '<a id="p４２"><a id="p٤٣">', ["４２", "٤٣"]),
    ]
    for name, body, ids in marker_cases:
        responses = [{"kind": "html", "body": body, "status": 200}]
        rows = []
        for pid in ids:
            row = dict(input_post(FAMILY), id=int(pid))
            rows.append(row)
            responses.append(response([row]))
        cases.append(run(name, responses, rows=rows))
    row = dict(input_post(FAMILY), id="00042", tags="  cat  dog \t")
    cases.append(standard("raw_xml_id_tags", ids=[42], overrides={42: [row]}))
    for field in ("tags", "created_at"):
        row = dict(input_post(FAMILY), id=41)
        del row[field]
        case = standard("valid_then_missing_" + field, ids=[42, 41, 40], overrides={41: [row]})
        case["expected_prepare_error"] = True
        cases.append(case)
    row = input_post(FAMILY)
    for field in ("file_url", "tags", "created_at"):
        del row[field]
    cases.append(standard("missing_file_url_skip", overrides={42: [row]}))
    for name, body in (("empty_xml", "<posts />"), ("xml_error", "<error>Bad request</error>"),
                       ("xml_auth_error", "<error>Missing authentication credentials</error>"),
                       ("xml_malformed", "<posts><post")):
        responses = [{"kind": "html", "body": html([42]), "status": 200},
                     {"kind": "xml", "body": body, "status": 200}]
        cases.append(run(name, responses))
    for event in ("post", "file", "post-after"):
        cases.append(standard("scalar_" + event, hooks=[modify(event, {"favorite_id": event.upper()})]))
        cases.append(standard("delete_" + event, hooks=[{"name": "metadata", "mode": "delete", "event": event, "fields": ["favorite_id"]}]))
    cases.append(standard("all_scalar_hooks", hooks=[modify(event, {"favorite_id": event.upper()}) for event in ("post", "file", "post-after")]))
    for name, overrides in (
        ("directory_override", {"directory": ["custom", "{favorite_id}"]}),
        ("filename_override", {"filename": "chosen_{id}.{extension}"}),
        ("both_overrides", {"directory": [], "filename": "chosen_{id}.{extension}"}),
        ("conditional_override", {"directory": {"id == 42": ["match"], "": ["other"]}, "filename": {"id == 42": "match_{id}.{extension}", "": "other_{id}.{extension}"}}),
    ):
        cases.append(standard(name, options=overrides))
    # Real MetadataPP uses the source extractor archive format implicitly.
    observer = {"name": "metadata", "mode": "jsonl", "event": "file", "filename": "files.jsonl",
                "base-directory": True, "directory": [], "archive": [":basedirectory", "metadata.sqlite"],
                "include": ["favorite_id", "id", "category", "filename", "extension", "tags"]}
    cases.append(standard("actual_metadata_archive", hooks=[observer]))
    cases.append(standard("actual_mutated_metadata_archive", hooks=[modify("file", {"favorite_id": "HOOK"}), observer]))
    first = list(range(100, 50, -1))
    rows = [dict(input_post(FAMILY), id=pid) for pid in first + [50]]
    responses = [{"kind": "html", "body": html(first), "status": 200}]
    responses.extend(response([row]) for row in rows[:-1])
    responses.append({"kind": "html", "body": html([50]), "status": 200})
    responses.append(response(rows[-1:]))
    cases.append(run("50_boundary_offset50", responses, rows=rows))
    cases.append(run("cancel50_before_next_html", responses, limit=50, rows=rows))
    fewer = copy.deepcopy(responses)
    fewer[1] = response([])  # Fifty HTML IDs, but only49 API rows on firstpage.
    cases.append(run("50_ids_fewer_api_rows_still_offset50", fewer, rows=rows[1:]))
    # No source producer num exists, but user-added counters live on this
    # post's shared dictionary until the next fresh API post replaces it.
    for event in ("post", "file", "post-after"):
        cases.append(standard("user_num_" + event,
                              hooks=[modify(event, {"num": "{id!i:A+57/}"})]))
    routes = []
    for query in (QUERY, QUERY.replace("id=7", "id=0"), QUERY.replace("id=7", "id=0007"),
                  QUERY + "suffix", QUERY + "&id=8", "s=view&page=favorites&id=7",
                  "page=favorites&id=7&s=view", "page=favorites&s=view&id=", "page=favorites&s=view&id=-7"):
        url = EXAMPLE.partition("?")[0] + "?" + query
        obj = extractor.find(url)
        routes.append({"url": url, "extractor": type(obj).__name__ if obj else None,
                       "captured_id": obj.groups[-1] if obj else None,
                       "archive_format": obj.archive_fmt if obj else None})
    expected_errors = ("valid_then_missing_tags", "valid_then_missing_created_at", "xml_error", "xml_auth_error", "xml_malformed")
    assert all(case["error"] is None or case["name"].endswith(expected_errors) for case in cases)
    assert all(case["error"] is not None for case in cases if case["name"].endswith(expected_errors))
    modules = (common, booru, gelbooru_v02, config, formatter, job, path, text, metadata_pp, pp_common)
    root = Path(inspect.getfile(gallery_dl)).parent
    output = {"version": gallery_dl.version.__version__, "generator": __doc__,
              "source_sha256": {str(Path(inspect.getfile(module)).relative_to(root)):
                                hashlib.sha256(Path(inspect.getfile(module)).read_bytes()).hexdigest()
                                for module in modules},
              "runner_sha256": hashlib.sha256(Path(inspect.getfile(runner)).read_bytes()).hexdigest(),
              "input_schema_sha256": hashlib.sha256(Path(__file__).with_name("booru_search_generate.py").read_bytes()).hexdigest(),
              "source_skip_default": True, "excluded_internal_artifacts": ["metadata.sqlite"],
              "route_cases": routes, "cases": cases}
    Path(__file__).with_name("booru_dapi_favorite.json").write_text(json.dumps(output, ensure_ascii=False, indent=2) + "\n")


if __name__ == "__main__":
    main()
