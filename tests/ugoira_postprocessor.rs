use gallery_dl_rs::{
    extract::{Media, MediaJob, PostMetadata},
    pixiv::UgoiraFrame,
    postprocess::{FrameInput, SourceFrame, UgoiraOptions},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};
fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/pixiv/ugoira_postprocessor.json")).unwrap()
}
fn frames(value: &Value) -> Vec<UgoiraFrame> {
    serde_json::from_value(value.clone()).unwrap()
}
fn options(value: &Value, base: &Path) -> UgoiraOptions {
    UgoiraOptions::parse(value.as_object().unwrap(), base).unwrap()
}
fn media() -> Media {
    Media {
        url: url::Url::parse("https://i.pximg.net/fixture.zip").unwrap(),
        filename: "42_p0.zip".into(),
        category: "pixiv".into(),
        metadata: Some(PostMetadata {
            post_id: "42".into(),
            media_id: "42_p0".into(),
            index: 1,
            timestamp: Some(1_704_164_646),
            ..Default::default()
        }),
        job: MediaJob::DirectHttp,
        headers: Default::default(),
        handoff: Default::default(),
    }
}
fn bytes(hex: &str) -> Vec<u8> {
    hex.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn input_files(root: &Path, source: &[UgoiraFrame]) -> FrameInput {
    FrameInput::Files(
        source
            .iter()
            .map(|frame| {
                let path = root.join(&frame.file);
                fs::write(&path, format!("fixture {}", frame.file)).unwrap();
                // Windows needs write access to change file times.
                fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_times(
                        fs::FileTimes::new()
                            .set_modified(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
                    )
                    .unwrap();
                SourceFrame {
                    frame: frame.clone(),
                    path,
                    extension: "jpg".into(),
                }
            })
            .collect(),
    )
}
#[test]
fn source_normalized_options_and_timing_vectors_match_public_options() {
    let source = fixture();
    for case in source["option_cases"].as_array().unwrap() {
        // Detection was injected in the Python oracle. Explicit modes/locations are portable here.
        if case["mkvmerge_detected"] == true && case["options"].get("mkvmerge-location").is_none() {
            continue;
        }
        let native = options(&case["options"], Path::new("."));
        let expected = &case["normalized"];
        assert_eq!(
            native.extension(),
            expected["extension"].as_str().unwrap(),
            "{}",
            case["name"]
        );
        assert_eq!(native.keep_files(), !expected["delete"].as_bool().unwrap());
        assert_eq!(native.skip(), expected["skip"].as_bool().unwrap());
        assert_eq!(
            native.repeat_last_frame(),
            expected["repeat"].as_bool().unwrap_or(false)
        );
        assert_eq!(native.mtime(), expected["mtime"].as_bool().unwrap_or(false));
        assert_eq!(native.two_pass(), expected["twopass"].as_bool().unwrap());
        assert_eq!(
            native.prevent_odd(),
            expected["prevent_odd"].as_bool().unwrap()
        );
        let metadata = match &expected["metadata"] {
            Value::String(name) => Some(name.as_str()),
            Value::Bool(true) => Some("animation.json"),
            _ => None,
        };
        assert_eq!(native.metadata_name(), metadata, "{}", case["name"]);
    }
    for case in source["rate_cases"].as_array().unwrap() {
        let source = frames(&case["frames"]);
        let actual = options(&case["options"], Path::new(".")).rates(&source);
        if !case["error"].is_null() {
            assert!(actual.is_err());
        } else {
            let expected = &case["rate"];
            let actual = actual.unwrap();
            assert_eq!(
                actual.0.as_deref(),
                expected[0].as_str(),
                "{}",
                case["name"]
            );
            let output = match &expected[1] {
                Value::Null => None,
                Value::String(value) => Some(value.clone()),
                value => Some(value.to_string()),
            };
            assert_eq!(actual.1, output, "{}", case["name"]);
        }
    }
}
#[test]
fn source_command_concat_timecodes_and_image2_mutation_match_native_plan() {
    for case in fixture()["command_cases"].as_array().unwrap() {
        if case["name"] == "existing_output_skips" {
            continue;
        }
        let mut config = case["options"].clone();
        // This oracle injected absent mkvmerge detection; keep planning portable.
        if config.get("mode").is_none() {
            config["mode"] = "concat".into();
        }
        let native = options(&config, Path::new("."));
        let extension = native.extension();
        let output = PathBuf::from(format!("$WORK/out/42_p0.{extension}"));
        let plan = native
            .plan(
                &frames(&case["frames"]),
                Path::new("$WORK/frames"),
                &output,
                Some(1_704_164_646),
            )
            .unwrap();
        let commands = plan
            .commands
            .iter()
            .map(|command| {
                command
                    .iter()
                    .map(|arg| {
                        // Joined paths and the null device are native, as in
                        // gallery-dl; the oracle was recorded on POSIX.
                        let arg = arg.to_str().unwrap();
                        if !cfg!(windows) {
                            arg.to_owned()
                        } else if arg == "NUL" {
                            "/dev/null".to_owned()
                        } else if arg.contains("$WORK") {
                            arg.replace('\\', "/")
                        } else {
                            arg.to_owned()
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let expected = case["calls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|call| serde_json::from_value::<Vec<String>>(call["args"].clone()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(commands, expected, "{}", case["name"]);
        assert_eq!(
            plan.concat.as_deref(),
            case["text_files"]["ffconcat.txt"].as_str(),
            "{}",
            case["name"]
        );
        assert_eq!(
            plan.timecodes.as_deref(),
            case["text_files"]["timecodes.tc"].as_str()
        );
        assert_eq!(
            serde_json::to_value(&plan.frames).unwrap(),
            case["mutated_frames"]
        );
        let actual = plan
            .mtimes
            .iter()
            .map(|(file, millis)| (format!("$WORK/frames/{file}"), millis * 1_000_000))
            .collect::<Vec<_>>();
        let expected = case["utimes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|call| {
                (
                    call["path"].as_str().unwrap().to_owned(),
                    call["ns"][0].as_u64().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "{}", case["name"]);
    }
}
#[tokio::test]
async fn archive_entries_metadata_bytes_and_timestamps_match_actual_source() {
    use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter, write::SimpleFileOptions};
    for case in fixture()["archive_cases"].as_array().unwrap() {
        let temp = tempfile::tempdir().unwrap();
        let source = frames(&case["frames"]);
        let input = if case["zip_source"] == true {
            let path = temp.path().join("source.zip");
            let mut zip = ZipWriter::new(fs::File::create(&path).unwrap());
            for frame in &source {
                zip.start_file(
                    &frame.file,
                    SimpleFileOptions::default()
                        .compression_method(CompressionMethod::Stored)
                        .last_modified_time(
                            DateTime::from_date_and_time(2024, 1, 2, 3, 4, 6).unwrap(),
                        ),
                )
                .unwrap();
                zip.write_all(format!("fixture {}", frame.file).as_bytes())
                    .unwrap();
            }
            zip.finish().unwrap();
            FrameInput::Zip {
                path,
                frames: Arc::from(source),
            }
        } else {
            input_files(temp.path(), &source)
        };
        let native = options(&case["options"], temp.path());
        let output = temp.path().join(format!("result.{}", native.extension()));
        native
            .convert(&input, &output, &media(), Duration::from_secs(5))
            .await
            .unwrap();
        let mut zip = ZipArchive::new(fs::File::open(&output).unwrap()).unwrap();
        let expected = case["entries"].as_array().unwrap();
        assert_eq!(zip.len(), expected.len(), "{}", case["name"]);
        for (index, expected) in expected.iter().enumerate() {
            let mut file = zip.by_index(index).unwrap();
            assert_eq!(file.name().unwrap(), expected["name"].as_str().unwrap());
            let date = file.last_modified().unwrap();
            assert_eq!(
                json!([
                    date.year(),
                    date.month(),
                    date.day(),
                    date.hour(),
                    date.minute(),
                    date.second()
                ]),
                expected["date_time"],
                "{}",
                case["name"]
            );
            assert_eq!(file.compression(), CompressionMethod::Stored);
            let mut data = Vec::new();
            file.read_to_end(&mut data).unwrap();
            assert_eq!(
                data,
                bytes(expected["data_hex"].as_str().unwrap()),
                "{}",
                case["name"]
            );
        }
    }
}

#[cfg(unix)]
fn executable_stub(path: &Path, log: &Path, retcode: i64) {
    use std::os::unix::fs::PermissionsExt;
    let script = format!(
        r#"#!/usr/bin/env python3
import json, os, pathlib, sys
args=sys.argv[1:]
record={{'argv':sys.argv,'text_files':{{}},'mtimes':{{}}}}
if '-i' in args:
    source=args[args.index('-i')+1]
    directory=pathlib.Path(source).parent
    for name in ('ffconcat.txt','timecodes.tc'):
        target=directory/name
        if target.exists():record['text_files'][name]=target.read_text()
    for frame in directory.glob('*.jpg'):record['mtimes'][frame.name]=frame.stat().st_mtime_ns
with open({log},'a') as fp:fp.write(json.dumps(record)+'\n')
if {retcode}:sys.stderr.write('PRIVATE_CONVERTER_DIAGNOSTIC\n');sys.exit({retcode})
target=args[args.index('-o')+1] if '-o' in args else args[-1]
if target not in ('/dev/null','NUL'):
    pathlib.Path(target).parent.mkdir(parents=True,exist_ok=True)
    pathlib.Path(target).write_bytes(b'synthetic converted animation')
"#,
        log = serde_json::to_string(&log.to_string_lossy()).unwrap()
    );
    fs::write(path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}
#[cfg(unix)]
#[tokio::test]
async fn executable_stubs_receive_source_commands_and_preserve_inputs_on_failure() {
    for case in fixture()["command_cases"].as_array().unwrap() {
        if case["name"] == "existing_output_skips" {
            continue;
        }
        let temp = tempfile::tempdir().unwrap();
        let input_dir = temp.path().join("input");
        fs::create_dir(&input_dir).unwrap();
        let source = frames(&case["frames"]);
        let input = input_files(&input_dir, &source);
        let log = temp.path().join("calls.jsonl");
        let ffmpeg = temp.path().join("ffmpeg-stub");
        let mkvmerge = temp.path().join("mkvmerge-stub");
        let retcode = case["retcode"].as_i64().unwrap();
        executable_stub(&ffmpeg, &log, retcode);
        executable_stub(&mkvmerge, &log, retcode);
        let mut config = case["options"].clone();
        if config.get("mode").is_none() {
            config["mode"] = "concat".into();
        }
        config["ffmpeg-location"] = ffmpeg.to_string_lossy().into_owned().into();
        config["mkvmerge-location"] = mkvmerge.to_string_lossy().into_owned().into();
        let native = options(&config, temp.path());
        let output = temp.path().join(format!("result.{}", native.extension()));
        let result = native
            .convert(&input, &output, &media(), Duration::from_secs(10))
            .await;
        if retcode == 0 {
            result.unwrap();
            assert!(fs::metadata(&output).unwrap().len() > 0);
        } else {
            let error = result.unwrap_err().to_string();
            assert!(!error.contains("PRIVATE_CONVERTER_DIAGNOSTIC"));
            assert!(!output.exists());
        }
        for frame in &source {
            assert!(input_dir.join(&frame.file).is_file());
        }
        let calls = fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        let expected = case["calls"].as_array().unwrap();
        assert_eq!(calls.len(), expected.len(), "{}", case["name"]);
        let first = calls[0]["argv"].as_array().unwrap();
        let index = first.iter().position(|arg| arg == "-i").unwrap();
        let private_dir = Path::new(first[index + 1].as_str().unwrap())
            .parent()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        for (actual, expected) in calls.iter().zip(expected) {
            let args = actual["argv"]
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
                .map(|(index, arg)| {
                    let arg = arg.as_str().unwrap();
                    if index == 0 {
                        if arg == ffmpeg.to_str().unwrap() {
                            "ffmpeg".to_owned()
                        } else {
                            "mkvmerge".to_owned()
                        }
                    } else {
                        arg.replace(
                            output.to_str().unwrap(),
                            &format!("$WORK/out/42_p0.{}", native.extension()),
                        )
                        .replace(&private_dir, "$WORK/frames")
                    }
                })
                .collect::<Vec<_>>();
            assert_eq!(
                serde_json::to_value(args).unwrap(),
                expected["args"],
                "{}",
                case["name"]
            );
        }
        assert_eq!(
            calls[0]["text_files"], case["text_files"],
            "{}",
            case["name"]
        );
        for time in case["utimes"].as_array().unwrap() {
            let name = time["path"].as_str().unwrap().rsplit('/').next().unwrap();
            assert_eq!(calls[0]["mtimes"][name], time["ns"][0], "{}", case["name"]);
        }
    }
}
#[tokio::test]
async fn real_ffmpeg_converts_tiny_animation_with_source_timing_and_preserves_frames() {
    let ffmpeg = Path::new("/opt/homebrew/bin/ffmpeg");
    if !ffmpeg.is_file() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let red = bytes(
        "89504e470d0a1a0a0000000d4948445200000004000000040802000000269309290000001049444154789c63f8cfc000470cc47100ae930ff1d05f239e0000000049454e44ae426082",
    );
    let blue = bytes(
        "89504e470d0a1a0a0000000d4948445200000004000000040802000000269309290000001049444154789c636060f88f8488e200008eb30ff1b3a32f330000000049454e44ae426082",
    );
    let mut source = Vec::new();
    for (index, data) in [red, blue].into_iter().enumerate() {
        let path = temp.path().join(format!("input{index}.png"));
        fs::write(&path, data).unwrap();
        source.push(SourceFrame {
            frame: UgoiraFrame {
                file: format!("{index:06}.jpg"),
                delay: if index == 0 { 50 } else { 125 },
            },
            path,
            extension: "png".into(),
        });
    }
    let options = options(
        &json!({"mode":"concat","ffmpeg-location":ffmpeg,"ffmpeg-args":["-c:v","libvpx-vp9","-pix_fmt","yuv420p","-y"]}),
        temp.path(),
    );
    let output = temp.path().join("tiny.webm");
    options
        .convert(
            &FrameInput::Files(source.clone()),
            &output,
            &media(),
            Duration::from_secs(20),
        )
        .await
        .unwrap();
    let content = fs::read(&output).unwrap();
    assert!(content.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]));
    assert!(source.iter().all(|frame| frame.path.exists()));
    let modified = fs::metadata(&output).unwrap().modified().unwrap();
    assert_eq!(
        modified.duration_since(UNIX_EPOCH).unwrap().as_secs(),
        1_704_164_646
    );
    let probe = Path::new("/opt/homebrew/bin/ffprobe");
    if probe.is_file() {
        let result = std::process::Command::new(probe)
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=width,height,codec_name",
                "-of",
                "json",
            ])
            .arg(&output)
            .output()
            .unwrap();
        assert!(result.status.success());
        let data: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(data["streams"][0]["width"], 4);
        assert_eq!(data["streams"][0]["height"], 4);
        assert_eq!(data["streams"][0]["codec_name"], "vp9");
    }
}
