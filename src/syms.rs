use anyhow::Context as _;
pub use camino::Utf8PathBuf as PathBuf;
use clap::Parser;
use rayon::prelude::*;
use reqwest::blocking::Client;
use std::time::Instant;
use tame_gcs::{self as gcs, http, objects::Metadata};

use symbolic_debuginfo::{
    sourcebundle::SourceBundleWriter, Archive, FileFormat, Object, ObjectKind,
};

pub struct ObjectFile {
    pub file: std::fs::File,
    pub path: PathBuf,
    pub map: memmap2::Mmap,
    pub format: FileFormat,
}

pub fn gather_objects(dirs: Vec<PathBuf>) -> Vec<ObjectFile> {
    dirs.into_iter()
        .flat_map(walkdir::WalkDir::new)
        .filter_map(|entry| {
            entry
                .ok()
                .and_then(|entry| entry.file_type().is_file().then_some(entry))
        })
        .par_bridge()
        .filter_map(|entry| {
            let path = PathBuf::from_path_buf(entry.into_path()).ok()?;

            let file = std::fs::File::open(&path).ok()?;
            // SAFETY: It's marked unsafe...
            let map = unsafe { memmap2::Mmap::map(&file).ok()? };

            let format = Archive::peek(&map);
            if format != FileFormat::Unknown {
                Some(ObjectFile {
                    file,
                    map,
                    path,
                    format,
                })
            } else {
                None
            }
        })
        .collect()
}

#[inline]
fn get_unified_id(obj: &Object<'_>) -> anyhow::Result<String> {
    match obj.code_id() {
        Some(code_id) if obj.file_format() != FileFormat::Pe => Ok(code_id.to_string()),
        _ => {
            let debug_id = obj.debug_id();
            anyhow::ensure!(!debug_id.is_nil(), "unable to generate debug identifier");
            Ok(debug_id.breakpad().to_string().to_lowercase())
        }
    }
}

#[inline]
fn create_source_bundle(name: &str, obj: &Object<'_>) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::<u8>::new();
    let writer = SourceBundleWriter::start(std::io::Cursor::new(&mut out))
        .context("failed to create source bundle writer")?;

    writer
        .write_object(obj, name)
        .context("failed to write source bundle")?;

    Ok(out)
}

use std::time::Duration;

struct Ctx {
    client: Client,
    bucket: gcs::BucketName<'static>,
    prefix: gcs::ObjectName<'static>,
    gcs: gcs::objects::Object,
    compression_level: i32,
    bundle_sources: bool,
}

impl Ctx {
    fn upload(&self, metadata: Metadata, content: Vec<u8>) -> anyhow::Result<()> {
        let len = content.len() as u64;

        let req = self.gcs.insert_multipart(
            &self.bucket,
            std::io::Cursor::new(content),
            len,
            &metadata,
            None,
        )?;

        let (req, mut body) = req.into_parts();

        let buf = Vec::with_capacity(len as usize + 1024);
        let mut cursor = std::io::Cursor::new(buf);
        std::io::copy(&mut body, &mut cursor)?;

        let rb = self.client.request(req.method, req.uri.to_string());
        let req = rb
            .headers(req.headers)
            .body(cursor.into_inner())
            .build()
            .context("failed to build request")?;

        let res = {
            let res = self.client.execute(req).context("failed to send request")?;

            let mut builder = http::Response::builder()
                .status(res.status())
                .version(res.version());

            let headers = builder
                .headers_mut()
                .context("failed to convert response headers")?;

            headers.extend(
                res.headers()
                    .into_iter()
                    .map(|(k, v)| (k.clone(), v.clone())),
            );

            let body = res.bytes().context("failed to receive body")?;

            builder.body(body)?
        };

        use gcs::ApiResponse;
        if res.status().is_success() {
            gcs::objects::InsertResponse::try_from_parts(res).context("API request failed")?;
        } else {
            match res
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|hv| hv.to_str().ok())
            {
                Some(ct) if ct.starts_with("text/plain") => {
                    anyhow::bail!(
                        "request failed: HTTP status: {} -> {}",
                        res.status(),
                        std::str::from_utf8(res.body()).unwrap_or("text/plain body was not utf8")
                    );
                }
                _ => {
                    gcs::objects::InsertResponse::try_from_parts(res)
                        .context("API request failed")?;
                }
            }
        }

        Ok(())
    }

    #[inline]
    fn compress(&self, input: &[u8]) -> anyhow::Result<Vec<u8>> {
        zstd::encode_all(input, self.compression_level).context("failed to compress")
    }

    #[inline]
    fn get_gcs_path(&self, obj: &Object<'_>) -> anyhow::Result<(String, PathBuf)> {
        let id = get_unified_id(obj)?;

        #[allow(clippy::wildcard_enum_match_arm)]
        let suffix = match obj.kind() {
            ObjectKind::Debug => "debuginfo",
            ObjectKind::Sources if obj.file_format() == FileFormat::SourceBundle => "sourcebundle",
            ObjectKind::Relocatable | ObjectKind::Library | ObjectKind::Executable => "executable",
            _ => anyhow::bail!("unsupported file"),
        };

        let path = format!("{}/{}/{}/{suffix}", self.prefix, &id[..2], &id[2..]).into();
        Ok((id, path))
    }

    #[inline]
    fn compress_and_upload(&self, obj: &Object<'_>) -> anyhow::Result<ObjectStat> {
        let (id, path) = self.get_gcs_path(obj)?;

        let (compressed_blob, compression_time) = {
            let start = Instant::now();
            let compressed = self.compress(obj.data())?;
            (compressed, start.elapsed())
        };

        let compressed_size = compressed_blob.len() as u64;

        let upload_time = {
            let start = Instant::now();
            let md = Metadata {
                name: Some(path.into_string()),
                content_encoding: Some("zstd".to_owned()),
                content_type: Some("application/octet-stream".to_owned()),
                ..Default::default()
            };

            self.upload(md, compressed_blob)?;
            start.elapsed()
        };

        Ok(ObjectStat {
            id,
            kind: obj.kind(),
            size: obj.data().len() as u64,
            compressed_size,
            compression_time,
            upload_time,
            gather_time: None,
        })
    }
}

pub struct ObjectStat {
    pub id: String,
    pub kind: ObjectKind,
    pub size: u64,
    pub compressed_size: u64,
    pub compression_time: Duration,
    pub upload_time: Duration,
    pub gather_time: Option<Duration>,
}

fn process_archive(
    archive: &Archive<'_>,
    name: &str,
    ctx: &Ctx,
) -> Vec<anyhow::Result<ObjectStat>> {
    let stats: Vec<_> = archive
        .objects()
        .par_bridge()
        .map(|obj| {
            let obj = obj.context("failed to parse object")?;

            let mut obj_stat: Option<anyhow::Result<ObjectStat>> = None;
            let mut sb_stat: Option<anyhow::Result<ObjectStat>> = None;

            rayon::scope(|s| {
                s.spawn(|_s| {
                    obj_stat = Some(ctx.compress_and_upload(&obj));
                });

                // This metadata is not strictly necessary for the symbol server to function
                // from Sentry's standpoint, but it costs us basically nothing to serialize
                // and upload this so that we could trivially import it into a more
                // structured database or the like in the future if we wanted to
                s.spawn(|_s| {
                    let upload_metadata = || -> anyhow::Result<()> {
                        let (_id, mut path) = ctx.get_gcs_path(&obj)?;

                        path.set_file_name("meta");

                        let json = serde_json::json!({
                            "name": name,
                            "arch": obj.arch(),
                            "file_format": obj.file_format(),
                        })
                        .to_string()
                        .into_bytes();

                        let md = Metadata {
                            name: Some(path.into_string()),
                            content_type: Some("application/json".to_owned()),
                            ..Default::default()
                        };

                        ctx.upload(md, json)
                    };

                    let _res = upload_metadata();
                });

                if ctx.bundle_sources && obj.has_debug_info() && !obj.has_sources() {
                    s.spawn(|_s| {
                        let create_and_upload = || {
                            let (sb, gather_time) = {
                                let start = std::time::Instant::now();
                                let sb = create_source_bundle(name, &obj)?;
                                (sb, start.elapsed())
                            };

                            let sb_obj = Object::parse(&sb)?;

                            anyhow::ensure!(
                                sb_obj.file_format() == FileFormat::SourceBundle,
                                "expected SourceBundle but found {}",
                                sb_obj.file_format()
                            );

                            let mut sb_stat = ctx.compress_and_upload(&sb_obj)?;
                            sb_stat.gather_time = Some(gather_time);

                            Ok(sb_stat)
                        };

                        sb_stat = Some(create_and_upload());
                    });
                }
            });

            let mut v = Vec::with_capacity(2);
            v.extend(obj_stat);
            v.extend(sb_stat);

            Ok(v)
        })
        .collect();

    stats
        .into_iter()
        .flat_map(|res| match res {
            Ok(v) => v,
            Err(err) => vec![Err(err)],
        })
        .collect()
}

pub struct FileStat {
    pub path: PathBuf,
    pub format: FileFormat,
    pub objects: anyhow::Result<Vec<anyhow::Result<ObjectStat>>>,
}

pub fn upload(
    client: Client,
    bucket: String,
    mut path: String,
    compression_level: i32,
    bundle_sources: bool,
    objects: Vec<ObjectFile>,
) -> anyhow::Result<Vec<FileStat>> {
    while path.ends_with('/') {
        path.pop();
    }

    let bucket: gcs::BucketName<'static> = bucket.try_into().context("invalid gcs bucket name")?;
    let prefix: gcs::ObjectName<'static> = path.try_into().context("invalid gcs path")?;

    let ctx = Ctx {
        client,
        bucket,
        prefix,
        compression_level,
        bundle_sources,
        gcs: gcs::objects::Object::default(),
    };

    Ok(objects
        .into_par_iter()
        .map(|file| {
            let process = || -> anyhow::Result<Vec<anyhow::Result<ObjectStat>>> {
                let archive = Archive::parse(&file.map)
                    .with_context(|| format!("failed to parse {}", file.path))?;

                Ok(process_archive(
                    &archive,
                    file.path.file_stem().context("no file stem for path")?,
                    &ctx,
                ))
            };

            let objects = process();

            FileStat {
                path: file.path,
                format: file.format,
                objects,
            }
        })
        .collect())
}

fn level_in_range(s: &str) -> Result<i32, String> {
    let range = zstd::compression_level_range();

    let level: i32 = s
        .parse()
        .map_err(|err| format!("`{s}` isn't a valid integer {err}"))?;
    if range.contains(&level) {
        Ok(level)
    } else {
        Err(format!(
            "Compression level not in range {}-{}",
            range.start(),
            range.end()
        ))
    }
}

/// Uploads debug symbols to GCS
#[derive(Parser)]
pub struct Args {
    /// GCS bucket to upload symbols to
    #[arg(long, env = "SYMS_BUCKET")]
    bucket: String,
    #[arg(long, env = "SYMS_PATH")]
    path: String,
    /// Creates source bindles and includes them in the upload
    #[arg(long)]
    bundle_sources: bool,
    /// The ZSTD compression level to use when compressing objects before upload
    #[arg(long, short, default_value = "5", value_parser = level_in_range)]
    compression_level: i32,
    /// If set, _any_ failure to parse or upload symbols will cause the command
    /// to fail, even if some succeeded
    #[arg(long)]
    strict: bool,
    /// Directories to find symbols in
    dirs: Vec<PathBuf>,
}

impl crate::Scopes for Args {
    fn scopes(&self) -> &'static [&'static str] {
        &["https://www.googleapis.com/auth/devstorage.full_control"]
    }
}

pub async fn run(args: Args, client: reqwest::blocking::Client) -> anyhow::Result<()> {
    let objects = gather_objects(args.dirs);
    anyhow::ensure!(
        !objects.is_empty(),
        "no valid objects were found in the specified directories"
    );

    let stats = upload(
        client,
        args.bucket,
        args.path,
        args.compression_level,
        args.bundle_sources,
        objects,
    )?;

    use nu_ansi_term::{Color, Style};

    let mut failures = 0;
    let mut successes = 0;

    for fstat in stats {
        match fstat.objects {
            Ok(ostats) => {
                println!(
                    "{} {} {}",
                    Color::Green.paint("OK"),
                    Style::default()
                        .dimmed()
                        .paint(fstat.path.file_name().unwrap_or_default()),
                    Style::default().dimmed().paint(fstat.format.to_string()),
                );

                for ostat in ostats {
                    match ostat {
                        Ok(ostat) => {
                            fn bytes_to_human(bytes: u64) -> String {
                                let mut bytes = bytes as f64;

                                for unit in ["B", "KB", "MB", "GB", "TB"] {
                                    if bytes > 1024.0 {
                                        bytes /= 1024.0;
                                    } else {
                                        return format!("{bytes:.1}{unit}");
                                    }
                                }

                                unreachable!("if we have more than a TB something is wrong");
                            }

                            println!(
                                "  {} {} {}",
                                Color::Green.paint("OK"),
                                Style::default().dimmed().paint(ostat.id),
                                Style::default().dimmed().paint(ostat.kind.to_string()),
                            );
                            if let Some(gt) = ostat.gather_time {
                                println!("    source gather: {:?}", gt);
                            }
                            println!(
                                "    compression: {} -> {} {}% ({:?})\n    upload: {:?}",
                                Style::default().dimmed().paint(bytes_to_human(ostat.size)),
                                Style::default()
                                    .dimmed()
                                    .paint(bytes_to_human(ostat.compressed_size)),
                                Style::default().bold().paint(
                                    ((ostat.compressed_size as f64 / ostat.size as f64 * 100f64)
                                        as u32)
                                        .to_string()
                                ),
                                ostat.compression_time,
                                ostat.upload_time
                            );

                            successes += 1;
                        }
                        Err(err) => {
                            println!("  {} {err:#}", Color::Red.paint("ERR"));
                            failures += 1;
                        }
                    }
                }
            }
            Err(err) => {
                println!(
                    "{} {} {}\n  {}",
                    Color::Red.paint("ERR"),
                    Style::default()
                        .dimmed()
                        .paint(fstat.path.file_name().unwrap_or_default()),
                    Style::default().dimmed().paint(fstat.format.to_string()),
                    Color::Red.paint(err.to_string()),
                );
                failures += 1;
            }
        }
    }

    if failures > 0 && args.strict {
        anyhow::bail!("detected {failures} failures");
    }

    if successes == 0 {
        anyhow::bail!("no debug objects were successfuly parsed and uploaded");
    }

    Ok(())
}
