"""Regenerate: /private/tmp/gdlvenv/bin/python archive_generate.py archive.json.
Actual gallery-dl 1.32.17 MetadataPP and archive classes; no network/media.
"""
import contextlib
import copy
import hashlib
import io
import json
import logging
import os
from pathlib import Path
import sqlite3
import sys
import tempfile
from types import SimpleNamespace
from gallery_dl import archive, version
from gallery_dl.postprocessor.metadata import MetadataPP

class Job:
    def __init__(self, path):
        self.pathfmt = path
        self.extractor = SimpleNamespace(category="fixture", archive_fmt="{id}_{num}.{extension}")
        self.hooks = []
    def get_logger(self, name): return logging.getLogger(name)
    def register_hooks(self, hooks, *args): self.hooks.extend(hooks)

def normalize(value, root):
    if isinstance(value, str): return value.replace(str(root), "$WORK")
    if isinstance(value, list): return [normalize(v, root) for v in value]
    if isinstance(value, dict): return {k: normalize(v, root) for k,v in value.items()}
    return value

def run_case(root, name, extra, action="check_add", mutation=None):
    folder = root / name
    folder.mkdir()
    keywords = dict(id=17, num=2, extension="jpg", tags=["alpha", "alpha", "beta"])
    path = SimpleNamespace(kwdict=keywords, filename="17_2.jpg", extension="jpg", realdirectory=str(folder)+"/", basedirectory=str(root)+"/", extended=False)
    job = Job(path)
    options = dict(archive=str(folder / "archive.sqlite"), **extra)
    pp = MetadataPP(job, options)
    result = dict(name=name, action=action, options=options, keywords=copy.deepcopy(keywords), hooks=job.hooks, mutation=mutation)
    try:
        if action == "check_add":
            result["before"] = bool(pp.archive.check(keywords))
            result["cached"] = keywords["_archive_metadata"]
            if mutation: keywords.update(mutation)
            pp.archive.add(keywords)
            result["after"] = bool(pp.archive.check(keywords))
        else:
            if action == "skip_existing": (folder / "17_2.jpg.json").write_text("existing")
            if action == "render_error":
                def fail(_): raise ValueError("synthetic render failure")
                pp.write = fail
            if action == "write_error":
                def fail(_): raise OSError("synthetic write failure")
                pp.open = fail
            with contextlib.redirect_stdout(io.StringIO()) as out: pp.run(path)
            result["stdout"] = out.getvalue()
    except Exception as exc:
        result["error"] = type(exc).__name__
    result["keywords_after"] = copy.deepcopy(keywords)
    if pp.archive:
        con = pp.archive.connection
        result["tables"] = [{"name": table, "sql": sql, "rows": [r[0] for r in con.execute('SELECT entry FROM '+archive.sanitize(table)+' ORDER BY entry')]} for table, sql in con.execute("SELECT name,sql FROM sqlite_master WHERE type='table' ORDER BY name")]
        pp._archive_close(None)
    result["files"] = sorted(p.name for p in folder.iterdir() if not p.name.startswith("archive.sqlite"))
    return normalize(result, root)

with tempfile.TemporaryDirectory(prefix="gdl-source-archive-") as tmp:
    root=Path(tmp)
    cases=[]
    for name, options, action, mutation in [
        ("default", {}, "check_add", None),
        ("explicit_format", {"archive-format":"{id}"}, "check_add", None),
        ("explicit_prefix", {"archive-prefix":"custom_"}, "check_add", None),
        ("explicit_empty_prefix", {"archive-prefix":""}, "check_add", None),
        ("explicit_table", {"archive-table":"artist_{id}"}, "check_add", None),
        ("empty_table", {"archive-table":""}, "check_add", None),
        ("quoted_table", {"archive-table":"a\"b"}, "check_add", None),
        ("cached_key_survives_mutation", {}, "check_add", {"id":99}),
        ("empty_cache_regenerates", {}, "check_add", {"id":99,"_archive_metadata":""}),
        ("cached_key_override", {}, "check_add", {"_archive_metadata":"replacement"}),
        ("json_success", {}, "run", None),
        ("tags_success", {"mode":"tags"}, "run", None),
        ("custom_success", {"mode":"custom","format":"{id}"}, "run", None),
        ("jsonl_success", {"mode":"jsonl"}, "run", None),
        ("skip_existing", {"skip":True}, "skip_existing", None),
        ("empty_no_file_is_recorded", {"mode":"custom","format":"","empty":False}, "run", None),
        ("empty_file_is_recorded", {"mode":"custom","format":"","empty":True}, "run", None),
        ("render_failure_not_recorded", {}, "render_error", None),
        ("write_failure_not_recorded", {}, "write_error", None),
        ("metadata_path_overwrites_cache", {"metadata-path":"_archive_metadata"}, "run", None),
        ("modify_bypasses_archive", {"mode":"modify","fields":{"new":"{id}"}}, "run", None),
        ("delete_bypasses_archive", {"mode":"delete","fields":["id"]}, "run", None),
        ("print_bypasses_archive", {"mode":"print","format":"{id}"}, "run", None),
        ("stdout_bypasses_archive", {"filename":"-"}, "run", None),
        ("file_mode_ignores_archive_mode", {"archive-mode":"memory"}, "run", None),
        ("pragmas", {"archive-pragma":["journal_mode=WAL","synchronous=NORMAL"]}, "check_add", None),
    ]:
        cases.append(run_case(root,name,options,action,mutation))
    reuse=[]
    path=str(root/"reuse.sqlite")
    for reuse_flag in [True,True,False]:
        obj=archive.connect(path,"","{id}",reuse=reuse_flag,pragma=["cache_size=23" if not reuse else "cache_size=47"],cache_key="_archive_metadata")
        kw={"id":1}; hit=bool(obj.check(kw)); obj.add(kw)
        reuse.append(dict(reuse=reuse_flag,hit=hit,cache_size=obj.connection.execute("PRAGMA cache_size").fetchone()[0]))
        obj.close()
    for con in archive.CACHE_CONNECTIONS.values(): con.close()
    archive.CACHE_CONNECTIONS.clear()
    result=dict(version=version.__version__,source_sha256={name:hashlib.sha256(Path(module.__file__).read_bytes()).hexdigest() for name,module in [("archive.py",archive),("metadata.py",__import__("gallery_dl.postprocessor.metadata",fromlist=["x"])),("common.py",__import__("gallery_dl.postprocessor.common",fromlist=["x"]))]},source="Actual MetadataPP construction/run, archive.check/add/close and SQLite results. Synthetic public metadata and minimal path adapter; string paths CWD-relative upstream. No network. See adjacent generator.",cases=cases,reuse=reuse)
    Path(sys.argv[1]).write_text(json.dumps(result,ensure_ascii=False,indent=2)+"\n")
    print(len(cases))
