"""Offline genuine collection route oracle for gallery-dl 1.32.17.

Regenerate from the repository root:
/private/tmp/gdlvenv/bin/python -B tests/fixtures/source_archive/booru_collection_routes_generate.py

Only authentication discovery and request_json are stubbed. Actual extractor.find,
_init, metadata, posts, pagination and archive formatting execute unmodified.
"""
import copy
import hashlib
import json
import logging
from pathlib import Path
import sys
from gallery_dl import config, extractor, formatter, version
from gallery_dl.extractor import common, danbooru, e621

logging.disable(logging.CRITICAL)
HUGE_ID = "999999999999999999999999999999999999999999999"
SUFFIXES = [
    ("ordinary", "7"),
    ("leading_zero", "0007"),
    ("zero", "0"),
    ("all_zero", "0000"),
    ("beyond_u64", HUGE_ID),
    ("word_suffix", "7tail"),
    ("path_suffix", "7/extra/path"),
    ("json_suffix", "7.json"),
    ("encoded_digit_suffix", "7%32"),
    ("encoded_slash_suffix", "7%2F999"),
    ("leading_zero_encoded_suffix", "0007%2ejson"),
    ("query", "0007?tags=cat&other=9"),
    ("fragment", "0007#suffix"),
    ("query_fragment", "0007?tags=cat&other=9#fragment"),
    ("encoded_initial_digit", "%37"),
    ("encoded_initial_zeros", "%30%30%37"),
    ("positive_sign", "+7"),
    ("negative_sign", "-7"),
    ("nonnumeric", "word7"),
    ("empty", ""),
]

def run(name, family, url):
    config.clear()
    obj = extractor.find(url)
    out = dict(name=name, family=family, url=url, extractor=None,
               category=None, root=None, groups=None, captured_id=None,
               archive_format=None, error=None, calls=[], metadata_response=None,
               metadata=None, posts=[], archive_keys=[])
    if obj is None:
        out["error"] = "NoExtractor"
        return out
    out.update(extractor=type(obj).__name__, category=obj.category, root=obj.root,
               groups=list(obj.groups), captured_id=obj.groups[-1],
               archive_format=obj.archive_fmt)
    if obj.subcategory not in ("pool", "favgroup"):
        raise AssertionError((name, type(obj).__name__))
    response = {
        "id": 99,
        "name": "fixture_collection_name",
        "description": "synthetic description",
        "post_ids": [42],
    }
    out["metadata_response"] = copy.deepcopy(response)
    obj._get_auth_info = lambda: (None, None)
    obj._init()
    out["per_page"] = obj.per_page
    out["threshold"] = obj.threshold
    def request(url, *, params=None, **kwargs):
        out["calls"].append({"url": url, "params": copy.deepcopy(params)})
        if url.endswith("/posts.json"):
            rows = [{"id": 42}]
            return {"posts": rows} if family == "e621" else rows
        return copy.deepcopy(response)
    obj.request_json = request
    out["metadata"] = obj.metadata()
    out["posts"] = list(obj.posts())
    keygen = formatter.parse(obj.archive_fmt).format_map
    for post in out["posts"]:
        keywords = post.copy()
        keywords.update(out["metadata"])
        out["archive_keys"].append(keygen(keywords))
    return out

cases = []
for label, family, host in [
    ("danbooru", "danbooru", "danbooru.donmai.us"),
    ("e621", "e621", "e621.net"),
    ("e926", "e621", "e926.net"),
]:
    for route, prefix in [("modern", "/pools/"), ("legacy", "/pool/show/")]:
        for name, suffix in SUFFIXES:
            cases.append(run(f"{label}_pool_{route}_{name}", family,
                             "https://" + host + prefix + suffix))
    for name, path in [
        ("double_initial_slash", "//pools/7"),
        ("wrong_case", "/Pools/7"),
        ("missing_id_separator", "/pools7"),
        ("encoded_route", "/%70ools/7"),
        ("wrong_singular", "/pool/7"),
        ("legacy_missing_show", "/pool/show7"),
    ]:
        cases.append(run(f"{label}_pool_{name}", family, "https://" + host + path))
for route, prefix in [("modern", "/favorite_groups/"), ("legacy", "/favorite_group/show/")]:
    for name, suffix in SUFFIXES:
        cases.append(run(f"danbooru_favgroup_{route}_{name}", "danbooru",
                         "https://danbooru.donmai.us" + prefix + suffix))
for name, path in [
    ("double_initial_slash", "//favorite_groups/7"),
    ("wrong_case", "/Favorite_groups/7"),
    ("encoded_route", "/favorite_%67roups/7"),
    ("wrong_singular", "/favorite_group/7"),
    ("legacy_missing_show", "/favorite_group/show7"),
]:
    cases.append(run("danbooru_favgroup_" + name, "danbooru",
                     "https://danbooru.donmai.us" + path))
for host in ["e621.net", "e926.net"]:
    for path in ["/favorite_groups/7", "/favorite_group/show/0007"]:
        cases.append(run(host.split(".")[0] + "_unsupported_" + path.split("/")[1],
                         "e621", "https://" + host + path))
modules = {"extractor/__init__.py": extractor, "extractor/common.py": common,
           "extractor/danbooru.py": danbooru, "extractor/e621.py": e621,
           "formatter.py": formatter}
result = {
    "version": version.__version__,
    "source_sha256": {name: hashlib.sha256(Path(module.__file__).read_bytes()).hexdigest()
                      for name, module in modules.items()},
    "generator": __doc__,
    "scope": "Canonical HTTPS Danbooru/e621/e926 hosts; ASCII digit collection captures. Source Unicode decimal digits are deliberately outside this native Url-normalized route unit. Constructor prefix matching is unmodified and suffixes are retained in input URLs. Metadata ID 99 intentionally differs from requested lexical ID; actual source does not bind response identity to request identity. No HTTP, files, or source edits.",
    "cases": cases,
}
out = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).with_name("booru_collection_routes.json")
out.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
print(f"{len(cases)} cases, {sum(c['error'] is None for c in cases)} matched")
