"""Record unmodified gallery-dl's shared queue state after a hook write fails."""
import copy
import hashlib
import json
import logging
import re
import sys
import tempfile
from pathlib import Path
from unittest.mock import patch

from gallery_dl import config, job, version
from gallery_dl.extractor.common import Extractor, Message


class Empty(Extractor):
    category = "fixture-child"
    subcategory = "empty"

    @classmethod
    def from_url(cls, url):
        return cls(re.match(".*", url))

    def initialize(self):
        pass

    def items(self):
        return iter(())

    def cookies_store(self):
        pass


class Parent(Empty):
    category = "fixture"
    subcategory = "failure"
    directory_fmt = ()
    filename_fmt = "{id}.{extension}"
    archive_fmt = "{id}"

    def items(self):
        data = dict(id=42, num=0, seed=42, suffix="original")
        yield Message.Directory, "", data
        data["num"] += 1
        data["_extractor"] = Empty
        yield Message.Queue, "https://fixture.invalid/child", data


def main(destination):
    config.clear()
    logging.disable(logging.CRITICAL)
    records = []
    options = [
        dict(name="metadata", mode="modify", event="child-after",
             fields=dict(kept="partial", num="{seed}", suffix="changed")),
        dict(name="metadata", mode="custom", event="child-after",
             format="é", encoding="ascii", filename="failed.txt",
             **{"metadata-path": "attempt"}),
    ]
    for event in ("post-after", "finalize-error", "finalize"):
        options.append(dict(name="metadata", mode="modify", event=event,
                            fields={event: "observed"}))
    register = job.DownloadJob.register_hooks

    def register_hooks(self, hooks, options=None):
        wrapped = {}
        for event, callback in hooks.items():
            def run(path, event=event, callback=callback):
                def record(phase):
                    data = {key: copy.deepcopy(path.kwdict[key]) for key in
                            ("num", "suffix", "kept", "attempt", "post-after",
                             "finalize-error", "finalize") if key in path.kwdict}
                    records.append(dict(event=event, phase=phase, keywords=data))
                record("before")
                try:
                    callback(path)
                except Exception as error:
                    record(type(error).__name__)
                    raise
                record("after")
            wrapped[event] = run
        return register(self, wrapped, options)

    with tempfile.TemporaryDirectory() as work:
        config.set(("extractor", "fixture"), "base-directory", work)
        for name in ("metadata-path", "metadata-extractor", "metadata-url"):
            config.set(("extractor", "fixture"), name, False)
        config.set(("extractor", "fixture"), "postprocessors", options)
        with patch.object(job.DownloadJob, "register_hooks", register_hooks):
            actual = job.DownloadJob(Parent.from_url("https://fixture.invalid/parent"))
            status = actual.run()
        result = dict(version=version.__version__, status=status, options=options,
                      records=records, generator_source=Path(__file__).read_text(),
                      source_sha256={name: hashlib.sha256(
                          (Path(job.__file__).parent / name).read_bytes()).hexdigest()
                          for name in ("job.py", "postprocessor/metadata.py")})
        result = json.loads(json.dumps(result).replace(work, "$WORK"))
    Path(destination).write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")


if __name__ == "__main__":
    main(sys.argv[1])
