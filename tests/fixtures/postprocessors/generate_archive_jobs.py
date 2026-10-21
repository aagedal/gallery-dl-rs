"""Record actual DownloadJob + MetadataPP SQLite archive lifecycle, offline.

Regenerate with gallery-dl installed:
  python tests/fixtures/postprocessors/generate_archive_jobs.py [archive_jobs.json]
Only synthetic Extractor messages and text: downloads are used. Production
classes are unmodified; register_hooks is wrapped solely to observe callbacks.
SQLite files are inspected after each real job, never replaced with a model.
"""
import contextlib
import copy
import hashlib
import io
import json
import logging
import os
from pathlib import Path
import re
import sqlite3
import sys
import tempfile
from unittest.mock import patch

from gallery_dl import archive, config, job, version
from gallery_dl.extractor.common import Extractor, Message

GENERATOR = "tests/fixtures/postprocessors/generate_archive_jobs.py"
KEYS = (
    "id", "num", "phase", "payload", "scope", "removed", "changed",
    "metadata_path", "_archive_metadata", "category", "subcategory",
    "filename", "extension", "init_seen", "child_seen",
)
CURRENT = None


def keywords(data):
    return {key: copy.deepcopy(data[key]) for key in KEYS if key in data}


def snapshot(stage, extractor=None, data=None, **extra):
    record = {"stage": stage, "job": CURRENT.phase}
    if extractor is not None:
        record["extractor"] = extractor.subcategory
    if data is not None:
        record["keywords"] = keywords(data)
    record.update(extra)
    CURRENT.trace.append(record)


class Empty(Extractor):
    category = "fixture"
    subcategory = "empty"
    directory_fmt = ()
    filename_fmt = "file-{phase}-{id}-{num}.{extension}"
    archive_fmt = "{id}_{num}"

    @classmethod
    def from_url(cls, url):
        return cls(re.match(".*", url))

    def initialize(self):
        snapshot("extractor.initialize", self)
        Extractor.initialize(self)

    def items(self):
        snapshot("source.empty", self)
        return iter(())

    def cookies_store(self):
        pass


class Source(Empty):
    subcategory = "archive-job"

    def items(self):
        spec = CURRENT.spec
        data = dict(id=1, num=0, phase=CURRENT.phase, payload="first",
                    scope="directory", removed="present", extension="txt")
        CURRENT.live = data
        snapshot("source.emit.directory", self, data)
        yield Message.Directory, "", data
        snapshot("source.resume.directory", self, data)
        for num in (1, 2):
            data["num"] = num
            data["payload"] = spec.get("payloads", ["first", "second"])[num - 1]
            snapshot("source.emit.url", self, data, url=f"text:body-{num}")
            yield Message.Url, f"text:body-{num}", data
            snapshot("source.resume.url", self, data)
        if spec.get("empty_child"):
            data["_extractor"] = Empty
            snapshot("source.emit.child", self, data)
            yield Message.Queue, "https://fixture.invalid/empty", data
            snapshot("source.resume.child", self, data)
        snapshot("source.end", self, data)


def sql_artifacts(root):
    artifacts = []
    for path in sorted(root.rglob("*")):
        if not path.is_file():
            continue
        raw = path.read_bytes()
        item = {"path": path.relative_to(root).as_posix()}
        if raw.startswith(b"SQLite format 3\0"):
            connection = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
            try:
                tables = []
                for name, sql in connection.execute(
                        "SELECT name, sql FROM sqlite_master "
                        "WHERE type='table' ORDER BY name"):
                    quoted = '"' + name.replace('"', '""') + '"'
                    rows = [list(row) for row in connection.execute(
                        f"SELECT * FROM {quoted} ORDER BY entry")]
                    tables.append(dict(name=name, sql=sql, rows=rows))
                item.update(kind="sqlite", tables=tables)
            finally:
                connection.close()
        else:
            try:
                item.update(kind="file", utf8=raw.decode("utf-8"))
            except UnicodeDecodeError:
                item.update(kind="file", hex=raw.hex())
        artifacts.append(item)
    return artifacts


def archive_state(processor):
    actual = getattr(processor, "archive", None)
    if actual is None:
        return None
    state = dict(cache_key=actual._cache_key,
                 select_sql=actual._stmt_select,
                 insert_sql=actual._stmt_insert)
    try:
        state["database"] = actual.connection.execute(
            "PRAGMA database_list").fetchone()[2]
        state["connection"] = "open"
    except sqlite3.ProgrammingError:
        state["connection"] = "closed"
    return state


class Context:
    def __init__(self, phase, spec):
        self.phase = phase
        self.spec = spec
        self.trace = []
        self.live = None
        self.processors = {}
        self.jobs = {}

    def processor_name(self, processor):
        if processor not in self.processors:
            self.processors[processor] = f"pp{len(self.processors)}"
        return self.processors[processor]

    def job_name(self, actual):
        if actual not in self.jobs:
            self.jobs[actual] = f"frame{len(self.jobs)}"
        return self.jobs[actual]


def run_case(case):
    global CURRENT
    results = []
    with tempfile.TemporaryDirectory(prefix="gdl-archive-jobs-") as directory:
        work = os.path.realpath(directory)
        root = Path(work)
        old_cwd = os.getcwd()
        os.chdir(work)
        try:
            for relative, content in case.get("preseed", {}).items():
                target = root / relative
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(content, encoding="utf-8")
            for index, spec in enumerate(case["jobs"]):
                config.clear()
                logging.disable(logging.CRITICAL)
                config.set(("output",), "mode", "null")
                category = ("extractor", "fixture")
                config.set(category, "base-directory", work)
                for name in ("metadata-path", "metadata-extractor", "metadata-url"):
                    config.set(category, name, False)
                config.set(category, "part", False)
                if "init" in spec:
                    config.set(category, "init", spec["init"])
                options = copy.deepcopy(spec["postprocessors"])
                config.set(category, "postprocessors", options)
                for name in ("DOWNLOAD", "FILE", "POST", "CHILD"):
                    setattr(job.FLAGS, name, None)
                CURRENT = ctx = Context(spec.get("phase", f"job{index}"), spec)
                original_register = job.DownloadJob.register_hooks

                def register_hooks(actual_job, hooks, hook_options=None):
                    frame = ctx.job_name(actual_job)
                    wrapped = {}
                    for event, callback in hooks.items():
                        processor = getattr(callback, "__self__", None)
                        processor_id = ctx.processor_name(processor)
                        callback_name = callback.__name__
                        snapshot("hook.register", extractor=actual_job.extractor,
                                 data=actual_job.pathfmt.kwdict,
                                 source_shared=actual_job.pathfmt.kwdict is ctx.live,
                                 frame=frame, event=event, processor=processor_id,
                                 callback=callback_name,
                                 archive=archive_state(processor))

                        def run(pathfmt, event=event, callback=callback,
                                callback_name=callback_name, processor=processor,
                                processor_id=processor_id, frame=frame,
                                actual_job=actual_job):
                            def record(phase):
                                snapshot("hook." + phase, extractor=actual_job.extractor,
                                         data=pathfmt.kwdict,
                                         source_shared=pathfmt.kwdict is ctx.live,
                                         frame=frame, event=event,
                                         processor=processor_id, callback=callback_name,
                                         archive=archive_state(processor),
                                         path=dict(filename=pathfmt.filename,
                                                   path=pathfmt.path,
                                                   realpath=pathfmt.realpath))
                            record("before")
                            try:
                                callback(pathfmt)
                            except Exception as error:
                                record(type(error).__name__)
                                raise
                            record("after")
                        wrapped[event] = run
                    return original_register(actual_job, wrapped, hook_options)

                stdout = io.StringIO()
                with contextlib.redirect_stdout(stdout), patch.object(
                        job.DownloadJob, "register_hooks", register_hooks):
                    actual = job.DownloadJob(Source.from_url(
                        "https://fixture.invalid/archive-job"))
                    status = actual.run()
                results.append(dict(
                    phase=ctx.phase, input=copy.deepcopy(spec), status=status,
                    stdout=stdout.getvalue(), trace=ctx.trace,
                    final_keywords=keywords(ctx.live) if ctx.live is not None else {},
                    path_keywords=keywords(actual.pathfmt.kwdict)
                    if actual.pathfmt else None,
                    artifacts=sql_artifacts(root),
                ))
                for connection in archive.CACHE_CONNECTIONS.values():
                    connection.close()
                archive.CACHE_CONNECTIONS.clear()
        finally:
            os.chdir(old_cwd)
        normalized = json.loads(json.dumps(results, ensure_ascii=False).replace(work, "$WORK"))
    return dict(name=case["name"], preseed=case.get("preseed", {}), jobs=normalized)


def metadata(**extra):
    return dict(name="metadata", mode="custom", format="{id}:{num}:{payload}",
                filename="meta-{num}.txt", archive="archive.sqlite3",
                **{"metadata-path": "metadata_path"}, **extra)


def definitions():
    cases = []
    def case(name, processors, **settings):
        cases.append(dict(name=name, jobs=[dict(postprocessors=processors, **settings)]))
    case("default-key-and-prefix", [metadata()])
    case("explicit-format-prefix-table", [metadata(**{
        "archive-format": "entry-{id}-{num}", "archive-prefix": "P:",
        "archive-table": "rows\"quoted"})])
    case("empty-format-prefix-table", [metadata(**{
        "archive-format": "", "archive-prefix": "", "archive-table": ""})])
    case("empty-format-keeps-default-prefix", [metadata(**{"archive-format": ""})])
    case("empty-prefix-keeps-default-format-table", [metadata(**{"archive-prefix": ""})])
    case("empty-table-changes-default-prefix", [metadata(**{"archive-table": ""})])
    case("repeated-key-hit-preserves-metadata-path", [metadata(**{"archive-format": "{id}"})])
    case("existing-metadata-skips-without-add", [metadata(skip=True)])
    cases[-1]["preseed"] = {"meta-1.txt": "existing-one", "meta-2.txt": "existing-two"}
    empty = metadata(); empty.update(format="", empty=False)
    case("empty-custom-adds-without-output", [empty])
    failed = metadata(); failed.update(format="é", encoding="ascii")
    case("ascii-write-failure-does-not-add", [failed])
    case("modify-initializes-archive-but-bypasses-check-add", [dict(
        metadata(), mode="modify", fields={"changed": "modified-{num}"})])
    case("delete-initializes-archive-but-bypasses-check-add", [dict(
        metadata(), mode="delete", fields=["removed"])])
    case("stdout-initializes-archive-but-bypasses-check-add", [dict(metadata(), filename="-")])
    case("two-instances-share-cache-key-close-in-registration-order", [
        metadata(**{"archive-format": "{id}", "archive-prefix": "A:"}),
        dict(metadata(**{"archive-format": "{num}", "archive-prefix": "B:"}), filename="second-{num}.txt"),
    ])
    case("two-instances-reuse-connection-close-is-noop", [
        metadata(**{"archive-format": "{id}", "archive-prefix": "A:", "archive-reuse": True}),
        dict(metadata(**{"archive-format": "{num}", "archive-prefix": "B:", "archive-reuse": True}), filename="second-{num}.txt"),
    ])
    for init in (False, True, "lazy"):
        init_modify = dict(name="metadata", mode="modify", event="init",
                           fields={"id": "7", "num": "9", "scope": "after-init", "init_seen": "yes"})
        archived = metadata(**{"archive-table": "table_{id}_{num}"})
        archived["archive"] = [":basedirectory", "archives", "meta_{id}_{num}.sqlite3"]
        case(f"initialization-context-{str(init).lower()}", [init_modify, archived], init=init)
    case("empty-child-no-archive-item", [metadata(), dict(name="metadata", mode="modify", event="child-after", fields={"child_seen": "yes"})], empty_child=True)
    case("empty-child-eager-archive-initialization", [metadata()], empty_child=True, init=True)
    later_failure = metadata(**{"archive-format": "later-{id}_{num}"}); later_failure.update(format="é", encoding="ascii")
    cases.append(dict(name="later-job-failure-keeps-committed-rows", jobs=[
        dict(phase="first", postprocessors=[metadata()]),
        dict(phase="later", postprocessors=[later_failure]),
    ]))
    return cases


def main(destination):
    sources = ("job.py", "archive.py", "path.py", "postprocessor/common.py",
               "postprocessor/metadata.py", "downloader/text.py", "extractor/common.py")
    package = Path(job.__file__).parent
    result = dict(
        version=version.__version__, generator=GENERATOR,
        source="Actual unmodified gallery_dl.job.DownloadJob.run with MetadataPP and SQLite archives; synthetic offline text: extractor only; hook registration wrapped for observation.",
        source_sha256={name: hashlib.sha256((package / name).read_bytes()).hexdigest() for name in sources},
        generator_source=Path(__file__).read_text(),
        generator_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        cases=[run_case(case) for case in definitions()],
    )
    Path(destination).write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else Path(__file__).with_name("archive_jobs.json"))
