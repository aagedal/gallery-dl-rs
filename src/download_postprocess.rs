//! File hook execution with private input bytes and a stable resume identity.
use super::*;
use crate::postprocess::{self, Hook, Processor};
use serde_json::{Map, Value};

pub(crate) struct Prepared {
    pub media: Media,
    pub keywords: Map<String, Value>,
    pub path: PathBuf,
}
fn logical_path(
    root: &Path,
    media: &Media,
    keywords: &Map<String, Value>,
    options: DownloadOptions<'_>,
    template: Option<&FilenameTemplate>,
) -> Result<PathBuf> {
    let mut path = destination_with_templates(root, media, None, options.directory)?;
    if let Some(template) = template
        && let Some(name) = template.render_keywords(media, keywords)?
    {
        path.set_file_name(name);
    }
    Ok(path)
}
pub(crate) async fn prepare(
    client: &DownloadClient,
    root: &Path,
    media: &Media,
    options: DownloadOptions<'_>,
    template: Option<&FilenameTemplate>,
) -> Result<Prepared> {
    let pp = options.postprocessors.expect("generic processors");
    let prepared = pp.prepared_media(media);
    let mut keywords = postprocess::context(&prepared);
    if let Some(scope) = options.postprocessor_scope {
        scope.inherit_for(&prepared, &mut keywords).await?;
        scope.complete(&keywords);
    }
    let path = logical_path(root, &prepared, &keywords, options, template).or_else(|failure| {
        if pp
            .specifications
            .iter()
            .any(|spec| matches!(&spec.options,Processor::Hash(h) if h.rebuilds_filename()))
        {
            destination_with_templates(root, &prepared, None, options.directory)
        } else {
            Err(failure)
        }
    })?;
    let mut result = Prepared {
        media: prepared,
        keywords,
        path,
    };
    let input = result.path.clone();
    run_hook(
        HookCall {
            client,
            root,
            media,
            options,
            template,
        },
        Hook::Prepare,
        &input,
        &mut result,
    )
    .await?;
    if let Ok(path) = logical_path(root, &result.media, &result.keywords, options, template) {
        result.path = path;
    }
    Ok(result)
}

pub(crate) async fn skip(
    client: &DownloadClient,
    root: &Path,
    media: &Media,
    options: DownloadOptions<'_>,
    template: Option<&FilenameTemplate>,
    prepared: &mut Prepared,
) -> Result<Option<Outcome>> {
    let input = prepared.path.clone();
    run_hook(
        HookCall {
            client,
            root,
            media,
            options,
            template,
        },
        Hook::Skip,
        &input,
        prepared,
    )
    .await?;
    prepared.path = input;
    let mut converted = None;
    if media
        .metadata
        .as_ref()
        .is_some_and(|m| m.ugoira_frame_index.is_some())
        && options
            .postprocessors
            .is_some_and(|pp| !pp.selected(media).is_empty())
        && postprocess::regular_file(&prepared.path)?
        && let Some((path, converted_media)) = complete_original(
            client,
            root,
            media,
            &Outcome::Skipped(prepared.path.clone()),
            options,
            template,
        )
        .await?
    {
        prepared.path = path.clone();
        prepared.media = converted_media;
        converted = Some(Outcome::Downloaded(path));
    }
    if let Some(scope) = options.postprocessor_scope {
        scope.complete_at(&prepared.keywords, &prepared.path);
    }
    Ok(converted)
}
struct HookCall<'a, 'b> {
    client: &'a DownloadClient,
    root: &'a Path,
    media: &'a Media,
    options: DownloadOptions<'b>,
    template: Option<&'a FilenameTemplate>,
}
async fn run_hook(
    call: HookCall<'_, '_>,
    hook: Hook,
    input: &Path,
    prepared: &mut Prepared,
) -> Result<()> {
    let HookCall {
        client,
        root,
        media,
        options,
        template,
    } = call;
    let pp = options.postprocessors.expect("generic processors");
    tokio::time::timeout(client.wait_limit, async {
        for spec in &pp.specifications {
            if !spec.events.contains(&hook) || !spec.matches_keywords(media, &prepared.keywords) {
                continue;
            }
            let result: Result<()> = async {
                match &spec.options {
                    Processor::Ugoira(_) => {}
                    Processor::Metadata(config) => {
                        config
                            .clone()
                            .with_filename_template(template.cloned())
                            .run(
                                &mut prepared.keywords,
                                &prepared.media,
                                &prepared.path,
                                root,
                            )
                            .await?
                    }
                    Processor::Mtime(config) => {
                        if config.is_file() {
                            prepared.keywords.insert(
                                "_mtime_meta".into(),
                                config
                                    .getter(&prepared.keywords)
                                    .map_or(Value::Null, Value::from),
                            );
                        } else {
                            config.apply(
                                &mut prepared.keywords,
                                prepared
                                    .path
                                    .parent()
                                    .ok_or_else(|| error("invalid postprocessor directory"))?,
                            )?;
                        }
                    }
                    Processor::Hash(config) => {
                        if config.run(&mut prepared.keywords, input).await? {
                            prepared.path = logical_path(
                                root,
                                &prepared.media,
                                &prepared.keywords,
                                options,
                                template,
                            )?;
                        }
                    }
                }
                Ok(())
            }
            .await;
            if let Some(scope) = options.postprocessor_scope {
                scope.complete(&prepared.keywords);
            }
            result?;
        }
        Ok(())
    })
    .await
    .map_err(|_| error("postprocessor hook timed out"))?
}
fn publish(input: &Path, output: &Path, replace: bool) -> Result<()> {
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::File::open(input)?.sync_all()?;
    if replace {
        postprocess::regular_file(output)?;
        std::fs::rename(input, output)?;
    } else {
        match std::fs::hard_link(input, output) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                postprocess::regular_file(output)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
pub(crate) async fn download(
    client: &DownloadClient,
    root: &Path,
    media: &Media,
    options: DownloadOptions<'_>,
    template: Option<&FilenameTemplate>,
    mut prepared: Prepared,
) -> Result<Outcome> {
    if postprocess::regular_file(&prepared.path)? {
        let converted = skip(client, root, media, options, template, &mut prepared).await?;
        return Ok(converted.unwrap_or(Outcome::Skipped(prepared.path)));
    }
    let pending = prepared.path.clone();
    run_hook(
        HookCall {
            client,
            root,
            media,
            options,
            template,
        },
        Hook::PrepareAfter,
        &pending,
        &mut prepared,
    )
    .await?;
    let parent = prepared
        .path
        .parent()
        .ok_or_else(|| error("invalid postprocessor destination"))?;
    tokio::fs::create_dir_all(parent).await?;
    let recovery = destination_with_templates(root, media, template, options.directory)
        .or_else(|_| destination_with_templates(root, media, None, options.directory))?;
    if let Some(parent) = recovery.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut source = UgoiraSource {
        directory: tempfile::TempDir::new_in(parent)?,
        source: None,
        recover_at_original: recovery != prepared.path,
        recovery,
    };
    let captured_mtime = std::sync::Arc::new(std::sync::Mutex::new(None));
    let mut source_client = client.clone();
    source_client.response_mtime = Some(captured_mtime.clone());
    let result = download_media(
        &source_client,
        source.directory.path(),
        media,
        DownloadOptions {
            directory: None,
            write_metadata: false,
            ..options
        },
        None,
        Some(&source.recovery),
    )
    .await;
    let downloaded = match result {
        Ok(outcome) => outcome,
        Err(failure) => {
            let path = prepared.path.clone();
            let _ = run_hook(
                HookCall {
                    client,
                    root,
                    media,
                    options,
                    template,
                },
                Hook::Error,
                &path,
                &mut prepared,
            )
            .await;
            return Err(failure);
        }
    };
    let Some(input) = downloaded.saved_path() else {
        return Ok(downloaded);
    };
    source.source = Some(input.to_owned());
    let size = std::fs::metadata(input)?.len();
    prepared.keywords.insert("_filesize".into(), size.into());
    prepared.keywords.insert("_mtime_http".into(), Value::Null);
    if let Some(timestamp) = *captured_mtime
        .lock()
        .map_err(|_| error("HTTP metadata lock poisoned"))?
    {
        let seconds = match timestamp.duration_since(std::time::UNIX_EPOCH) {
            Ok(duration) => i64::try_from(duration.as_secs()).ok(),
            Err(failure) => i64::try_from(failure.duration().as_secs())
                .ok()
                .and_then(i64::checked_neg),
        };
        if let Some(seconds) = seconds {
            prepared
                .keywords
                .insert("_mtime_http".into(), seconds.into());
        }
    }
    let pp = options.postprocessors.expect("generic processors");
    let converters = pp.selected(media);
    let zip = media
        .metadata
        .as_ref()
        .is_some_and(|m| m.frames.is_some() && m.ugoira_frame_index.is_none());
    let mut outputs = Vec::new();
    let mut primary_index = None;
    let mut hook_input = input.to_owned();
    // File hooks see the downloaded representation, including the original
    // ZIP after video conversion. Archive mode can update that representation.
    for (index, spec) in pp.specifications.iter().enumerate() {
        if let Processor::Ugoira(config) = &spec.options {
            if zip
                && converters
                    .iter()
                    .any(|selected| std::ptr::eq(*selected, config))
            {
                let converted = postprocess::with_extension(&prepared.media, config.extension());
                let output = logical_path(
                    root,
                    &converted,
                    &{
                        let mut keywords = prepared.keywords.clone();
                        keywords.insert("extension".into(), config.extension().into());
                        keywords
                    },
                    options,
                    template,
                )
                .or_else(|failure| {
                    if pp.specifications.iter().any(
                        |spec| matches!(&spec.options,Processor::Hash(h) if h.rebuilds_filename()),
                    ) {
                        Ok(prepared.path.with_extension(config.extension()))
                    } else {
                        Err(failure)
                    }
                })?;
                if !config.keep_files() {
                    primary_index = Some(outputs.len());
                }
                if config.skip() && postprocess::regular_file(&output)? {
                    outputs.push((output, None, false));
                    continue;
                }
                let temporary = source
                    .directory
                    .path()
                    .join(format!("converted-{index}.{}", config.extension()));
                config
                    .convert(
                        &postprocess::FrameInput::Zip {
                            path: hook_input.clone(),
                            frames: media.metadata.as_ref().unwrap().frames.clone().unwrap(),
                        },
                        &temporary,
                        &converted,
                        client.wait_limit,
                    )
                    .await?;
                if !postprocess::regular_file(&temporary)? {
                    return Err(error("postprocessor produced no output"));
                }
                if config.mode() == postprocess::ugoira::Mode::Archive
                    && (!config.keep_files()
                        || config.extension() == media.filename.rsplit('.').next().unwrap_or(""))
                {
                    hook_input = temporary.clone();
                }
                outputs.push((output, Some(temporary), !config.skip()));
            }
        } else if spec.events.contains(&Hook::File)
            && spec.matches_keywords(media, &prepared.keywords)
        {
            // Execute exactly one configured hook here, preserving processor order.
            tokio::time::timeout(
                client.wait_limit,
                run_single_file(
                    spec,
                    root,
                    media,
                    template,
                    &hook_input,
                    &mut prepared,
                    options,
                ),
            )
            .await
            .map_err(|_| error("postprocessor hook timed out"))??;
        }
    }
    // Source file-target hooks only record metadata; finalization applies it once.
    if let Some(timestamp) = prepared
        .keywords
        .get("_mtime_meta")
        .and_then(Value::as_i64)
        .filter(|v| *v != 0)
    {
        let config = postprocess::mtime::MtimeOptions::parse(&serde_json::Map::from_iter([(
            "value".into(),
            Value::String(timestamp.to_string()),
        )]))?;
        config.apply(&mut prepared.keywords, &hook_input)?;
    }
    let deletes_zip = zip && converters.iter().any(|config| !config.keep_files());
    if deletes_zip {
        let primary = outputs
            .get_mut(
                primary_index.ok_or_else(|| error("postprocessor produced no primary output"))?,
            )
            .ok_or_else(|| error("postprocessor produced no primary output"))?;
        if primary.1.is_none() && primary.0 != prepared.path {
            publish(&primary.0, &prepared.path, false)?;
        }
        primary.0 = prepared.path.clone();
    } else if !outputs.iter().any(|(path, _, _)| path == &prepared.path) {
        publish(&hook_input, &prepared.path, false)?;
    }
    for (path, temporary, replace) in outputs {
        if let Some(temporary) = temporary {
            publish(&temporary, &path, replace)?;
        }
    }
    if !postprocess::regular_file(&prepared.path)? {
        return Err(error("postprocessor produced no primary output"));
    }
    source.source = None;
    let mut outcome = Outcome::Downloaded(prepared.path.clone());
    if media
        .metadata
        .as_ref()
        .is_some_and(|m| m.ugoira_frame_index.is_some())
        && !converters.is_empty()
        && let Some((path, converted)) =
            complete_original(client, root, media, &outcome, options, template).await?
    {
        prepared.path = path.clone();
        prepared.media = converted;
        let canonical = postprocess::context(&prepared.media);
        for key in ["filename", "name", "extension", "num", "index", "media_id"] {
            if let Some(value) = canonical.get(key) {
                prepared.keywords.insert(key.into(), value.clone());
            }
        }
        outcome = Outcome::Downloaded(path);
    }
    let path = prepared.path.clone();
    run_hook(
        HookCall {
            client,
            root,
            media,
            options,
            template,
        },
        Hook::After,
        &path,
        &mut prepared,
    )
    .await?;
    if options.write_metadata {
        write_metadata(&path, &prepared.media).await?;
    }
    if let Some(scope) = options.postprocessor_scope {
        scope.complete_at(&prepared.keywords, &path);
    }
    Ok(outcome)
}
async fn run_single_file(
    spec: &postprocess::Specification,
    root: &Path,
    media: &Media,
    template: Option<&FilenameTemplate>,
    input: &Path,
    prepared: &mut Prepared,
    options: DownloadOptions<'_>,
) -> Result<()> {
    let result: Result<()> = async {
        match &spec.options {
            Processor::Metadata(config) => {
                config
                    .clone()
                    .with_filename_template(template.cloned())
                    .run(
                        &mut prepared.keywords,
                        &prepared.media,
                        &prepared.path,
                        root,
                    )
                    .await?
            }
            Processor::Mtime(config) => {
                if config.is_file() {
                    prepared.keywords.insert(
                        "_mtime_meta".into(),
                        config
                            .getter(&prepared.keywords)
                            .map_or(Value::Null, Value::from),
                    );
                } else {
                    config.apply(
                        &mut prepared.keywords,
                        prepared
                            .path
                            .parent()
                            .ok_or_else(|| error("invalid postprocessor directory"))?,
                    )?;
                }
            }
            Processor::Hash(config) => {
                if config.run(&mut prepared.keywords, input).await? {
                    prepared.path =
                        logical_path(root, media, &prepared.keywords, options, template)?;
                }
            }
            Processor::Ugoira(_) => {}
        }
        Ok(())
    }
    .await;
    if let Some(scope) = options.postprocessor_scope {
        scope.complete(&prepared.keywords);
    }
    result
}
