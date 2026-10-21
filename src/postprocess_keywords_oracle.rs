//! Pure keyword replay of the installed DownloadJob reference-ownership oracle.
//! Inputs are reconstructed from generator_source, never hook-mutated snapshots.
use super::*;
use crate::postprocess::metadata::MetadataOptions;
use serde_json::json;

fn baseline(id: u64) -> Map<String, Value> {
    if id == 1 {
        json!({"id":1,"num":0,"caption":"original","remove":"original","nested":{"remove":"nested-original"}})
    } else {
        json!({"id":2,"num":0,"caption":"second","remove":"second","nested":{"remove":"second-nested"}})
    }.as_object().unwrap().clone()
}
fn dispatch(context: &mut Map<String, Value>) {
    context.insert("category".into(), json!("fixture"));
    context.insert("subcategory".into(), json!("context"));
}
fn file_fields(context: &mut Map<String, Value>, index: u64) {
    context.insert("num".into(), json!(index));
    context.insert("filename".into(), json!(format!("file-{index}")));
    context.insert("extension".into(), json!("txt"));
}
fn record(
    records: &mut Vec<Value>,
    event: &str,
    hook: Option<(&str, &str)>,
    context: &Map<String, Value>,
) {
    let mut record = json!({"event":event,"keywords":context});
    if let Some((hook, mode)) = hook {
        record["hook"] = hook.into();
        record["mode"] = mode.into();
    }
    records.push(record);
}
struct Mutator {
    hook: String,
    mode: String,
    options: MetadataOptions,
}
fn mutators(case: &Value) -> Vec<Mutator> {
    case["hook_options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|options| Mutator {
            hook: options["event"].as_str().unwrap().into(),
            mode: options["mode"].as_str().unwrap().into(),
            options: MetadataOptions::parse(options.as_object().unwrap(), Path::new("/fixture"))
                .unwrap(),
        })
        .collect()
}
async fn run_hook(
    mutators: &[Mutator],
    event: &str,
    context: &mut Map<String, Value>,
    records: &mut Vec<Value>,
) {
    let media = crate::extract::direct(
        &crate::extract::http_url("https://fixture.invalid/file.jpg").unwrap(),
    )
    .unwrap();
    for mutator in mutators.iter().filter(|m| m.hook == event) {
        record(
            records,
            "hook-before",
            Some((event, &mutator.mode)),
            context,
        );
        mutator
            .options
            .run(
                context,
                &media,
                Path::new("/fixture/file.txt"),
                Path::new("/fixture"),
            )
            .await
            .unwrap();
        record(records, "hook-after", Some((event, &mutator.mode)), context);
    }
}
async fn replay(case: &Value) -> (Vec<Value>, Option<Map<String, Value>>) {
    let mut records = Vec::new();
    let mut current = None;
    let processors = mutators(case);
    if case["eager_init"] == true {
        let mut context = Map::new();
        run_hook(&processors, "init", &mut context, &mut records).await;
        current = Some(context);
    }
    if case["no_results"] != true {
        let mut posts = vec![1];
        if case["empty_tail"] == true {
            posts.push(2);
        }
        for id in posts {
            let raw = baseline(id);
            record(&mut records, "emit-directory", None, &raw);
            let mut context = raw.clone();
            dispatch(&mut context);
            let original = context.clone();
            if let Some(previous) = &mut current {
                run_hook(&processors, "post-after", previous, &mut records).await;
            } else {
                run_hook(&processors, "init", &mut context, &mut records).await;
            }
            run_hook(&processors, "post", &mut context, &mut records).await;
            record(&mut records, "resume-after-directory", None, &context);
            let policy = match case["emitter"].as_str().unwrap() {
                "shared" => KeywordPolicy::shared(&["num", "filename", "extension"]),
                "after-directory" => {
                    KeywordPolicy::copied_after_post(&["num", "filename", "extension"])
                }
                "precomputed" => KeywordPolicy::Independent,
                _ => panic!("unknown source emitter"),
            };
            let shared = Shared::new(original, context, policy, Arc::new(AtomicBool::new(false)));
            if id == 1 && case["empty_only"] != true {
                // Source prepared[n] is deep-copied before Directory, and the
                // other emitters assign exactly these fields on each iteration.
                let leases = [shared.lease().unwrap(), shared.lease().unwrap()];
                for (index, lease) in leases.into_iter().enumerate() {
                    let mut file = raw.clone();
                    file_fields(&mut file, index as u64);
                    if case["emitter"] == "precomputed" {
                        record(&mut records, "emit-file", None, &file);
                        dispatch(&mut file);
                        lease.inherit(&mut file).await;
                    } else {
                        dispatch(&mut file);
                        lease.inherit(&mut file).await;
                        record(&mut records, "emit-file", None, &file);
                    }
                    run_hook(&processors, "file", &mut file, &mut records).await;
                    run_hook(&processors, "after", &mut file, &mut records).await;
                    record(&mut records, "resume-after-file", None, &file);
                    lease.complete(&file);
                    drop(lease);
                }
            }
            let (context, path) = shared.finish().await;
            assert!(path.is_none());
            current = Some(context);
        }
    }
    if let Some(context) = &mut current {
        run_hook(&processors, "post-after", context, &mut records).await;
        run_hook(&processors, "finalize", context, &mut records).await;
    }
    (records, current)
}
#[tokio::test]
async fn all_source_job_context_traces_reconstruct_original_emitter_inputs() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../tests/fixtures/postprocessors/job_context.json"
    ))
    .unwrap();
    assert_eq!(fixture["cases"].as_array().unwrap().len(), 9);
    for case in fixture["cases"].as_array().unwrap() {
        let (records, final_context) = replay(case).await;
        let expected = case["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                let mut record = json!({"event":row["event"],"keywords":row["keywords"]});
                if row.get("hook").is_some() {
                    record["hook"] = row["hook"].clone();
                    record["mode"] = row["mode"].clone();
                }
                record
            })
            .collect::<Vec<_>>();
        assert_eq!(
            records.len(),
            expected.len(),
            "{} record count",
            case["name"]
        );
        for (index, (actual, expected)) in records.iter().zip(&expected).enumerate() {
            assert_eq!(actual, expected, "{} record {index}", case["name"]);
        }
        assert_eq!(
            final_context.map(Value::Object),
            case["final_path_context"].get("keywords").cloned(),
            "{} final context",
            case["name"]
        );
    }
}
